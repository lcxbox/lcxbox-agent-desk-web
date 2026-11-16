use super::*;
use crate::readable::{ReadableFormat, to_markdown};

/// "中文" in GBK.
const GBK_TEXT: &[u8] = &[0xD6, 0xD0, 0xCE, 0xC4];
/// "日本語" in Shift_JIS.
const SHIFT_JIS_TEXT: &[u8] = &[0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA];
/// "café €" in windows-1252.
const WINDOWS_1252_TEXT: &[u8] = b"caf\xE9 \x80";

fn html(head: &str, body: &[u8]) -> Vec<u8> {
    let mut source = format!("<!doctype html><html><head>{head}</head><body><p>").into_bytes();
    source.extend_from_slice(body);
    source.extend_from_slice(b"</p></body></html>");
    source
}

#[test]
fn byte_order_mark_wins_over_declarations() {
    let mut source = vec![0xEF, 0xBB, 0xBF];
    source.extend_from_slice("中文".as_bytes());
    assert_eq!(decode(&source, Some("gbk"), true, false), "中文");

    let source = [0xFF, 0xFE, b'h', 0, b'i', 0];
    assert_eq!(decode(&source, Some("windows-1252"), false, false), "hi");
    let source = [0xFE, 0xFF, 0, b'h', 0, b'i'];
    assert_eq!(decode(&source, None, true, false), "hi");
}

#[test]
fn content_type_charset_decodes_each_encoding() {
    assert_eq!(decode(GBK_TEXT, Some("gbk"), false, false), "中文");
    assert_eq!(
        decode(SHIFT_JIS_TEXT, Some("Shift_JIS"), false, false),
        "日本語"
    );
    assert_eq!(
        decode(WINDOWS_1252_TEXT, Some("\"windows-1252\""), false, false),
        "café €"
    );
}

