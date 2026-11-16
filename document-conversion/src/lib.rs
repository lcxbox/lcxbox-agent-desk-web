//! Bounded, network-free document conversion primitives.
//!
//! This crate deliberately has no filesystem API. Callers resolve and authorize
//! file references, pass one immutable byte snapshot, and publish returned bytes
//! through the existing create-new artifact path. Work with unbounded paths on
//! hostile input (PDF parsing and Typst evaluation) runs only in the document
//! sandbox child; the child-process runner comes from `desk-utils`. Typst
//! generation and preview need the `typst` feature.

#[cfg(feature = "typst")]
mod markdown;
mod pdf_extract;
mod pdf_preflight;
pub mod readable;
pub mod sandbox;
#[cfg(feature = "typst")]
mod typst_engine;

pub use pdf_extract::ExtractedPdf;
pub use readable::{clamp_published_at, clamp_title};

use serde::{Deserialize, Serialize};

pub const MAX_SOURCE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_TEXT_SOURCE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_SOURCE_PAGES: u32 = 500;
pub const MAX_SELECTED_PAGES: usize = 200;
pub const MAX_PAGE_RANGES: usize = 64;
pub const MAX_GENERATED_PAGES: usize = 500;
pub const MAX_PREVIEW_BYTES: usize = 400_000;

pub const TEMPLATE_VERSION: &str = "standard/v1";
pub const DIRECT_TYPST_TEMPLATE_VERSION: &str = "source/v1";
pub const ENGINE_VERSION: &str = "typst/0.14.2";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFormat {
    Pdf,
    Markdown,
    Text,
    Typst,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversionKind {
    PdfToMarkdown,
    PdfToText,
    MarkdownToPdf,
    TextToPdf,
    TypstToPdf,
}

impl ConversionKind {
    pub fn source_format(self) -> SourceFormat {
        match self {
            Self::PdfToMarkdown | Self::PdfToText => SourceFormat::Pdf,
            Self::MarkdownToPdf => SourceFormat::Markdown,
            Self::TextToPdf => SourceFormat::Text,
            Self::TypstToPdf => SourceFormat::Typst,
        }
    }

    pub fn media_type(self) -> &'static str {
        match self {
            Self::PdfToMarkdown => "text/markdown;charset=utf-8",
            Self::PdfToText => "text/plain;charset=utf-8",
            Self::MarkdownToPdf | Self::TextToPdf | Self::TypstToPdf => "application/pdf",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageMarkerStyle {
    None,
    HtmlComment,
    PlainText,
}

/// Text format of a PDF extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractedFormat {
    Markdown,
    Text,
}

impl ExtractedFormat {
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Markdown => "text/markdown;charset=utf-8",
            Self::Text => "text/plain;charset=utf-8",
        }
    }
}

