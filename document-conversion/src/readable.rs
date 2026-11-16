//! Converts fetched web content (HTML, Markdown, plain text) into Markdown.
//!
//! Bytes are decoded with the declared or sniffed charset, HTML is parsed
//! under a fixed node/depth budget and rendered with `htmd`, and the result
//! is cut to the caller's output budget. PDF is recognized by
//! [`readable_format`] but is converted elsewhere (sandboxed extraction).

mod charset;
mod html;

use crate::{ConversionError, MAX_TEXT_SOURCE_BYTES};

/// Media types accepted by [`readable_format`], without parameters.
pub const READABLE_MEDIA_TYPES: [&str; 5] = [
    "text/html",
    "application/xhtml+xml",
    "text/plain",
    "text/markdown",
    "application/pdf",
];

const MAX_TITLE_CHARS: usize = 2_000;
const MAX_PUBLISHED_AT_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadableFormat {
    Html,
    Pdf,
    Markdown,
    Text,
}

/// Content limits that were applied while producing a [`ReadableDocument`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadableNotice {
    /// HTML parsing stopped because the node or depth budget was reached.
    ParseBudgetExceeded,
    /// Subtrees nested deeper than the depth limit were removed.
    DepthPruned,
    /// Children beyond the per-element sibling limit were removed.
    SiblingsTruncated,
    /// The Markdown was cut at the output byte limit.
    OutputTruncated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadableDocument {
    pub markdown: String,
    /// True when content is missing: the source was cut by the download limit,
    /// a parse or tree budget dropped content, or the output was cut.
    pub truncated: bool,
    pub title: Option<String>,
    pub published_at: Option<String>,
    pub notices: Vec<ReadableNotice>,
}

/// Maps a `Content-Type` value to a readable format, ignoring parameters and case.
pub fn readable_format(media_type: &str) -> Option<ReadableFormat> {
    let essence = media_type.split(';').next().unwrap_or_default().trim();
    let format = if essence.eq_ignore_ascii_case("text/html")
        || essence.eq_ignore_ascii_case("application/xhtml+xml")
    {
        ReadableFormat::Html
    } else if essence.eq_ignore_ascii_case("text/plain") {
        ReadableFormat::Text
    } else if essence.eq_ignore_ascii_case("text/markdown") {
        ReadableFormat::Markdown
    } else if essence.eq_ignore_ascii_case("application/pdf") {
        ReadableFormat::Pdf
    } else {
        return None;
    };
    Some(format)
}

/// Converts HTML, Markdown or plain text to Markdown.
///
/// `charset` is the `Content-Type` charset parameter, `base_url` the final URL
/// after redirects (HTML links are made absolute against it or `<base href>`),
/// and `truncated_source` tells the decoder that the bytes were cut by the
/// download limit, so an incomplete trailing character is dropped.
pub fn to_markdown(
    source: &[u8],
    format: ReadableFormat,
    charset: Option<&str>,
    base_url: &url::Url,
    truncated_source: bool,
    max_output_bytes: usize,
) -> Result<ReadableDocument, ConversionError> {
    let is_html = match format {
        ReadableFormat::Pdf => {
            return Err(ConversionError::new(
                "unsupported_source_format",
                "PDF is not converted by the readable text path",
            ));
        }
        ReadableFormat::Html => true,
        ReadableFormat::Markdown | ReadableFormat::Text => false,
    };
    if source.len() > MAX_TEXT_SOURCE_BYTES {
        return Err(ConversionError::new(
            "source_too_large",
            format!("text source exceeds {MAX_TEXT_SOURCE_BYTES} bytes"),
        ));
    }
    let text = charset::decode(source, charset, is_html, truncated_source);
    let mut document = if is_html {
        html::to_markdown(&text, base_url)
    } else {
        ReadableDocument {
            markdown: if format == ReadableFormat::Text {
                trim_line_ends(&text)
            } else {
                text
            },
            truncated: false,
            title: None,
            published_at: None,
            notices: Vec::new(),
        }
    };
    document.truncated |= truncated_source;
    if truncate_on_char_boundary(&mut document.markdown, max_output_bytes) {
        document.truncated = true;
        document.notices.push(ReadableNotice::OutputTruncated);
    }
    Ok(document)
}

/// Normalizes a document title: collapses whitespace, drops empty titles and
/// keeps at most 2000 characters.
pub fn clamp_title(title: &str) -> Option<String> {
    let mut clamped = String::new();
    for (count, word) in title.split_whitespace().enumerate() {
        if count > 0 {
            clamped.push(' ');
        }
        clamped.push_str(word);
    }
    if let Some((index, _)) = clamped.char_indices().nth(MAX_TITLE_CHARS) {
        clamped.truncate(index);
        clamped.truncate(clamped.trim_end().len());
    }
    (!clamped.is_empty()).then_some(clamped)
}

/// Keeps a publication time only when it is non-empty, at most 256 bytes and
/// free of control characters.
pub fn clamp_published_at(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= MAX_PUBLISHED_AT_BYTES
        && !value.chars().any(char::is_control))
    .then(|| value.to_owned())
}

fn trim_line_ends(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            output.push('\n');
        }
        output.push_str(line.trim_end());
    }
    output
}

/// Cuts `text` to at most `max_bytes` on a UTF-8 boundary; returns whether it was cut.
fn truncate_on_char_boundary(text: &mut String, max_bytes: usize) -> bool {
    if text.len() <= max_bytes {
        return false;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    true
}

#[cfg(test)]
mod tests;
