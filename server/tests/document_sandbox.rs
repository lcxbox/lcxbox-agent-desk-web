//! Runs the real document sandbox child (the built server executable): the
//! dispatch in `main`, Typst generation and preview, and PDF extraction with
//! exact page selection, all contained by the process boundary and limits.
use std::path::Path;
use std::time::{Duration, Instant};

use desk_document_conversion::sandbox::{self, SERVICE_NAME, SandboxRequest, SandboxResponse};
use desk_document_conversion::{
    ConversionDetails, ConversionError, ConversionKind, ExtractedFormat, MAX_OUTPUT_BYTES,
    PageMarkerStyle, PageRange, PdfExtractOptions, PdfPageSelection, SourceFormat,
};
use desk_utils::process_sandbox::{self, SandboxFailure, SandboxSpec};

fn server() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_lcxl-remote-desk-server"))
}

/// Runs requests in the built server executable.
fn server_runner(
    request: &SandboxRequest,
    source: &[u8],
    spec: &SandboxSpec,
) -> Result<(SandboxResponse, Vec<u8>), SandboxFailure> {
    process_sandbox::run_program(server(), SERVICE_NAME, request, source, spec)
}

fn typst(source: &str, spec: SandboxSpec) -> Result<(SandboxResponse, Vec<u8>), SandboxFailure> {
    process_sandbox::run_program(
        server(),
        SERVICE_NAME,
        &SandboxRequest::Convert {
            kind: ConversionKind::TypstToPdf,
        },
        source.as_bytes(),
        &spec,
    )
}

fn tight() -> SandboxSpec {
    SandboxSpec {
        memory_bytes: 512 * 1024 * 1024,
        cpu_seconds: 5,
        wall_clock: Duration::from_secs(10),
        ..sandbox::typst_spec()
    }
}

fn pdf(pages: u32) -> Vec<u8> {
    let source = (1..=pages)
        .map(|page| format!("Page {page}"))
        .collect::<Vec<_>>()
        .join("\n#pagebreak()\n");
    sandbox::run_typst(
        source.as_bytes(),
        ConversionKind::TypstToPdf,
        &server_runner,
    )
    .unwrap()
    .bytes
}

fn exact(pages: Vec<PageRange>, max_output_bytes: usize) -> PdfExtractOptions {
    PdfExtractOptions {
        kind: ExtractedFormat::Markdown,
        pages: PdfPageSelection::Exact { pages },
        page_markers: Some(PageMarkerStyle::HtmlComment),
        max_output_bytes,
    }
}

#[test]
fn ordinary_typst_converts_and_renders_in_the_child() {
    let (response, pdf) = typst("= Report\nHello", sandbox::typst_spec()).unwrap();
    assert!(matches!(response, SandboxResponse::Converted { .. }));
    assert!(pdf.starts_with(b"%PDF-"));
    let page =
        sandbox::render_page(b"# Preview", SourceFormat::Markdown, 1, &server_runner).unwrap();
    assert_eq!(page.page_count, 1);
    assert!(page.png.starts_with(b"\x89PNG"));
}

#[test]
fn pdf_extraction_keeps_exact_pages_markers_and_errors() {
    let source = pdf(3);
    let extracted = sandbox::extract_pdf(
        &source,
        &exact(vec![PageRange { start: 2, end: 2 }], MAX_OUTPUT_BYTES),
        &server_runner,
    )
    .unwrap();
    assert!(extracted.text.starts_with("<!-- lcxl-page: 2 -->"));
    assert!(extracted.text.contains("Page 2") && !extracted.text.contains("Page 3"));
    assert!(matches!(
        extracted.details,
        ConversionDetails::PdfExtraction {
            source_pages: 3,
            converted_pages: 1,
            ..
        }
    ));
    let code = |options: &PdfExtractOptions, source: &[u8]| {
        sandbox::extract_pdf(source, options, &server_runner)
            .map(|_| ())
            .unwrap_err()
            .code
    };
    assert_eq!(
        code(
            &exact(vec![PageRange { start: 4, end: 4 }], MAX_OUTPUT_BYTES),
            &source
        ),
        "page_out_of_range"
    );
    // An empty exact selection still means every page and still fails loudly.
    assert_eq!(code(&exact(vec![], 10), &source), "output_too_large");
    assert_eq!(
        code(&exact(vec![], MAX_OUTPUT_BYTES), &pdf(201)),
        "page_limit_exceeded"
    );
    assert_eq!(
        code(&exact(vec![], MAX_OUTPUT_BYTES), b"plain"),
        "source_format_mismatch"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_small_source_that_allocates_gigabytes_is_contained() {
    // 35 bytes of Typst that asks for about 1 GB: under the memory limit the
    // allocation fails and aborts only the child.
    let bomb = "#let large = \"x\" * 1000000000\nHello";
    assert!(bomb.len() <= 40);
    assert!(matches!(
        typst(bomb, tight()),
        Err(SandboxFailure::Exited(_))
    ));
    // The worker and the next conversion are unaffected.
    assert!(typst("Hello", tight()).is_ok());
}

#[test]
fn a_non_terminating_source_is_stopped() {
    let started = Instant::now();
    let limits = SandboxSpec {
        cpu_seconds: 2,
        wall_clock: Duration::from_secs(3),
        ..tight()
    };
    // Bounded but astronomically long: 10^10 loop iterations.
    let result = typst(
        "#let n = 0\n#for i in range(100000) { for j in range(100000) { n += 1 } }",
        limits,
    );
    assert!(
        matches!(
            result,
            Err(SandboxFailure::Timeout | SandboxFailure::Exited(_))
        ),
        "{result:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(8));
    assert!(typst("Hello", tight()).is_ok());
}

#[test]
fn compile_errors_are_structured_not_crashes() {
    let error: ConversionError = sandbox::run_typst(
        "#panic(\"界\" * 1000)".as_bytes(),
        ConversionKind::TypstToPdf,
        &server_runner,
    )
    .unwrap_err();
    assert_eq!(error.code, "document_compile_failed");
}

#[test]
fn an_unknown_service_or_program_is_unavailable() {
    let request = SandboxRequest::Convert {
        kind: ConversionKind::TextToPdf,
    };
    let unknown: Result<(SandboxResponse, Vec<u8>), _> =
        process_sandbox::run_program(server(), "missing", &request, b"Hello", &tight());
    assert!(matches!(unknown, Err(SandboxFailure::Unavailable(_))));
    let missing: Result<(SandboxResponse, Vec<u8>), _> = process_sandbox::run_program(
        Path::new("/nonexistent/lcxl-document-sandbox"),
        SERVICE_NAME,
        &request,
        b"Hello",
        &tight(),
    );
    assert!(matches!(missing, Err(SandboxFailure::Unavailable(_))));
}
