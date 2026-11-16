//! Runs PDF extraction in real sandbox children of this test binary.
//!
//! Fixtures: `chrome_print.pdf` was printed by headless Chrome and
//! `macos_quartz.pdf` by macOS `cupsfilter`; the `*_password_*` files are
//! that Chrome output encrypted by pypdf. `indirect_length.pdf`,
//! `filter_array.pdf` and `lzw_ascii85_image.pdf` are hand-written per the
//! PDF specification to carry those traits of other generators. `hostile_*`
//! files reproduce parser failures that only the sandbox limits contain.
use std::path::Path;
use std::time::{Duration, Instant};

use desk_document_conversion::sandbox::{self, SERVICE};
use desk_document_conversion::{
    ConversionDetails, ExtractedFormat, PageMarkerStyle, PdfExtractOptions, PdfPageSelection,
};
use desk_utils::process_sandbox;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
}

fn leading() -> PdfExtractOptions {
    PdfExtractOptions {
        kind: ExtractedFormat::Markdown,
        pages: PdfPageSelection::Leading { max_pages: 200 },
        page_markers: None,
        max_output_bytes: 128 * 1024,
    }
}

fn extract(
    name: &str,
) -> (
    Result<desk_document_conversion::ExtractedPdf, String>,
    Duration,
) {
    let started = Instant::now();
    let result = sandbox::extract_pdf(&fixture(name), &leading(), &sandbox::process_runner)
        .map_err(|error| error.code.to_owned());
    (result, started.elapsed())
}

/// Text without whitespace: some generators position every glyph, and the
/// extracted text then has spaces between letters.
fn compact(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

fn main() {
    if let Some(code) = process_sandbox::run_child_if_requested(&[SERVICE]) {
        std::process::exit(code);
    }
    let checks: &[(&str, fn())] = &[
        (
            "generator_samples_extract_within_limits",
            generator_samples_extract_within_limits,
        ),
        (
            "only_password_free_encryption_is_read",
            only_password_free_encryption_is_read,
        ),
        (
            "exact_pages_keep_markers_in_the_child",
            exact_pages_keep_markers_in_the_child,
        ),
        ("hostile_pdfs_are_contained", hostile_pdfs_are_contained),
        ("generation_matches_the_build", generation_matches_the_build),
        (
            "a_long_document_fits_the_limits",
            a_long_document_fits_the_limits,
        ),
    ];
    for (name, check) in checks {
        check();
        println!("test {name} ... ok");
    }
    println!("test result: ok. {} passed", checks.len());
}

fn generator_samples_extract_within_limits() {
    let wall = sandbox::pdf_spec().wall_clock;
    for (name, expected) in [
        ("chrome_print.pdf", "Chrome print sample"),
        ("macos_quartz.pdf", "Quartz text sample"),
        ("indirect_length.pdf", "Indirect length sample"),
        ("filter_array.pdf", "Filter array sample"),
        ("lzw_ascii85_image.pdf", "LZW ASCII85 image sample"),
    ] {
        let (result, elapsed) = extract(name);
        let extracted = result.unwrap_or_else(|code| panic!("{name}: {code}"));
        assert!(
            compact(&extracted.text).contains(&compact(expected)),
            "{name}: {}",
            extracted.text
        );
        assert!(elapsed < wall, "{name} took {elapsed:?}");
        println!("  {name}: {elapsed:?}");
    }
}

fn only_password_free_encryption_is_read() {
    for name in ["owner_password_rc4.pdf", "owner_password_aes128.pdf"] {
        let (result, _) = extract(name);
        let extracted = result.unwrap_or_else(|code| panic!("{name}: {code}"));
        assert!(
            compact(&extracted.text).contains("Chromeprintsample"),
            "{name}"
        );
    }
    for name in ["user_password_aes128.pdf", "owner_password_aes256.pdf"] {
        assert_eq!(extract(name).0.unwrap_err(), "encrypted_pdf", "{name}");
    }
}

fn exact_pages_keep_markers_in_the_child() {
    let options = PdfExtractOptions {
        kind: ExtractedFormat::Text,
        pages: PdfPageSelection::Exact { pages: vec![] },
        page_markers: Some(PageMarkerStyle::PlainText),
        max_output_bytes: 32 * 1024 * 1024,
    };
    let extracted = sandbox::extract_pdf(
        &fixture("macos_quartz.pdf"),
        &options,
        &sandbox::process_runner,
    )
    .unwrap();
    assert!(extracted.text.starts_with("--- page 1 ---"));
    assert!(matches!(
        extracted.details,
        ConversionDetails::PdfExtraction {
            converted_pages: 1,
            ..
        }
    ));
}

fn hostile_pdfs_are_contained() {
    let wall = sandbox::pdf_spec().wall_clock;
    for name in [
        "hostile_deep_arrays.pdf",
        "hostile_prev_loop.pdf",
        "hostile_flate_bomb.pdf",
        "hostile_repeated_contents.pdf",
        "hostile_bad_encryption.pdf",
    ] {
        let (result, elapsed) = extract(name);
        println!(
            "  {name}: {:?} in {elapsed:?}",
            result.as_ref().map(|_| "text")
        );
        assert!(
            elapsed < wall + Duration::from_secs(2),
            "{name} took {elapsed:?}"
        );
        if let Err(code) = &result {
            assert!(
                [
                    "document_resource_limit_exceeded",
                    "document_conversion_timeout",
                    "invalid_pdf",
                    "encrypted_pdf",
                    "no_extractable_text",
                    "incomplete_extraction",
                ]
                .contains(&code.as_str()),
                "{name}: {code}"
            );
        }
        // The parent keeps serving ordinary documents.
        assert!(extract("filter_array.pdf").0.is_ok());
    }
}

fn generation_matches_the_build() {
    let result = sandbox::run_typst(
        b"Hello",
        desk_document_conversion::ConversionKind::TextToPdf,
        &sandbox::process_runner,
    );
    if cfg!(feature = "typst") {
        assert!(result.unwrap().bytes.starts_with(b"%PDF-"));
    } else {
        assert_eq!(result.unwrap_err().code, "unsupported_in_build");
    }
}

/// 200 text-dense pages, the most a fetch reads, finish well inside the time
/// limits of every platform in an optimized build (hosts ship release builds;
/// unoptimized parsing is several times slower).
fn a_long_document_fits_the_limits() {
    if !cfg!(feature = "typst") || cfg!(debug_assertions) {
        return;
    }
    let paragraph = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(40);
    let source = (1..=200)
        .map(|page| format!("= Section {page}\n{paragraph}\n{paragraph}"))
        .collect::<Vec<_>>()
        .join("\n#pagebreak()\n");
    let pdf = sandbox::run_typst(
        source.as_bytes(),
        desk_document_conversion::ConversionKind::TypstToPdf,
        &sandbox::process_runner,
    )
    .unwrap()
    .bytes;
    let options = PdfExtractOptions {
        max_output_bytes: 1024 * 1024,
        ..leading()
    };
    let started = Instant::now();
    let extracted = sandbox::extract_pdf(&pdf, &options, &sandbox::process_runner).unwrap();
    let elapsed = started.elapsed();
    println!(
        "  200 pages, {} source bytes: {elapsed:?}, {} text bytes",
        pdf.len(),
        extracted.text.len()
    );
    assert!(elapsed < sandbox::pdf_spec().wall_clock / 2, "{elapsed:?}");
}
