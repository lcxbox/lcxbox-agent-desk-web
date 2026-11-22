use super::*;
use crate::{ExtractedFormat, PageMarkerStyle, PageRange, PdfPageSelection};

fn round_trip(request: SandboxRequest, source: &[u8]) -> (SandboxResponse, Vec<u8>) {
    in_process_runner(&request, source, &pdf_spec()).unwrap()
}

fn options(pages: PdfPageSelection, max_output_bytes: usize) -> PdfExtractOptions {
    PdfExtractOptions {
        kind: ExtractedFormat::Markdown,
        pages,
        page_markers: Some(PageMarkerStyle::HtmlComment),
        max_output_bytes,
    }
}

#[test]
fn oversized_frames_are_rejected_before_allocation() {
    let mut frame = Vec::new();
    frame.extend_from_slice(&((FRAMES.header_bytes as u32) + 1).to_le_bytes());
    assert!(process_sandbox::read_frame::<SandboxRequest>(&mut frame.as_slice(), FRAMES).is_err());
    let mut frame = Vec::new();
    let header = br#"{"op":"convert","kind":"text_to_pdf"}"#;
    frame.extend_from_slice(&(header.len() as u32).to_le_bytes());
    frame.extend_from_slice(header);
    frame.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(process_sandbox::read_frame::<SandboxRequest>(&mut frame.as_slice(), FRAMES).is_err());
}

#[test]
fn unknown_codes_and_long_messages_are_normalized() {
    let error = error_from_wire("anything", "界".repeat(10_000));
    assert_eq!(error.code, "document_compile_failed");
    assert!(error.message.len() <= 4 * 1024);
    for code in [
        "encrypted_pdf",
        "ocr_required",
        "unsupported_in_build",
        "page_limit_exceeded",
    ] {
        assert_eq!(error_from_wire(code, String::new()).code, code);
    }
}

#[test]
fn sandbox_failures_map_to_definite_errors() {
    let cases = [
        (SandboxFailure::Timeout, "document_conversion_timeout"),
        (
            SandboxFailure::Exited(None),
            "document_resource_limit_exceeded",
        ),
        (
            SandboxFailure::Unavailable("missing".into()),
            "document_sandbox_unavailable",
        ),
        (
            SandboxFailure::Protocol("bad".into()),
            "document_sandbox_unavailable",
        ),
    ];
    for (failure, code) in cases {
        let failing = move |_: &SandboxRequest, _: &[u8], _: &SandboxSpec| Err(failure.clone());
        let pdf = options(PdfPageSelection::Leading { max_pages: 1 }, 1024);
        assert_eq!(
            extract_pdf(b"%PDF-1.7", &pdf, &failing).unwrap_err().code,
            code
        );
    }
}

#[test]
fn a_child_cannot_return_more_text_than_requested() {
    let oversized = |_: &SandboxRequest, _: &[u8], _: &SandboxSpec| {
        Ok((
            SandboxResponse::Extracted {
                details: ConversionDetails::PdfExtraction {
                    source_pages: 1,
                    selected_page_ranges: vec![PageRange { start: 1, end: 1 }],
                    converted_pages: 1,
                },
                title: Some("界".repeat(3000)),
                published_at: None,
                truncated: false,
                warnings: vec![],
            },
            vec![b'a'; 11],
        ))
    };
    let pdf = options(PdfPageSelection::Leading { max_pages: 1 }, 10);
    assert_eq!(
        extract_pdf(b"%PDF-1.7", &pdf, &oversized).unwrap_err().code,
        "document_sandbox_unavailable"
    );
}

#[test]
fn malformed_pdfs_fail_with_known_codes() {
    let pdf = options(PdfPageSelection::Leading { max_pages: 1 }, 1024);
    assert_eq!(
        extract_pdf(b"plain text", &pdf, &in_process_runner)
            .unwrap_err()
            .code,
        "source_format_mismatch"
    );
    assert_eq!(
        extract_pdf(b"%PDF-1.7\ngarbage", &pdf, &in_process_runner)
            .unwrap_err()
            .code,
        "invalid_pdf"
    );
}

#[cfg(feature = "typst")]
mod generated {
    use super::*;

    /// A PDF of `pages` pages, each saying "Page N".
    fn pdf(pages: u32) -> Vec<u8> {
        let source = (1..=pages)
            .map(|page| format!("Page {page}"))
            .collect::<Vec<_>>()
            .join("\n#pagebreak()\n");
        run_typst(
            source.as_bytes(),
            ConversionKind::TypstToPdf,
            &in_process_runner,
        )
        .unwrap()
        .bytes
    }

