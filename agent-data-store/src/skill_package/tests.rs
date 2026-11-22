use super::*;
use desk_agent_protocol::ai_agent::skill::SkillFileInput;
use std::io::Write;

fn text_input(skill_md: &str, files: &[(&str, &str)]) -> SkillPackageInput {
    SkillPackageInput {
        zip_base64: None,
        skill_md: Some(skill_md.into()),
        files: files
            .iter()
            .map(|(path, text)| SkillFileInput {
                path: (*path).into(),
                text: (*text).into(),
            })
            .collect(),
    }
}

fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut out);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap();
    }
    out.into_inner()
}

fn zip_input(bytes: &[u8]) -> SkillPackageInput {
    use base64::Engine as _;
    SkillPackageInput {
        zip_base64: Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
        ..Default::default()
    }
}

const BASIC: &str =
    "---\nname: pdf-tools\ndescription: Work with PDF files.\n---\n# Steps\nDo it.\n";

#[test]
fn standard_frontmatter_with_block_scalars_metadata_and_extensions_is_accepted() {
    let skill_md = "---\nname: pdf-tools\ndescription: >\n  Extract text from PDFs.\n  Use when handling PDFs.\nlicense: Apache-2.0\ncompatibility: Requires python3\nmetadata:\n  author: example-org\n  version: \"1.0\"\nallowed-tools: Bash(git:*) Read\ndisable-model-invocation: true\nhooks:\n  pre: [a, b]\n---\nBody\n";
    let parsed = parse_input(&text_input(skill_md, &[]), None).unwrap();
    let package = parsed.package;
    assert_eq!(package.name, "pdf-tools");
    assert_eq!(
        package.description,
        "Extract text from PDFs. Use when handling PDFs.\n"
    );
    assert_eq!(package.license.as_deref(), Some("Apache-2.0"));
    assert_eq!(package.compatibility.as_deref(), Some("Requires python3"));
    assert_eq!(package.metadata["version"], "1.0");
    assert_eq!(package.allowed_tools.as_deref(), Some("Bash(git:*) Read"));
    assert_eq!(package.body, "Body\n");
}

#[test]
fn allowed_tools_list_is_joined() {
    let skill_md = "---\nname: a\ndescription: b\nallowed-tools:\n  - Read\n  - Grep\n---\nx";
    let parsed = parse_input(&text_input(skill_md, &[]), None).unwrap();
    assert_eq!(parsed.package.allowed_tools.as_deref(), Some("Read Grep"));
}

#[test]
fn rendered_skill_md_round_trips() {
    let skill_md = "---\nname: pdf-tools\ndescription: \"Quote \\\" and \\\\ and\\nnewline\"\nmetadata:\n  a: b\n---\nBody\nmore";
    let package = parse_input(&text_input(skill_md, &[]), None)
        .unwrap()
        .package;
    let again = parse_input(&text_input(&package.render_skill_md(), &[]), None)
        .unwrap()
        .package;
    assert_eq!(package, again);
}

#[test]
fn invalid_names_and_reserved_names_are_rejected() {
    for name in ["PDF", "-a", "a-", "a--b", "", "a_b"] {
        let skill_md = format!("---\nname: \"{name}\"\ndescription: d\n---\nx");
        assert!(
            matches!(
                parse_input(&text_input(&skill_md, &[]), None),
                Err(PackageError::InvalidName)
            ),
            "{name}"
        );
    }
    let reserved = format!(
        "---\nname: {}\ndescription: d\n---\nx",
        desk_diagnose_core::skills::bundled_names()[0]
    );
    assert_eq!(
        parse_input(&text_input(&reserved, &[]), None).unwrap_err(),
        PackageError::ReservedName
    );
}

#[test]
fn an_existing_skill_keeps_its_name() {
    assert_eq!(
        parse_input(&text_input(BASIC, &[]), Some("other")).unwrap_err(),
        PackageError::NameMismatch
    );
    assert!(parse_input(&text_input(BASIC, &[]), Some("pdf-tools")).is_ok());
}

#[test]
fn yaml_aliases_anchors_and_extra_documents_are_rejected() {
    for header in [
        "name: &n a\ndescription: d",
        "name: a\ndescription: *n",
        "name: a\ndescription: d\n...\nx: y",
        "name: a\nname: b\ndescription: d",
        "- a\n- b",
    ] {
        let skill_md = format!("---\n{header}\n---\nbody");
        assert!(
            parse_input(&text_input(&skill_md, &[]), None).is_err(),
            "{header}"
        );
    }
}

#[test]
fn missing_or_empty_parts_are_rejected() {
    for skill_md in [
        "no frontmatter",
        "---\nname: a\ndescription: d\n",
        "---\nname: a\n---\nbody",
        "---\nname: a\ndescription: d\n---\n   \n",
    ] {
        assert!(parse_input(&text_input(skill_md, &[]), None).is_err());
    }
    assert_eq!(
        parse_input(&SkillPackageInput::default(), None).unwrap_err(),
        PackageError::MissingInput
    );
}

