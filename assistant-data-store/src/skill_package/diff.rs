//! Bounded line diff between two package versions. It is the single diff used
//! by online editing previews and by model-authored proposals.

use super::SkillPackage;
use desk_agent_protocol::ai_assistant::skill::{
    SkillDiffHunk, SkillDiffLine, SkillDiffOp, SkillFileChange, SkillFileDiff,
};

const CONTEXT: usize = 2;
/// Larger changed regions are shown as a whole replacement instead of an LCS.
const MAX_LCS_CELLS: usize = 1_000_000;

#[derive(Clone, Copy)]
enum Edit {
    Same(usize),
    Remove(usize),
    Add(usize),
}

fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n')
        .map(|line| line.trim_end_matches('\n').trim_end_matches('\r'))
        .collect()
}

fn script(old: &[&str], new: &[&str]) -> Vec<Edit> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (a, b) = (
        &old[prefix..old.len() - suffix],
        &new[prefix..new.len() - suffix],
    );
    let mut edits: Vec<Edit> = (0..prefix).map(Edit::Same).collect();
    if a.len().saturating_mul(b.len()) <= MAX_LCS_CELLS && a.len() < u16::MAX as usize {
        // table[i][j] = LCS length of a[i..] and b[j..].
        let width = b.len() + 1;
        let mut table = vec![0u16; (a.len() + 1) * width];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                table[i * width + j] = if a[i] == b[j] {
                    table[(i + 1) * width + j + 1] + 1
                } else {
                    table[(i + 1) * width + j].max(table[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < a.len() || j < b.len() {
            if i < a.len() && j < b.len() && a[i] == b[j] {
                edits.push(Edit::Same(prefix + i));
                i += 1;
                j += 1;
            } else if i < a.len()
                && (j == b.len() || table[(i + 1) * width + j] >= table[i * width + j + 1])
            {
                edits.push(Edit::Remove(prefix + i));
                i += 1;
            } else {
                edits.push(Edit::Add(prefix + j));
                j += 1;
            }
        }
    } else {
        edits.extend((0..a.len()).map(|i| Edit::Remove(prefix + i)));
        edits.extend((0..b.len()).map(|j| Edit::Add(prefix + j)));
    }
    edits.extend((0..suffix).map(|k| Edit::Same(old.len() - suffix + k)));
    edits
}

struct Budget {
    remaining: usize,
}

impl Budget {
    fn take(&mut self, text: &str) -> bool {
        match self.remaining.checked_sub(text.len() + 8) {
            Some(rest) => {
                self.remaining = rest;
                true
            }
            None => false,
        }
    }
}

fn file_diff(
    path: &str,
    old: Option<&str>,
    new: Option<&str>,
    budget: &mut Budget,
) -> Option<SkillFileDiff> {
    let change = match (old, new) {
        (Some(a), Some(b)) if a == b => return None,
        (None, None) => return None,
        (None, Some(_)) => SkillFileChange::Added,
        (Some(_), None) => SkillFileChange::Removed,
        (Some(_), Some(_)) => SkillFileChange::Modified,
    };
    let (old_lines, new_lines) = (lines(old.unwrap_or("")), lines(new.unwrap_or("")));
    let edits = script(&old_lines, &new_lines);
    let changed: Vec<usize> = edits
        .iter()
        .enumerate()
        .filter(|(_, edit)| !matches!(edit, Edit::Same(..)))
        .map(|(index, _)| index)
        .collect();
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for index in changed {
        let start = index.saturating_sub(CONTEXT);
        let end = (index + CONTEXT + 1).min(edits.len());
        match groups.last_mut() {
            Some(last) if start <= last.1 => last.1 = end,
            _ => groups.push((start, end)),
        }
    }
    let mut result = SkillFileDiff {
        path: path.into(),
        change,
        hunks: Vec::new(),
        truncated: false,
    };
    // Position of each edit in the old and new texts, 0-based.
    let mut positions = Vec::with_capacity(edits.len());
    let (mut o, mut n) = (0usize, 0usize);
    for edit in &edits {
        positions.push((o, n));
        match edit {
            Edit::Same(..) => {
                o += 1;
                n += 1;
            }
            Edit::Remove(_) => o += 1,
            Edit::Add(_) => n += 1,
        }
    }
    'groups: for (start, end) in groups {
        let (old_pos, new_pos) = positions[start];
        let mut hunk = SkillDiffHunk {
            old_start: if old.is_some() { old_pos as u32 + 1 } else { 0 },
            new_start: if new.is_some() { new_pos as u32 + 1 } else { 0 },
            lines: Vec::new(),
        };
        for edit in &edits[start..end] {
            let (op, text) = match *edit {
                Edit::Same(i) => (SkillDiffOp::Context, old_lines[i]),
                Edit::Remove(i) => (SkillDiffOp::Remove, old_lines[i]),
                Edit::Add(j) => (SkillDiffOp::Add, new_lines[j]),
            };
            if !budget.take(text) {
                result.truncated = true;
                if !hunk.lines.is_empty() {
                    result.hunks.push(hunk);
                }
                break 'groups;
            }
            hunk.lines.push(SkillDiffLine {
                op,
                text: text.into(),
            });
        }
        result.hunks.push(hunk);
    }
    Some(result)
}

/// Changes from `old` to `new`, `SKILL.md` first, within `max_bytes` of line text.
pub fn diff_packages(
    old: Option<&SkillPackage>,
    new: &SkillPackage,
    max_bytes: usize,
) -> Vec<SkillFileDiff> {
    let mut budget = Budget {
        remaining: max_bytes,
    };
    let mut out = Vec::new();
    let old_md = old.map(SkillPackage::render_skill_md);
    out.extend(file_diff(
        super::SKILL_FILE,
        old_md.as_deref(),
        Some(&new.render_skill_md()),
        &mut budget,
    ));
    let mut paths: Vec<&str> = new.resources.iter().map(|r| r.path.as_str()).collect();
    if let Some(old) = old {
        paths.extend(old.resources.iter().map(|r| r.path.as_str()));
    }
    paths.sort_unstable();
    paths.dedup();
    fn text<'a>(package: Option<&'a SkillPackage>, path: &str) -> Option<&'a str> {
        package.and_then(|p| {
            p.resources
                .iter()
                .find(|r| r.path == path)
                .map(|r| r.text.as_str())
        })
    }
    for path in paths {
        out.extend(file_diff(
            path,
            text(old, path),
            text(Some(new), path),
            &mut budget,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(diff: &SkillFileDiff) -> Vec<String> {
        diff.hunks
            .iter()
            .flat_map(|h| {
                h.lines.iter().map(|l| {
                    let sign = match l.op {
                        SkillDiffOp::Context => ' ',
                        SkillDiffOp::Add => '+',
                        SkillDiffOp::Remove => '-',
                    };
                    format!("{sign}{}", l.text)
                })
            })
            .collect()
    }

    #[test]
    fn modified_lines_are_shown_with_context_and_line_numbers() {
        let old = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "a\nb\nc\nD\ne\nf\ng\n";
        let mut budget = Budget { remaining: 10_000 };
        let diff = file_diff("x", Some(old), Some(new), &mut budget).unwrap();
        assert_eq!(diff.change, SkillFileChange::Modified);
        assert_eq!(diff.hunks.len(), 1);
        assert_eq!(diff.hunks[0].old_start, 2);
        assert_eq!(diff.hunks[0].new_start, 2);
        assert_eq!(render(&diff), [" b", " c", "-d", "+D", " e", " f"]);
    }

    #[test]
    fn distant_changes_form_separate_hunks_and_unchanged_files_are_omitted() {
        let old: String = (0..20).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l1\n", "x\n").replace("l18\n", "y\n");
        let mut budget = Budget { remaining: 10_000 };
        let diff = file_diff("x", Some(&old), Some(&new), &mut budget).unwrap();
        assert_eq!(diff.hunks.len(), 2);
        assert!(file_diff("x", Some(&old), Some(&old), &mut budget).is_none());
    }

    #[test]
    fn added_and_removed_files_use_zero_for_the_missing_side() {
        let mut budget = Budget { remaining: 10_000 };
        let added = file_diff("x", None, Some("a\nb"), &mut budget).unwrap();
        assert_eq!(added.change, SkillFileChange::Added);
        assert_eq!((added.hunks[0].old_start, added.hunks[0].new_start), (0, 1));
        assert_eq!(render(&added), ["+a", "+b"]);
        let removed = file_diff("x", Some("a"), None, &mut budget).unwrap();
        assert_eq!(
            (removed.hunks[0].old_start, removed.hunks[0].new_start),
            (1, 0)
        );
    }

    #[test]
    fn output_is_cut_at_the_byte_budget() {
        let new: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let mut budget = Budget { remaining: 300 };
        let diff = file_diff("x", None, Some(&new), &mut budget).unwrap();
        assert!(diff.truncated);
        assert!(diff.hunks[0].lines.len() < 200);
    }

    #[test]
    fn large_rewrites_fall_back_to_replacement() {
        let old: String = (0..1500).map(|i| format!("a{i}\n")).collect();
        let new: String = (0..1500).map(|i| format!("b{i}\n")).collect();
        let edits = script(&lines(&old), &lines(&new));
        assert_eq!(edits.len(), 3000);
    }
}
