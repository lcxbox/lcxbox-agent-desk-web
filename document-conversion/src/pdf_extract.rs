use unpdf::{
    ParseOptions,
    model::Document,
    render::{self, PageSelection, RenderOptions, TableFallback},
};

use crate::{
    ConversionDetails, ConversionError, ConversionWarning, ConversionWarningCode, ExtractedFormat,
    MAX_OUTPUT_BYTES, MAX_PAGE_RANGES, MAX_SELECTED_PAGES, MAX_SOURCE_PAGES, PageMarkerStyle,
    PageRange, PdfExtractOptions, PdfPageSelection,
};

/// Text extracted from a PDF, with the document facts reported alongside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedPdf {
    pub text: String,
    pub details: ConversionDetails,
    pub title: Option<String>,
    pub published_at: Option<String>,
    /// The output budget stopped extraction before the selected pages ended.
    pub truncated: bool,
    pub warnings: Vec<ConversionWarning>,
}

/// Extracts text from a PDF. The parser has unbounded paths on hostile
/// input, so this only runs inside the document sandbox child.
pub(crate) fn extract(
    source: &[u8],
    options: &PdfExtractOptions,
) -> Result<ExtractedPdf, ConversionError> {
    if matches!(options.kind, ExtractedFormat::Text)
        && matches!(options.page_markers, Some(PageMarkerStyle::HtmlComment))
    {
        return Err(ConversionError::new(
            "invalid_conversion_options",
            "HTML comment page markers are not valid for plain text output",
        ));
    }
    let max_output_bytes = options.max_output_bytes.min(MAX_OUTPUT_BYTES);
    crate::pdf_preflight::validate(source)?;
    // Owner-password-only documents are decrypted with the empty user
    // password; a document that needs a password fails to parse.
    let parse_options = ParseOptions::new().text_only().sequential();
    let document =
        unpdf::parse_bytes_with_options(source, parse_options).map_err(map_unpdf_error)?;
    let source_pages = document.page_count();
    if source_pages == 0 || source_pages > MAX_SOURCE_PAGES {
        return Err(ConversionError::new(
            "page_limit_exceeded",
            format!("PDF has {source_pages} pages; expected 1..={MAX_SOURCE_PAGES}"),
        ));
    }
    if document.extraction_quality.pages_incomplete
        || document.extraction_quality.unresolved_page_nodes > 0
    {
        return Err(ConversionError::new(
            "incomplete_extraction",
            "the PDF page tree is damaged and extraction would omit pages",
        ));
    }
    let selected_pages = match &options.pages {
        PdfPageSelection::Exact { pages } => normalize_ranges(pages, source_pages)?.1,
        PdfPageSelection::Leading { max_pages } => {
            let max_pages = (*max_pages).min(MAX_SELECTED_PAGES as u32);
            if max_pages == 0 {
                return Err(ConversionError::new(
                    "invalid_conversion_options",
                    "at least one leading page must be selected",
                ));
            }
            (1..=source_pages.min(max_pages)).collect()
        }
    };
    let leading = matches!(options.pages, PdfPageSelection::Leading { .. });

    let marker = options.page_markers.unwrap_or(PageMarkerStyle::None);
    let mut output = String::new();
    let mut converted = Vec::with_capacity(selected_pages.len());
    let mut has_text = false;
    let mut truncated = false;
    for page_number in &selected_pages {
        let page = document
            .get_page(*page_number)
            .ok_or_else(|| {
                ConversionError::new("page_out_of_range", "selected PDF page is missing")
            })?
            .clone();
        let mut one = Document::new();
        one.metadata = document.metadata.clone();
        one.add_page(page);
        let render_options = RenderOptions::new()
            .with_table_fallback(TableFallback::Ascii)
            .with_pages(PageSelection::All);
        let rendered = match options.kind {
            ExtractedFormat::Markdown => render::to_markdown(&one, &render_options),
            ExtractedFormat::Text => render::to_text(&one, &render_options),
        }
        .map_err(map_unpdf_error)?;
        has_text |= !rendered.trim().is_empty();
        let mut part = String::new();
        if !output.is_empty() {
            part.push_str("\n\n");
        }
        match marker {
            PageMarkerStyle::None => {}
            PageMarkerStyle::HtmlComment => {
                part.push_str(&format!("<!-- lcxbox-page: {page_number} -->\n\n"));
            }
            PageMarkerStyle::PlainText => {
                part.push_str(&format!("--- page {page_number} ---\n\n"));
            }
        }
        part.push_str(rendered.trim());
        if output.len() + part.len() > max_output_bytes {
            if !leading {
                return Err(ConversionError::new(
                    "output_too_large",
                    format!("extracted document exceeds the {max_output_bytes} byte output limit"),
                ));
            }
            let mut end = max_output_bytes - output.len();
            while !part.is_char_boundary(end) {
                end -= 1;
            }
            output.push_str(&part[..end]);
            converted.push(*page_number);
            truncated = true;
            break;
        }
        output.push_str(&part);
        converted.push(*page_number);
    }

    let mut warnings = page_warnings(&document, &converted);
    if !has_text {
        if warnings
            .iter()
            .any(|warning| warning.code == ConversionWarningCode::ScannedPage)
        {
            return Err(ConversionError::new(
                "ocr_required",
                "the selected PDF pages are image-only and require OCR",
            ));
        }
        return Err(ConversionError::new(
            "no_extractable_text",
            "the selected PDF pages contain no extractable text",
        ));
    }
    if document.extraction_quality.suppressed_text_runs > 0
        || document.extraction_quality.replacement_char_count > 0
    {
        warnings.push(ConversionWarning {
            code: ConversionWarningCode::FontDecodeIssue,
            page: None,
            detail: format!(
                "PDF extraction dropped {} unreadable text run(s) and produced {} replacement character(s)",
                document.extraction_quality.suppressed_text_runs,
                document.extraction_quality.replacement_char_count
            ),
        });
    }
    warnings.truncate(64);
    Ok(ExtractedPdf {
        text: output,
        details: ConversionDetails::PdfExtraction {
            source_pages,
            selected_page_ranges: ranges_of(&converted),
            converted_pages: converted.len() as u32,
        },
        title: document
            .metadata
            .title
            .as_deref()
            .and_then(crate::clamp_title),
        published_at: document
            .metadata
            .created
            .and_then(|created| crate::clamp_published_at(&created.to_rfc3339())),
        truncated,
        warnings,
    })
}