    #[test]
    fn converts_and_renders_through_the_frame_protocol() {
        let (response, pdf) = round_trip(
            SandboxRequest::Convert {
                kind: ConversionKind::TextToPdf,
            },
            b"Hello",
        );
        assert!(matches!(response, SandboxResponse::Converted { .. }));
        assert!(pdf.starts_with(b"%PDF-"));
        let page = render_page(b"# Title", SourceFormat::Markdown, 1, &in_process_runner).unwrap();
        assert_eq!(page.page_count, 1);
        assert!(page.png.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn compile_errors_come_back_as_structured_failures() {
        let error = run_typst(
            "#panic(\"界\" * 1000)".as_bytes(),
            ConversionKind::TypstToPdf,
            &in_process_runner,
        )
        .unwrap_err();
        assert_eq!(error.code, "document_compile_failed");
        assert_eq!(
            run_typst(b"%PDF-", ConversionKind::PdfToText, &in_process_runner)
                .unwrap_err()
                .code,
            "invalid_conversion_options"
        );
    }

    #[test]
    fn exact_pages_keep_ranges_markers_and_errors() {
        let source = pdf(4);
        let exact = options(
            PdfPageSelection::Exact {
                pages: vec![PageRange { start: 2, end: 3 }],
            },
            MAX_OUTPUT_BYTES,
        );
        let extracted = extract_pdf(&source, &exact, &in_process_runner).unwrap();
        assert!(extracted.text.contains("<!-- lcxbox-page: 2 -->"));
        assert!(extracted.text.contains("Page 3"));
        assert!(!extracted.text.contains("Page 1"));
        assert!(!extracted.truncated);
        assert_eq!(
            extracted.details,
            ConversionDetails::PdfExtraction {
                source_pages: 4,
                selected_page_ranges: vec![PageRange { start: 2, end: 3 }],
                converted_pages: 2,
            }
        );
        let out_of_range = options(
            PdfPageSelection::Exact {
                pages: vec![PageRange { start: 5, end: 5 }],
            },
            MAX_OUTPUT_BYTES,
        );
        assert_eq!(
            extract_pdf(&source, &out_of_range, &in_process_runner)
                .unwrap_err()
                .code,
            "page_out_of_range"
        );
        // An exact selection never truncates silently.
        let small = options(PdfPageSelection::Exact { pages: vec![] }, 20);
        assert_eq!(
            extract_pdf(&source, &small, &in_process_runner)
                .unwrap_err()
                .code,
            "output_too_large"
        );
        let text_with_comments = PdfExtractOptions {
            kind: ExtractedFormat::Text,
            ..options(PdfPageSelection::Exact { pages: vec![] }, MAX_OUTPUT_BYTES)
        };
        assert_eq!(
            extract_pdf(&source, &text_with_comments, &in_process_runner)
                .unwrap_err()
                .code,
            "invalid_conversion_options"
        );
    }

    #[test]
    fn leading_pages_stop_at_the_page_and_output_budgets() {
        let source = pdf(4);
        let first_two = options(PdfPageSelection::Leading { max_pages: 2 }, MAX_OUTPUT_BYTES);
        let extracted = extract_pdf(&source, &first_two, &in_process_runner).unwrap();
        assert!(extracted.text.contains("Page 2") && !extracted.text.contains("Page 3"));
        assert!(!extracted.truncated);
        assert!(matches!(
            extracted.details,
            ConversionDetails::PdfExtraction {
                source_pages: 4,
                converted_pages: 2,
                ..
            }
        ));
        let budget = options(PdfPageSelection::Leading { max_pages: 4 }, 40);
        let extracted = extract_pdf(&source, &budget, &in_process_runner).unwrap();
        assert!(extracted.truncated);
        assert!(extracted.text.len() <= 40);
        assert!(matches!(
            extracted.details,
            ConversionDetails::PdfExtraction { converted_pages, .. } if converted_pages < 4
        ));
    }
}

#[cfg(not(feature = "typst"))]
#[test]
fn generation_is_unsupported_without_typst() {
    for request in [
        SandboxRequest::Convert {
            kind: ConversionKind::TextToPdf,
        },
        SandboxRequest::RenderPage {
            format: SourceFormat::Text,
            page: 1,
        },
    ] {
        let (response, _) = round_trip(request, b"Hello");
        assert!(
            matches!(response, SandboxResponse::Failed { ref code, .. } if code == "unsupported_in_build")
        );
    }
}
