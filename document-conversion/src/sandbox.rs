//! The `document` sandbox service and the parent-side requests that use it.
//!
//! PDF parsing has unbounded paths on hostile input (deep nesting, reference
//! cycles, decompression bombs), and Typst source is a program that can
//! allocate or loop without bound; a failed allocation or a stack overflow
//! aborts the process. Both therefore run only in a short-lived child of the
//! host executable with OS resource limits (see
//! `desk_utils::process_sandbox`), and only this protocol crosses the
//! boundary. Hosts register [`SERVICE`] at the start of `main`.

use std::io::{self, Read, Write};
use std::time::Duration;

use desk_utils::process_sandbox::{
    self, FrameLimits, MEMORY_LIMIT_ENFORCED, SandboxFailure, SandboxSpec, Service,
};
use serde::{Deserialize, Serialize};

use crate::{
    ConversionDetails, ConversionError, ConversionKind, ConversionOutput, ConversionWarning,
    ExtractedPdf, MAX_OUTPUT_BYTES, MAX_SOURCE_BYTES, PdfExtractOptions, SourceFormat,
};

pub const SERVICE_NAME: &str = "document";

/// The child-side handler hosts register with
/// `desk_utils::process_sandbox::run_child_if_requested`.
pub const SERVICE: Service = (SERVICE_NAME, serve);

/// Bounds of request and response frames: a source going in, or a generated
/// PDF, PNG or extracted text coming out.
pub const FRAMES: FrameLimits = FrameLimits {
    header_bytes: 64 * 1024,
    payload_bytes: if MAX_OUTPUT_BYTES > MAX_SOURCE_BYTES {
        MAX_OUTPUT_BYTES
    } else {
        MAX_SOURCE_BYTES
    },
};

/// One unit of sandboxed work. The payload carries the source bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum SandboxRequest {
    /// Generate a PDF from Markdown, TXT or Typst; the response payload is the
    /// PDF.
    Convert { kind: ConversionKind },
    /// Compile a preview and render one page; the response payload is a PNG.
    RenderPage { format: SourceFormat, page: u32 },
    /// Extract text from a PDF; the response payload is UTF-8 text.
    ExtractPdf { options: PdfExtractOptions },
}

/// The child's answer. Only `Failed` can describe a failure: a crash, a
/// resource limit or a malformed frame is observed by the parent instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum SandboxResponse {
    Converted {
        details: ConversionDetails,
        warnings: Vec<ConversionWarning>,
    },
    RenderedPage {
        page_count: u32,
        template_version: String,
        font_set_sha256: String,
        engine: String,
        warnings: Vec<ConversionWarning>,
        width: u32,
        height: u32,
        pixels_per_point_milli: u32,
    },
    Extracted {
        details: ConversionDetails,
        title: Option<String>,
        published_at: Option<String>,
        truncated: bool,
        warnings: Vec<ConversionWarning>,
    },
    Failed {
        code: String,
        message: String,
    },
}

impl SandboxResponse {
    fn failed(error: ConversionError) -> Self {
        Self::Failed {
            code: error.code.into(),
            message: error.message,
        }
    }
}

/// Error codes the child may report. Anything else collapses to a generic
/// compile failure, so a child cannot inject arbitrary codes.
const KNOWN_CODES: [&str; 21] = [
    "document_compile_failed",
    "document_export_failed",
    "encrypted_pdf",
    "font_unavailable",
    "incomplete_extraction",
    "invalid_conversion_options",
    "invalid_markdown",
    "invalid_pdf",
    "invalid_source_encoding",
    "markdown_limit_exceeded",
    "no_extractable_text",
    "ocr_required",
    "output_too_large",
    "page_limit_exceeded",
    "page_out_of_range",
    "preview_render_failed",
    "source_format_mismatch",
    "source_too_large",
    "unsupported_in_build",
    "unsupported_markdown_feature",
    "unsupported_source_format",
];