/// Contiguous ranges covering `pages`, which are sorted and distinct.
fn ranges_of(pages: &[u32]) -> Vec<PageRange> {
    let mut ranges: Vec<PageRange> = Vec::new();
    for page in pages {
        match ranges.last_mut() {
            Some(last) if last.end + 1 == *page => last.end = *page,
            _ => ranges.push(PageRange {
                start: *page,
                end: *page,
            }),
        }
    }
    ranges
}

fn normalize_ranges(
    requested: &[PageRange],
    page_count: u32,
) -> Result<(Vec<PageRange>, Vec<u32>), ConversionError> {
    if requested.len() > MAX_PAGE_RANGES {
        return Err(ConversionError::new(
            "page_limit_exceeded",
            format!("at most {MAX_PAGE_RANGES} page ranges are supported"),
        ));
    }
    let mut ranges = if requested.is_empty() {
        vec![PageRange {
            start: 1,
            end: page_count,
        }]
    } else {
        requested.to_vec()
    };
    if ranges
        .iter()
        .any(|range| range.start == 0 || range.start > range.end || range.end > page_count)
    {
        return Err(ConversionError::new(
            "page_out_of_range",
            format!("page ranges must fall inside 1..={page_count}"),
        ));
    }
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut normalized: Vec<PageRange> = Vec::new();
    for range in ranges {
        if let Some(last) = normalized.last_mut()
            && range.start <= last.end.saturating_add(1)
        {
            last.end = last.end.max(range.end);
        } else {
            normalized.push(range);
        }
    }
    let selected = normalized
        .iter()
        .flat_map(|range| range.start..=range.end)
        .collect::<Vec<_>>();
    if selected.len() > MAX_SELECTED_PAGES {
        return Err(ConversionError::new(
            "page_limit_exceeded",
            format!("at most {MAX_SELECTED_PAGES} PDF pages may be converted at once"),
        ));
    }
    Ok((normalized, selected))
}

fn page_warnings(document: &Document, selected: &[u32]) -> Vec<ConversionWarning> {
    let mut warnings = Vec::new();
    for page_number in selected {
        let Some(page) = document.get_page(*page_number) else {
            continue;
        };
        if page.text_op_count == 0 && page.image_op_count > 0 {
            warnings.push(ConversionWarning {
                code: ConversionWarningCode::ScannedPage,
                page: Some(*page_number),
                detail: "page is image-only; OCR is not included in this conversion".into(),
            });
        } else if page.text_op_count == 0 && page.image_op_count == 0 {
            warnings.push(ConversionWarning {
                code: ConversionWarningCode::BlankPage,
                page: Some(*page_number),
                detail: "page has no text or image drawing operations".into(),
            });
        }
    }
    warnings
}

fn map_unpdf_error(error: unpdf::Error) -> ConversionError {
    let text = error.to_string();
    let lower = text.to_ascii_lowercase();
    if lower.contains("encrypt") || lower.contains("password") {
        ConversionError::new("encrypted_pdf", "encrypted PDFs are not supported")
    } else {
        ConversionError::new("invalid_pdf", format!("PDF extraction failed: {text}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_are_sorted_and_merged() {
        let (ranges, pages) = normalize_ranges(
            &[
                PageRange { start: 4, end: 5 },
                PageRange { start: 1, end: 2 },
                PageRange { start: 3, end: 3 },
            ],
            10,
        )
        .unwrap();
        assert_eq!(ranges, vec![PageRange { start: 1, end: 5 }]);
        assert_eq!(pages, vec![1, 2, 3, 4, 5]);
    }
}