/// Which PDF pages an extraction covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum PdfPageSelection {
    /// Exactly these pages (all pages when empty). More than
    /// [`MAX_SELECTED_PAGES`] pages, or output over the budget, fails.
    Exact { pages: Vec<PageRange> },
    /// The first `max_pages` pages; output over the budget is truncated.
    Leading { max_pages: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfExtractOptions {
    pub kind: ExtractedFormat,
    pub pages: PdfPageSelection,
    pub page_markers: Option<PageMarkerStyle>,
    pub max_output_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageRange {
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversionWarningCode {
    BlankPage,
    ScannedPage,
    FontDecodeIssue,
    TableDegraded,
    LinkDegraded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversionWarning {
    pub code: ConversionWarningCode,
    pub page: Option<u32>,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConversionDetails {
    PdfExtraction {
        source_pages: u32,
        selected_page_ranges: Vec<PageRange>,
        converted_pages: u32,
    },
    PdfGeneration {
        output_pages: u32,
        template_version: String,
        font_set_sha256: String,
        engine: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionOutput {
    pub bytes: Vec<u8>,
    pub media_type: &'static str,
    pub details: ConversionDetails,
    pub warnings: Vec<ConversionWarning>,
}

#[cfg(feature = "typst")]
#[derive(Debug, Clone)]
pub struct PreviewDocument {
    compiled: typst_engine::CompiledDocument,
    pub page_count: u32,
    pub template_version: &'static str,
    pub font_set_sha256: String,
    pub engine: &'static str,
    pub warnings: Vec<ConversionWarning>,
}

#[cfg(feature = "typst")]
impl PreviewDocument {
    pub fn render_page(&self, page: u32) -> Result<RenderedPage, ConversionError> {
        typst_engine::render_page(&self.compiled, page)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedPage {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub pixels_per_point_milli: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionError {
    pub code: &'static str,
    pub message: String,
}

impl ConversionError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ConversionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ConversionError {}

/// Generates a PDF from Markdown, TXT or Typst. Run it only in the document
/// sandbox child: Typst source is a program.
#[cfg(feature = "typst")]
pub fn generate_pdf(
    source: &[u8],
    kind: ConversionKind,
) -> Result<ConversionOutput, ConversionError> {
    validate_source_size(source, kind.source_format())?;
    if kind.source_format() == SourceFormat::Pdf {
        return Err(ConversionError::new(
            "invalid_conversion_options",
            "PDF extraction is not a PDF generation",
        ));
    }
    let text = decode_utf8(source)?;
    let compiled = typst_engine::compile(text, kind.source_format())?;
    let page_count = checked_generated_pages(&compiled)?;
    let font_set_sha256 = compiled.font_set_sha256.clone();
    let warnings = compiled.warnings.clone();
    let bytes = typst_engine::export_pdf(&compiled)?;
    if bytes.len() > MAX_OUTPUT_BYTES {
        return Err(ConversionError::new(
            "output_too_large",
            "generated PDF exceeds the 32 MiB output limit",
        ));
    }
    Ok(ConversionOutput {
        bytes,
        media_type: kind.media_type(),
        details: ConversionDetails::PdfGeneration {
            output_pages: page_count,
            template_version: if kind == ConversionKind::TypstToPdf {
                DIRECT_TYPST_TEMPLATE_VERSION
            } else {
                TEMPLATE_VERSION
            }
            .into(),
            font_set_sha256,
            engine: ENGINE_VERSION.into(),
        },
        warnings,
    })
}

#[cfg(feature = "typst")]
pub fn preview(source: &[u8], format: SourceFormat) -> Result<PreviewDocument, ConversionError> {
    if !matches!(format, SourceFormat::Markdown | SourceFormat::Text) {
        return Err(ConversionError::new(
            "unsupported_source_format",
            "only Markdown and TXT can be previewed",
        ));
    }
    validate_source_size(source, format)?;
    let text = decode_utf8(source)?;
    let compiled = typst_engine::compile(text, format)?;
    let page_count = checked_generated_pages(&compiled)?;
    Ok(PreviewDocument {
        page_count,
        template_version: TEMPLATE_VERSION,
        font_set_sha256: compiled.font_set_sha256.clone(),
        engine: ENGINE_VERSION,
        warnings: compiled.warnings.clone(),
        compiled,
    })
}

#[cfg(feature = "typst")]
fn validate_source_size(source: &[u8], format: SourceFormat) -> Result<(), ConversionError> {
    let limit = match format {
        SourceFormat::Pdf => MAX_SOURCE_BYTES,
        SourceFormat::Markdown | SourceFormat::Text | SourceFormat::Typst => MAX_TEXT_SOURCE_BYTES,
    };
    if source.len() > limit {
        return Err(ConversionError::new(
            "source_too_large",
            format!(
                "source is {} bytes, exceeding the {limit} byte limit",
                source.len()
            ),
        ));
    }
    Ok(())
}

#[cfg(feature = "typst")]
fn decode_utf8(source: &[u8]) -> Result<&str, ConversionError> {
    std::str::from_utf8(source).map_err(|_| {
        ConversionError::new(
            "invalid_source_encoding",
            "Markdown, TXT, and Typst sources must be valid UTF-8",
        )
    })
}

#[cfg(feature = "typst")]
fn checked_generated_pages(
    compiled: &typst_engine::CompiledDocument,
) -> Result<u32, ConversionError> {
    let page_count = compiled.document.pages.len();
    if page_count == 0 || page_count > MAX_GENERATED_PAGES {
        return Err(ConversionError::new(
            "page_limit_exceeded",
            format!(
                "generated document has {page_count} pages; expected 1..={MAX_GENERATED_PAGES}"
            ),
        ));
    }
    Ok(page_count as u32)
}

#[cfg(all(test, feature = "typst"))]
mod tests {
    use super::*;

    #[test]
    fn source_limits_are_format_specific() {
        assert!(preview(&vec![b'a'; MAX_TEXT_SOURCE_BYTES + 1], SourceFormat::Text).is_err());
    }

    #[test]
    fn pdf_extraction_kinds_are_not_generated() {
        let error = generate_pdf(b"%PDF-1.7", ConversionKind::PdfToText).unwrap_err();
        assert_eq!(error.code, "invalid_conversion_options");
    }

    #[test]
    fn converts_typst_source_without_applying_markdown_template() {
        let output = generate_pdf(
            b"#set page(width: 40pt, height: 40pt, margin: 0pt)\nHello",
            ConversionKind::TypstToPdf,
        )
        .unwrap();
        assert!(output.bytes.starts_with(b"%PDF-"));
        assert_eq!(output.media_type, "application/pdf");
        assert!(matches!(
            output.details,
            ConversionDetails::PdfGeneration { ref template_version, .. }
                if template_version == DIRECT_TYPST_TEMPLATE_VERSION
        ));
    }

    #[test]
    fn typst_conversion_rejects_external_files() {
        let missing_file =
            generate_pdf(b"#include \"secret.typ\"", ConversionKind::TypstToPdf).unwrap_err();
        assert_eq!(missing_file.code, "document_compile_failed");
        let unavailable_package = generate_pdf(
            b"#import \"@preview/example:1.0.0\"",
            ConversionKind::TypstToPdf,
        )
        .unwrap_err();
        assert_eq!(unavailable_package.code, "document_compile_failed");
    }
}