/// Maps a reported failure back to a [`ConversionError`] with a known code.
pub fn error_from_wire(code: &str, message: String) -> ConversionError {
    let code = KNOWN_CODES
        .iter()
        .find(|known| **known == code)
        .copied()
        .unwrap_or("document_compile_failed");
    let mut message = message;
    if message.len() > 4 * 1024 {
        let mut end = 4 * 1024;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    ConversionError::new(code, message)
}

/// Every sandbox failure happens before any output exists, so it is always a
/// definite "nothing was produced".
pub fn failure_error(failure: SandboxFailure) -> ConversionError {
    match failure {
        SandboxFailure::Unavailable(detail) => ConversionError::new(
            "document_sandbox_unavailable",
            format!("the document sandbox could not start: {detail}"),
        ),
        SandboxFailure::Exited(_) => ConversionError::new(
            "document_resource_limit_exceeded",
            "the document exceeded the conversion memory or CPU limit",
        ),
        SandboxFailure::Timeout => ConversionError::new(
            "document_conversion_timeout",
            "the document did not finish within the conversion time limit",
        ),
        SandboxFailure::Protocol(detail) => ConversionError::new(
            "document_sandbox_unavailable",
            format!("the document sandbox returned an invalid result: {detail}"),
        ),
    }
}

/// Child side: serve exactly one request.
pub fn serve(input: &mut dyn Read, output: &mut dyn Write) -> io::Result<()> {
    let (request, source): (SandboxRequest, Vec<u8>) = process_sandbox::read_frame(input, FRAMES)?;
    let (response, payload) = handle(request, &source);
    process_sandbox::write_frame(output, &response, &payload, FRAMES)
}

fn handle(request: SandboxRequest, source: &[u8]) -> (SandboxResponse, Vec<u8>) {
    let result = match request {
        SandboxRequest::Convert { kind } => generate(source, kind),
        SandboxRequest::RenderPage { format, page } => render(source, format, page),
        SandboxRequest::ExtractPdf { options } => crate::pdf_extract::extract(source, &options)
            .map(|extracted| {
                (
                    SandboxResponse::Extracted {
                        details: extracted.details,
                        title: extracted.title,
                        published_at: extracted.published_at,
                        truncated: extracted.truncated,
                        warnings: extracted.warnings,
                    },
                    extracted.text.into_bytes(),
                )
            }),
    };
    result.unwrap_or_else(|error| (SandboxResponse::failed(error), Vec::new()))
}

#[cfg(feature = "typst")]
fn generate(
    source: &[u8],
    kind: ConversionKind,
) -> Result<(SandboxResponse, Vec<u8>), ConversionError> {
    let output = crate::generate_pdf(source, kind)?;
    Ok((
        SandboxResponse::Converted {
            details: output.details,
            warnings: output.warnings,
        },
        output.bytes,
    ))
}

#[cfg(feature = "typst")]
fn render(
    source: &[u8],
    format: SourceFormat,
    page: u32,
) -> Result<(SandboxResponse, Vec<u8>), ConversionError> {
    let document = crate::preview(source, format)?;
    let rendered = document.render_page(page)?;
    Ok((
        SandboxResponse::RenderedPage {
            page_count: document.page_count,
            template_version: document.template_version.into(),
            font_set_sha256: document.font_set_sha256,
            engine: document.engine.into(),
            warnings: document.warnings,
            width: rendered.width,
            height: rendered.height,
            pixels_per_point_milli: rendered.pixels_per_point_milli,
        },
        rendered.png,
    ))
}

#[cfg(not(feature = "typst"))]
fn generate(_: &[u8], _: ConversionKind) -> Result<(SandboxResponse, Vec<u8>), ConversionError> {
    Err(unsupported_in_build())
}

#[cfg(not(feature = "typst"))]
fn render(
    _: &[u8],
    _: SourceFormat,
    _: u32,
) -> Result<(SandboxResponse, Vec<u8>), ConversionError> {
    Err(unsupported_in_build())
}

#[cfg(not(feature = "typst"))]
fn unsupported_in_build() -> ConversionError {
    ConversionError::new(
        "unsupported_in_build",
        "this executable does not include PDF generation",
    )
}

/// Runs one request out of process. [`process_runner`] is the production
/// runner; tests substitute an in-memory one.
pub type Runner = dyn Fn(&SandboxRequest, &[u8], &SandboxSpec) -> Result<(SandboxResponse, Vec<u8>), SandboxFailure>
    + Sync;

/// Runs the request in a child of the current executable.
pub fn process_runner(
    request: &SandboxRequest,
    source: &[u8],
    spec: &SandboxSpec,
) -> Result<(SandboxResponse, Vec<u8>), SandboxFailure> {
    process_sandbox::run(SERVICE_NAME, request, source, spec)
}

/// PDF parsing gets less memory and time than Typst. Where the memory limit
/// is not enforced, shorter time limits bound how long a hostile document can
/// hold memory.
pub fn pdf_spec() -> SandboxSpec {
    let (cpu_seconds, wall_seconds) = if MEMORY_LIMIT_ENFORCED {
        (15, 20)
    } else {
        (5, 8)
    };
    SandboxSpec {
        memory_bytes: 512 * 1024 * 1024,
        cpu_seconds,
        wall_clock: Duration::from_secs(wall_seconds),
        frames: FRAMES,
        env: &[],
    }
}

/// Typst compilation and rendering of approved user documents.
pub fn typst_spec() -> SandboxSpec {
    SandboxSpec {
        memory_bytes: 2 * 1024 * 1024 * 1024,
        cpu_seconds: 60,
        wall_clock: Duration::from_secs(60),
        frames: FRAMES,
        // Fewer render threads keep the reserved address space well below the
        // memory limit.
        env: &[("RAYON_NUM_THREADS", "2")],
    }
}

fn unexpected(what: &str) -> ConversionError {
    failure_error(SandboxFailure::Protocol(format!(
        "unexpected {what} response"
    )))
}

/// Extracts text from a PDF in the sandbox.
pub fn extract_pdf(
    source: &[u8],
    options: &PdfExtractOptions,
    runner: &Runner,
) -> Result<ExtractedPdf, ConversionError> {
    let request = SandboxRequest::ExtractPdf {
        options: options.clone(),
    };
    let (response, payload) = runner(&request, source, &pdf_spec()).map_err(failure_error)?;
    match response {
        SandboxResponse::Extracted {
            details: details @ ConversionDetails::PdfExtraction { .. },
            title,
            published_at,
            truncated,
            warnings,
        } if payload.len() <= options.max_output_bytes.min(MAX_OUTPUT_BYTES) => {
            let text = String::from_utf8(payload).map_err(|_| unexpected("PDF extraction"))?;
            Ok(ExtractedPdf {
                text,
                details,
                title: title.as_deref().and_then(crate::clamp_title),
                published_at: published_at.as_deref().and_then(crate::clamp_published_at),
                truncated,
                warnings,
            })
        }
        SandboxResponse::Failed { code, message } => Err(error_from_wire(&code, message)),
        _ => Err(unexpected("PDF extraction")),
    }
}

/// Generates a PDF from Markdown, TXT or Typst in the sandbox.
pub fn run_typst(
    source: &[u8],
    kind: ConversionKind,
    runner: &Runner,
) -> Result<ConversionOutput, ConversionError> {
    if kind.source_format() == SourceFormat::Pdf {
        return Err(ConversionError::new(
            "invalid_conversion_options",
            "PDF extraction is not a PDF generation",
        ));
    }
    let (response, bytes) =
        runner(&SandboxRequest::Convert { kind }, source, &typst_spec()).map_err(failure_error)?;
    match response {
        SandboxResponse::Converted { details, warnings }
            if bytes.starts_with(b"%PDF-") && bytes.len() <= MAX_OUTPUT_BYTES =>
        {
            Ok(ConversionOutput {
                bytes,
                media_type: kind.media_type(),
                details,
                warnings,
            })
        }
        SandboxResponse::Failed { code, message } => Err(error_from_wire(&code, message)),
        _ => Err(unexpected("conversion")),
    }
}

/// One rendered preview page with the facts of the compiled preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedPreviewPage {
    pub page_count: u32,
    pub template_version: String,
    pub font_set_sha256: String,
    pub engine: String,
    pub warnings: Vec<ConversionWarning>,
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub pixels_per_point_milli: u32,
}

