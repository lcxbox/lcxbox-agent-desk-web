use super::*;

fn base() -> url::Url {
    url::Url::parse("https://example.com/docs/page.html").unwrap()
}

#[test]
fn readable_media_types_map_consistently() {
    for media_type in READABLE_MEDIA_TYPES {
        let format = readable_format(media_type).expect(media_type);
        assert_eq!(
            readable_format(&media_type.to_ascii_uppercase()),
            Some(format)
        );
        assert_eq!(
            readable_format(&format!("{media_type}; charset=utf-8")),
            Some(format)
        );
    }
    assert_eq!(readable_format("text/html"), Some(ReadableFormat::Html));
    assert_eq!(
        readable_format("application/xhtml+xml"),
        Some(ReadableFormat::Html)
    );
    assert_eq!(
        readable_format("Text/Plain;charset=gbk"),
        Some(ReadableFormat::Text)
    );
    assert_eq!(
        readable_format(" text/markdown ; variant=GFM"),
        Some(ReadableFormat::Markdown)
    );
    assert_eq!(
        readable_format("application/pdf"),
        Some(ReadableFormat::Pdf)
    );
    assert_eq!(readable_format("application/json"), None);
    assert_eq!(readable_format("text/htmlx"), None);
    assert_eq!(readable_format(""), None);
}

#[test]
fn pdf_and_oversized_sources_are_rejected() {
    let error =
        to_markdown(b"%PDF-1.7", ReadableFormat::Pdf, None, &base(), false, 1024).unwrap_err();
    assert_eq!(error.code, "unsupported_source_format");

    let source = vec![b'a'; MAX_TEXT_SOURCE_BYTES + 1];
    let error = to_markdown(&source, ReadableFormat::Text, None, &base(), false, 1024).unwrap_err();
    assert_eq!(error.code, "source_too_large");

    let source = vec![b'a'; MAX_TEXT_SOURCE_BYTES];
    let document = to_markdown(&source, ReadableFormat::Text, None, &base(), false, 16).unwrap();
    assert_eq!(document.markdown, "a".repeat(16));
}

#[test]
fn text_trims_line_ends_and_keeps_newlines() {
    let document = to_markdown(
        b"first line  \r\n\tsecond\t\n\n\nthird   ",
        ReadableFormat::Text,
        None,
        &base(),
        false,
        1024,
    )
    .unwrap();
    assert_eq!(document.markdown, "first line\n\tsecond\n\n\nthird");
    assert!(!document.truncated);
    assert!(document.notices.is_empty());
    assert_eq!(document.title, None);
}

#[test]
fn markdown_is_returned_as_is() {
    let source = "# Title  \n\n[relative](../other.md) <b>raw</b>\n";
    let document = to_markdown(
        source.as_bytes(),
        ReadableFormat::Markdown,
        Some("utf-8"),
        &base(),
        false,
        1024,
    )
    .unwrap();
    assert_eq!(document.markdown, source);
    assert!(!document.truncated);
}

#[test]
fn output_is_cut_on_a_char_boundary() {
    let document = to_markdown(
        "ab中文".as_bytes(),
        ReadableFormat::Text,
        None,
        &base(),
        false,
        4,
    )
    .unwrap();
    assert_eq!(document.markdown, "ab");
    assert!(document.truncated);
    assert_eq!(document.notices, vec![ReadableNotice::OutputTruncated]);

    let document = to_markdown(
        "<p>hello world</p>".as_bytes(),
        ReadableFormat::Html,
        None,
        &base(),
        false,
        5,
    )
    .unwrap();
    assert_eq!(document.markdown, "hello");
    assert!(document.truncated);
    assert_eq!(document.notices, vec![ReadableNotice::OutputTruncated]);
}

#[test]
fn truncated_source_marks_the_document_truncated() {
    let document =
        to_markdown(b"partial", ReadableFormat::Text, None, &base(), true, 1024).unwrap();
    assert_eq!(document.markdown, "partial");
    assert!(document.truncated);
    assert!(document.notices.is_empty());
}

#[test]
fn title_is_clamped() {
    assert_eq!(clamp_title("  A \n\t title  "), Some("A title".to_owned()));
    assert_eq!(clamp_title(" \n "), None);
    let long = "标".repeat(2_500);
    let clamped = clamp_title(&long).unwrap();
    assert_eq!(clamped.chars().count(), 2_000);
    let words = "ab ".repeat(1_000);
    let clamped = clamp_title(&words).unwrap();
    assert!(clamped.chars().count() <= 2_000);
    assert!(!clamped.ends_with(' '));
}

#[test]
fn published_at_is_clamped() {
    assert_eq!(
        clamp_published_at(" 2026-11-15T08:00:00Z "),
        Some("2026-11-15T08:00:00Z".to_owned())
    );
    assert_eq!(clamp_published_at(""), None);
    assert_eq!(clamp_published_at("2026-11-15\u{7}"), None);
    assert_eq!(clamp_published_at("2026\n11-15"), None);
    assert_eq!(clamp_published_at(&"1".repeat(256)), Some("1".repeat(256)));
    assert_eq!(clamp_published_at(&"1".repeat(257)), None);
    assert_eq!(clamp_published_at(&"年".repeat(86)), None);
}