#[test]
fn content_type_charset_wins_over_meta() {
    let source = html(r#"<meta charset="gbk">"#, WINDOWS_1252_TEXT);
    assert!(decode(&source, Some("windows-1252"), true, false).contains("café €"));
}

#[test]
fn meta_charset_decodes_each_encoding() {
    let cases: [(&str, &[u8], &str); 3] = [
        ("gbk", GBK_TEXT, "中文"),
        ("shift_jis", SHIFT_JIS_TEXT, "日本語"),
        ("windows-1252", WINDOWS_1252_TEXT, "café €"),
    ];
    for (label, body, expected) in cases {
        let source = html(&format!("<meta charset={label}>"), body);
        assert!(
            decode(&source, None, true, false).contains(expected),
            "{label}"
        );
        let source = html(&format!("<META CHARSET='{label}' />"), body);
        assert!(
            decode(&source, None, true, false).contains(expected),
            "{label}"
        );
    }
}

#[test]
fn http_equiv_decodes_each_encoding() {
    let cases: [(&str, &[u8], &str); 3] = [
        ("GBK", GBK_TEXT, "中文"),
        ("Shift_JIS", SHIFT_JIS_TEXT, "日本語"),
        ("windows-1252", WINDOWS_1252_TEXT, "café €"),
    ];
    for (label, body, expected) in cases {
        let source = html(
            &format!(
                r#"<title>x</title><meta http-equiv="Content-Type" content="text/html; charset={label}">"#
            ),
            body,
        );
        assert!(
            decode(&source, None, true, false).contains(expected),
            "{label}"
        );
        // `content` without the http-equiv pragma is not a declaration.
        let source = html(
            &format!(r#"<meta content="text/html; charset={label}">"#),
            body,
        );
        assert!(
            !decode(&source, None, true, false).contains(expected),
            "{label}"
        );
    }
}

#[test]
fn xml_declaration_decodes_each_encoding() {
    let cases: [(&str, &[u8], &str); 3] = [
        ("GBK", GBK_TEXT, "中文"),
        ("Shift_JIS", SHIFT_JIS_TEXT, "日本語"),
        ("windows-1252", WINDOWS_1252_TEXT, "café €"),
    ];
    for (label, body, expected) in cases {
        let mut source =
            format!(r#"<?xml version="1.0" encoding="{label}"?><html><body><p>"#).into_bytes();
        source.extend_from_slice(body);
        assert!(
            decode(&source, None, true, false).contains(expected),
            "{label}"
        );
    }
}

#[test]
fn meta_is_only_sniffed_for_html_and_within_the_prescan_window() {
    let source = html("<meta charset=gbk>", GBK_TEXT);
    assert!(!decode(&source, None, false, false).contains("中文"));

    let padding = format!("<!-- {} -->", "x".repeat(PRESCAN_BYTES));
    let source = html(&format!("{padding}<meta charset=gbk>"), GBK_TEXT);
    assert!(!decode(&source, None, true, false).contains("中文"));

    let source = html("<!-- <meta charset=gbk> -->", GBK_TEXT);
    assert!(!decode(&source, None, true, false).contains("中文"));

    let source = html(
        r#"<link title="<meta charset=gbk>"><meta charset="shift_jis">"#,
        SHIFT_JIS_TEXT,
    );
    assert!(decode(&source, None, true, false).contains("日本語"));
}

#[test]
fn text_plain_with_gbk_charset() {
    let base = url::Url::parse("https://example.com/a.txt").unwrap();
    let content_type = "text/plain;charset=gbk";
    let charset = content_type.split_once("charset=").map(|(_, label)| label);
    let format = crate::readable::readable_format(content_type).unwrap();
    let document = to_markdown(GBK_TEXT, format, charset, &base, false, 1024).unwrap();
    assert_eq!(document.markdown, "中文");
}

#[test]
fn meta_declared_utf16_is_treated_as_utf8() {
    let source = html(r#"<meta charset="utf-16">"#, "中文".as_bytes());
    assert!(decode(&source, None, true, false).contains("中文"));
    let source = html(
        r#"<meta http-equiv="content-type" content="text/html;charset=UTF-16LE">"#,
        "中文".as_bytes(),
    );
    assert!(decode(&source, None, true, false).contains("中文"));
    let mut source = br#"<?xml version="1.0" encoding="UTF-16"?><p>"#.to_vec();
    source.extend_from_slice("中文".as_bytes());
    assert!(decode(&source, None, true, false).contains("中文"));
}

#[test]
fn unknown_labels_fall_back_to_utf8() {
    assert_eq!(
        decode("中文".as_bytes(), Some("no-such-charset"), false, false),
        "中文"
    );
    let source = html("<meta charset=no-such-charset>", "中文".as_bytes());
    assert!(decode(&source, None, true, false).contains("中文"));
}

#[test]
fn unmappable_bytes_become_replacement_characters() {
    assert_eq!(decode(b"a\xFFb", None, false, false), "a\u{FFFD}b");
    assert_eq!(decode(b"a\x80b", Some("gbk"), false, false), "a€b");
    assert_eq!(decode(b"a\xFFb", Some("gbk"), false, false), "a\u{FFFD}b");
}

#[test]
fn truncated_source_drops_the_incomplete_trailing_character() {
    let mut source = "ab中".as_bytes().to_vec();
    source.extend_from_slice(&"文".as_bytes()[..2]);
    assert_eq!(decode(&source, None, false, true), "ab中");
    assert_eq!(decode(&source, None, false, false), "ab中\u{FFFD}");

    let source = [GBK_TEXT, &GBK_TEXT[..1]].concat();
    assert_eq!(decode(&source, Some("gbk"), false, true), "中文");
    assert_eq!(decode(&source, Some("gbk"), false, false), "中文\u{FFFD}");
}

#[test]
fn large_single_byte_source_is_not_rejected() {
    let base = url::Url::parse("https://example.com/a.txt").unwrap();
    let source = vec![0xE9; 2 * 1024 * 1024];
    let document = to_markdown(
        &source,
        ReadableFormat::Text,
        Some("windows-1252"),
        &base,
        false,
        8 * 1024 * 1024,
    )
    .unwrap();
    assert_eq!(document.markdown.len(), 4 * 1024 * 1024);
    assert!(document.markdown.chars().all(|ch| ch == 'é'));
    assert!(!document.truncated);
}