/// Compiles a Markdown or TXT preview and renders one page in the sandbox.
pub fn render_page(
    source: &[u8],
    format: SourceFormat,
    page: u32,
    runner: &Runner,
) -> Result<RenderedPreviewPage, ConversionError> {
    let (response, png) = runner(
        &SandboxRequest::RenderPage { format, page },
        source,
        &typst_spec(),
    )
    .map_err(failure_error)?;
    match response {
        SandboxResponse::RenderedPage {
            page_count,
            template_version,
            font_set_sha256,
            engine,
            warnings,
            width,
            height,
            pixels_per_point_milli,
        } if png.starts_with(b"\x89PNG\r\n\x1a\n") => Ok(RenderedPreviewPage {
            page_count,
            template_version,
            font_set_sha256,
            engine,
            warnings,
            png,
            width,
            height,
            pixels_per_point_milli,
        }),
        SandboxResponse::Failed { code, message } => Err(error_from_wire(&code, message)),
        _ => Err(unexpected("preview")),
    }
}

/// Serves requests in process, for tests that do not exercise the child.
#[doc(hidden)]
pub fn in_process_runner(
    request: &SandboxRequest,
    source: &[u8],
    _: &SandboxSpec,
) -> Result<(SandboxResponse, Vec<u8>), SandboxFailure> {
    let protocol = |error: io::Error| SandboxFailure::Protocol(error.to_string());
    let mut input = Vec::new();
    process_sandbox::write_frame(&mut input, request, source, FRAMES).map_err(protocol)?;
    let mut output = Vec::new();
    serve(&mut input.as_slice(), &mut output).map_err(protocol)?;
    process_sandbox::read_frame(&mut output.as_slice(), FRAMES).map_err(protocol)
}

#[cfg(test)]
mod tests;