#[test]
fn files_are_classified_and_bad_paths_are_skipped() {
    let parsed = parse_input(
        &text_input(
            BASIC,
            &[
                ("scripts/extract.py", "print(1)"),
                ("references/guide.md", "guide"),
                ("assets/template.txt", "tpl"),
                ("notes.md", "n"),
                ("../escape.md", "x"),
                ("a//b.md", "x"),
                ("SKILL.md", "dup"),
            ],
        ),
        None,
    )
    .unwrap();
    let kinds: Vec<_> = parsed
        .package
        .resources
        .iter()
        .map(|r| (r.path.as_str(), r.kind, r.language.as_deref()))
        .collect();
    assert_eq!(
        kinds,
        [
            ("assets/template.txt", SkillResourceKind::Asset, None),
            ("notes.md", SkillResourceKind::Other, None),
            ("references/guide.md", SkillResourceKind::Reference, None),
            (
                "scripts/extract.py",
                SkillResourceKind::Script,
                Some("python")
            ),
        ]
    );
    assert_eq!(parsed.skipped.len(), 3);
    assert!(
        parsed
            .skipped
            .iter()
            .all(|s| s.reason == SkillSkipReason::InvalidPath)
    );
}

#[test]
fn oversized_files_are_skipped_and_long_bodies_warn() {
    let big = "x".repeat(MAX_RESOURCE_BYTES + 1);
    let body: String = (0..600).map(|_| "line\n").collect();
    let skill_md = format!("---\nname: a\ndescription: d\n---\n{body}");
    let parsed = parse_input(&text_input(&skill_md, &[("references/big.md", &big)]), None).unwrap();
    assert_eq!(parsed.skipped[0].reason, SkillSkipReason::TooLarge);
    assert_eq!(parsed.warnings, [SkillWarning::LongBody]);
}

#[test]
fn credentials_are_rejected_with_the_file_name() {
    let error = parse_input(
        &text_input(
            BASIC,
            &[(
                "scripts/run.sh",
                "export TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            )],
        ),
        None,
    )
    .unwrap_err();
    assert_eq!(error, PackageError::Credentials("scripts/run.sh".into()));
}

#[test]
fn zip_with_top_level_directory_is_unpacked_and_binaries_are_skipped() {
    let bytes = zip(&[
        ("pdf-tools/SKILL.md", BASIC.as_bytes()),
        ("pdf-tools/scripts/run.sh", b"echo hi"),
        ("pdf-tools/assets/logo.png", &[0x89, b'P', b'N', b'G', 0, 1]),
        ("__MACOSX/pdf-tools/._SKILL.md", b"junk"),
    ]);
    let parsed = parse_input(&zip_input(&bytes), None).unwrap();
    assert_eq!(parsed.package.resources.len(), 1);
    assert_eq!(parsed.package.resources[0].path, "scripts/run.sh");
    assert_eq!(parsed.skipped.len(), 1);
    assert_eq!(parsed.skipped[0].reason, SkillSkipReason::Binary);
}

#[test]
fn zip_directory_must_match_the_name_and_hold_skill_md() {
    let bytes = zip(&[("other/SKILL.md", BASIC.as_bytes())]);
    assert_eq!(
        parse_input(&zip_input(&bytes), None).unwrap_err(),
        PackageError::NameMismatch
    );
    let bytes = zip(&[("a/SKILL.md", BASIC.as_bytes()), ("b/x.md", b"x")]);
    assert_eq!(
        parse_input(&zip_input(&bytes), None).unwrap_err(),
        PackageError::MissingSkillFile
    );
    let root = zip(&[("SKILL.md", BASIC.as_bytes())]);
    assert!(parse_input(&zip_input(&root), None).is_ok());
}

#[test]
fn compression_bombs_and_garbage_are_rejected() {
    let bomb = vec![b'a'; 900 * 1024];
    let bytes = zip(&[("SKILL.md", BASIC.as_bytes()), ("references/a.md", &bomb)]);
    assert_eq!(
        parse_input(&zip_input(&bytes), None).unwrap_err(),
        PackageError::InvalidZip
    );
    assert_eq!(
        parse_input(&zip_input(b"not a zip"), None).unwrap_err(),
        PackageError::InvalidZip
    );
}

#[test]
fn package_diff_starts_with_skill_md_and_lists_file_changes() {
    let old = parse_input(&text_input(BASIC, &[("references/a.md", "one\n")]), None)
        .unwrap()
        .package;
    let new_md = BASIC.replace("Do it.", "Do it carefully.");
    let new = parse_input(&text_input(&new_md, &[("references/b.md", "two\n")]), None)
        .unwrap()
        .package;
    let diff = diff::diff_packages(Some(&old), &new, 64 * 1024);
    let changes: Vec<_> = diff.iter().map(|d| (d.path.as_str(), d.change)).collect();
    use desk_agent_protocol::ai_agent::skill::SkillFileChange::*;
    assert_eq!(
        changes,
        [
            ("SKILL.md", Modified),
            ("references/a.md", Removed),
            ("references/b.md", Added)
        ]
    );
}
