//! Root traversal through directory handles. Links, reparse points, special
//! files, files with more than one link and other filesystems are skipped.
//! Ignore rules are `.gitignore`/`.ignore` of each directory level, matched
//! with the `ignore` crate's gitignore matcher, plus built-in excludes. The
//! rules are loaded by the pre-run scan only and reused unchanged by the
//! post-run scan, so a command that edits its ignore files does not create
//! phantom additions or deletions.
use super::*;
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use scan_fs::{Kind, Stat};
use std::io::Read;

/// Directory names never traversed.
pub(crate) const BUILTIN_EXCLUDES: [&str; 7] = [
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "__pycache__",
];
const IGNORE_FILES: [&str; 2] = [".gitignore", ".ignore"];
const MAX_IGNORE_FILE_BYTES: u64 = 1024 * 1024;
/// Deeper directories are not traversed.
const MAX_DEPTH: usize = 128;

/// Why a scan stopped early.
#[derive(Debug)]
pub(crate) enum Stop {
    Skip(SkipReason),
    /// The conversation was deleted or the record is gone.
    Discarded,
    Io(io::Error),
}
impl From<io::Error> for Stop {
    fn from(error: io::Error) -> Self {
        Stop::Io(error)
    }
}

/// Ignore matchers per directory, keyed by the directory's root-relative
/// path (`""` for the root).
#[derive(Default)]
pub(crate) struct Rules {
    levels: HashMap<String, Gitignore>,
}
impl Rules {
    /// Read a directory's ignore files. Unreadable files and invalid lines
    /// add no rules.
    fn load(&mut self, directory: &File, rel: &str) {
        let mut builder = GitignoreBuilder::new(".");
        let mut lines = 0;
        for name in IGNORE_FILES {
            let Ok(Some(stat)) = scan_fs::stat_at(directory, name) else {
                continue;
            };
            if stat.kind != Kind::File || stat.size > MAX_IGNORE_FILE_BYTES {
                continue;
            }
            let Ok(file) = scan_fs::open(directory, name, false) else {
                continue;
            };
            let mut text = String::new();
            if file
                .take(MAX_IGNORE_FILE_BYTES)
                .read_to_string(&mut text)
                .is_err()
            {
                continue;
            }
            for line in text.lines() {
                if builder.add_line(None, line).is_ok() {
                    lines += 1;
                }
            }
        }
        if lines > 0
            && let Ok(matcher) = builder.build()
        {
            self.levels.insert(rel.to_owned(), matcher);
        }
    }

    /// The deepest level with a matching rule decides.
    fn ignored(&self, path: &str, is_dir: bool) -> bool {
        let mut level = parent(path);
        loop {
            if let Some(matcher) = self.levels.get(level) {
                let relative = if level.is_empty() {
                    path
                } else {
                    &path[level.len() + 1..]
                };
                match matcher.matched(relative, is_dir) {
                    Match::Ignore(_) => return true,
                    Match::Whitelist(_) => return false,
                    Match::None => (),
                }
            }
            if level.is_empty() {
                return false;
            }
            level = parent(level);
        }
    }
}

fn parent(path: &str) -> &str {
    path.rfind('/').map_or("", |index| &path[..index])
}

/// Traversal state shared by the roots of one scan.
pub(crate) struct Walk<'a> {
    pub rules: &'a mut Rules,
    /// Pre-run scans load rules; post-run scans reuse them.
    pub load_rules: bool,
    pub deadline: Instant,
    /// Files seen by this scan across all roots.
    pub files: &'a mut usize,
    pub max_files: usize,
}

/// Receives the parent handle, name, root-relative path and stat of a file.
pub(crate) type Visit<'a> = dyn FnMut(&File, &str, String, Stat) -> Result<(), Stop> + 'a;

struct Level {
    directory: File,
    rel: String,
    names: std::vec::IntoIter<String>,
}

/// Visit every included regular file of a root with its parent handle,
/// name, root-relative path and stat.
pub(crate) fn walk(root: &File, walk: Walk<'_>, visit: &mut Visit<'_>) -> Result<(), Stop> {
    let device = scan_fs::stat(root)?.device;
    let level = |directory: File, rel: String, rules: &mut Rules| -> io::Result<Level> {
        if walk.load_rules {
            rules.load(&directory, &rel);
        }
        let mut names = scan_fs::list(&directory)?;
        names.sort();
        Ok(Level {
            directory,
            rel,
            names: names.into_iter(),
        })
    };
    let mut stack = vec![level(root.try_clone()?, String::new(), walk.rules)?];
    loop {
        let depth = stack.len();
        let Some(current) = stack.last_mut() else {
            break;
        };
        if Instant::now() >= walk.deadline {
            return Err(Stop::Skip(SkipReason::TimeLimit));
        }
        let Some(name) = current.names.next() else {
            stack.pop();
            continue;
        };
        let path = if current.rel.is_empty() {
            name.clone()
        } else {
            format!("{}/{name}", current.rel)
        };
        if BUILTIN_EXCLUDES.contains(&name.as_str()) || validate_rel_path(&path, false).is_err() {
            continue;
        }
        let Some(stat) = scan_fs::stat_at(&current.directory, &name)? else {
            continue;
        };
        match stat.kind {
            Kind::Directory => {
                if stat.device != device || depth >= MAX_DEPTH || walk.rules.ignored(&path, true) {
                    continue;
                }
                // An unreadable directory is left out, like an excluded one.
                let Ok(directory) = scan_fs::open(&current.directory, &name, true) else {
                    continue;
                };
                if scan_fs::stat(&directory)?.file_id != stat.file_id {
                    continue;
                }
                let Ok(next) = level(directory, path, walk.rules) else {
                    continue;
                };
                stack.push(next);
            }
            Kind::File => {
                if stat.links != 1 || walk.rules.ignored(&path, false) {
                    continue;
                }
                *walk.files += 1;
                if *walk.files > walk.max_files {
                    return Err(Stop::Skip(SkipReason::TooManyFiles));
                }
                visit(&current.directory, &name, path, stat)?;
            }
            Kind::Other => (),
        }
    }
    Ok(())
}

/// Read a file's content up to the text bound. `None` when the file is
/// larger than the bound.
pub(crate) fn read_bounded(file: File) -> io::Result<Option<Vec<u8>>> {
    #[cfg(test)]
    CONTENT_READS.with(|reads| reads.set(reads.get() + 1));
    let mut bytes = Vec::new();
    file.take(MAX_TEXT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok((bytes.len() <= MAX_TEXT_BYTES).then_some(bytes))
}

pub(crate) fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

#[cfg(test)]
thread_local! {
    /// File contents read by scans on this thread.
    pub(crate) static CONTENT_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
