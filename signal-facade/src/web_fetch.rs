//! Shared bounded public HTTP/HTTPS fetcher for central Web Research.
//!
//! Fetching only connects to the pinned public address, follows same-host
//! redirects and bounds the download. The bytes are then converted to
//! Markdown: web pages and text in process under a parse budget, PDFs in the
//! resource-limited document sandbox child.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use desk_document_conversion::readable::{self, ReadableFormat};
use desk_document_conversion::sandbox;
use desk_document_conversion::{
    ConversionDetails, ConversionError, ExtractedFormat, MAX_SELECTED_PAGES, PdfExtractOptions,
    PdfPageSelection,
};
use futures_util::{Stream, StreamExt};
use sha2::{Digest, Sha256};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::seam::ToolRunOutput;
use desk_diagnose_core::web_research::{
    MAX_FETCH_RESULT_BYTES as MAX_RESULT_BYTES, ValidatedFetch, max_download_bytes,
};

const MAX_REDIRECTS: usize = 3;
const HEADER_TIMEOUT: Duration = Duration::from_secs(20);
const TEXT_BODY_TIMEOUT: Duration = Duration::from_secs(20);
const DOCUMENT_BODY_TIMEOUT: Duration = Duration::from_secs(60);
/// Conversions running at once in this process; each PDF conversion is one
/// sandbox child.
const CONVERSION_PERMITS: usize = 4;
const PERMIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Read one public page or document and return it as Markdown. At most
/// `max_fetch_bytes` of text is returned; a longer result is truncated and
/// says so.
pub async fn fetch_public_web_page(
    validated: ValidatedFetch,
    max_fetch_bytes: usize,
) -> Result<ToolRunOutput, AgentError> {
    let requested_url = validated.initial_url().as_str().to_string();
    let mut current = validated.initial_url().clone();
    for redirect_count in 0..=MAX_REDIRECTS {
        let client = pinned_public_client(&current).await?;
        let request = client
            .get(current.as_str())
            .header(
                reqwest::header::ACCEPT,
                "text/html,application/xhtml+xml,text/markdown,application/pdf,text/plain;q=0.9",
            )
            .header(
                reqwest::header::USER_AGENT,
                "lcxl-remote-desk-web-research/1",
            )
            .send();
        let response = tokio::time::timeout(HEADER_TIMEOUT, request)
            .await
            .map_err(|_| transport("Web Research request timed out"))?
            .map_err(|_| transport("Web Research request failed"))?;
        if response.status().is_redirection() {
            if redirect_count == MAX_REDIRECTS {
                return Err(invalid("Web Research redirect limit exceeded"));
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| invalid("Web Research redirect has no valid Location"))?;
            current = validated_redirect(&validated, &current, location)?;
            continue;
        }
        if !response.status().is_success() {
            return Err(
                if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || response.status().is_server_error()
                {
                    transport("Web Research endpoint returned a retryable error")
                } else {
                    invalid("Web Research endpoint rejected the request")
                },
            );
        }
        let head = ResponseHead {
            requested_url,
            final_url: current,
            content_type: response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_owned(),
            content_length: response.content_length(),
        };
        let content = page_from_response(
            head,
            response.bytes_stream(),
            max_fetch_bytes,
            &sandbox::process_runner,
            semaphore(),
        )
        .await?;
        return Ok(ToolRunOutput {
            format: desk_diagnose_core::seam::ToolOutputFormat::WebDocument,
            content,
            image_data_url: None,
            document_preview: None,
        });
    }
    unreachable!("bounded redirect loop returns or continues")
}

/// What the final response said about its body.
struct ResponseHead {
    requested_url: String,
    final_url: url::Url,
    /// The raw `Content-Type` header value.
    content_type: String,
    /// The server's declared length, reported and used only to fail early.
    content_length: Option<u64>,
}

/// The media type in lower case and its `charset` parameter.
fn media_type(header: &str) -> (String, Option<String>) {
    let mut parts = header.split(';');
    let media = parts.next().unwrap_or("").trim().to_ascii_lowercase();
    let charset = parts.find_map(|part| {
        let (name, value) = part.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"').to_owned())
    });
    (media, charset)
}

/// Download, convert and encode one response.
async fn page_from_response<S, E>(
    head: ResponseHead,
    body: S,
    max_fetch_bytes: usize,
    runner: &'static sandbox::Runner,
    permits: Arc<Semaphore>,
) -> Result<String, AgentError>
where
    S: Stream<Item = Result<bytes::Bytes, E>> + Unpin,
{
    let (media, charset) = media_type(&head.content_type);
    let format = readable::readable_format(&media)
        .ok_or_else(|| invalid("Web Research response is not a supported page or document"))?;
    let document = format == ReadableFormat::Pdf;
    let limit = max_download_bytes(&media);
    if document
        && head
            .content_length
            .is_some_and(|length| length > limit as u64)
    {
        return Err(invalid("Web Research document exceeds the download limit"));
    }
    let timeout = if document {
        DOCUMENT_BODY_TIMEOUT
    } else {
        TEXT_BODY_TIMEOUT
    };
    let body = tokio::time::timeout(timeout, read_body(body, limit))
        .await
        .map_err(|_| invalid("Web Research response body was not read in time"))??;
    if document && body.truncated {
        return Err(invalid("Web Research document exceeds the download limit"));
    }
    let permit = conversion_permit(permits).await?;
    let final_url = head.final_url.clone();
    let source = Arc::new(body.bytes);
    let converted_source = Arc::clone(&source);
    let downloaded_truncated = body.truncated;
    // The permit moves into the blocking task, so it is held until the
    // conversion ends even when this request is cancelled.
    let converted = tokio::task::spawn_blocking(move || {
        let _permit: OwnedSemaphorePermit = permit;
        convert(
            &converted_source,
            format,
            charset.as_deref(),
            &final_url,
            downloaded_truncated,
            max_fetch_bytes,
            runner,
        )
    })
    .await
    .map_err(|_| internal("Web Research conversion failed"))?
    .map_err(conversion_error)?;
    page_result(PageInput {
        requested_url: head.requested_url,
        final_url: head.final_url.to_string(),
        fetched_at: chrono::Utc::now().to_rfc3339(),
        content_type: media,
        content_length: head.content_length,
        sha256: format!("{:x}", Sha256::digest(source.as_slice())),
        body_bytes: source.len(),
        truncated: downloaded_truncated || converted.truncated,
        converted,
    })
}

fn semaphore() -> Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(SEMAPHORE.get_or_init(|| Arc::new(Semaphore::new(CONVERSION_PERMITS))))
}

async fn conversion_permit(permits: Arc<Semaphore>) -> Result<OwnedSemaphorePermit, AgentError> {
    tokio::time::timeout(PERMIT_TIMEOUT, permits.acquire_owned())
        .await
        .map_err(|_| {
            error(
                AgentErrorKind::Timeout,
                "Web Research is busy converting other documents; try again later",
                false,
            )
        })?
        .map_err(|_| internal("Web Research conversion is unavailable"))
}

/// Converted text and the facts reported next to it.
struct Converted {
    markdown: String,
    title: Option<String>,
    published_at: Option<String>,
    truncated: bool,
    /// Source and converted page counts of a PDF.
    pages: Option<(u32, u32)>,
}

fn convert(
    source: &[u8],
    format: ReadableFormat,
    charset: Option<&str>,
    final_url: &url::Url,
    downloaded_truncated: bool,
    max_fetch_bytes: usize,
    runner: &sandbox::Runner,
) -> Result<Converted, ConversionError> {
    if format != ReadableFormat::Pdf {
        let document = readable::to_markdown(
            source,
            format,
            charset,
            final_url,
            downloaded_truncated,
            max_fetch_bytes,
        )?;
        return Ok(Converted {
            markdown: document.markdown,
            title: document.title,
            published_at: document.published_at,
            truncated: document.truncated,
            pages: None,
        });
    }
    let options = PdfExtractOptions {
        kind: ExtractedFormat::Markdown,
        pages: PdfPageSelection::Leading {
            max_pages: MAX_SELECTED_PAGES as u32,
        },
        page_markers: None,
        max_output_bytes: max_fetch_bytes,
    };
    let extracted = sandbox::extract_pdf(source, &options, runner)?;
    let ConversionDetails::PdfExtraction {
        source_pages,
        converted_pages,
        ..
    } = extracted.details
    else {
        return Err(ConversionError::new(
            "document_sandbox_unavailable",
            "the document sandbox returned an invalid result",
        ));
    };
    Ok(Converted {
        markdown: extracted.text,
        title: extracted.title,
        published_at: extracted.published_at,
        truncated: extracted.truncated,
        pages: Some((source_pages, converted_pages)),
    })
}

fn conversion_error(error: ConversionError) -> AgentError {
    if error.code == "document_sandbox_unavailable" {
        // A deployment problem: the host cannot start its own executable.
        internal(
            "Web Research could not start the document sandbox; the server must be able to run its own executable",
        )
    } else {
        invalid(format!(
            "Web Research could not read the content: {}: {}",
            error.code, error.message
        ))
    }
}

/// Response bytes read up to a limit; the remainder of the stream is dropped.
struct BoundedBody {
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
}

impl BoundedBody {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            truncated: false,
        }
    }

    /// Append a chunk; false once the limit is reached and reading should stop.
    fn push(&mut self, chunk: &[u8]) -> bool {
        let room = self.limit.saturating_sub(self.bytes.len());
        if chunk.len() > room {
            self.bytes.extend_from_slice(&chunk[..room]);
            self.truncated = true;
            return false;
        }
        self.bytes.extend_from_slice(chunk);
        true
    }
}

async fn read_body<S, E>(mut stream: S, limit: usize) -> Result<BoundedBody, AgentError>
where
    S: Stream<Item = Result<bytes::Bytes, E>> + Unpin,
{
    let mut body = BoundedBody::new(limit);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| transport("Web Research response body failed"))?;
        if !body.push(&chunk) {
            break;
        }
    }
    Ok(body)
}

struct PageInput {
    requested_url: String,
    final_url: String,
    fetched_at: String,
    content_type: String,
    content_length: Option<u64>,
    /// Digest and size of the downloaded bytes, a prefix when truncated.
    sha256: String,
    body_bytes: usize,
    truncated: bool,
    converted: Converted,
}

/// Build the model-facing result, shortening the excerpt so the encoded
/// result stays within the per-call output limit.
fn page_result(input: PageInput) -> Result<String, AgentError> {
    let mut excerpt = input.converted.markdown;
    let mut truncated = input.truncated;
    loop {
        let mut result = serde_json::json!({
            "schema_version": 1,
            "untrusted_external_content": true,
            "requested_url": input.requested_url,
            "final_url": input.final_url,
            "title": input.converted.title,
            "published_at": input.converted.published_at,
            "fetched_at": input.fetched_at,
            "content_type": input.content_type,
            "body_bytes": input.body_bytes,
            "sha256": input.sha256,
            "truncated": truncated,
            "excerpt": excerpt,
        });
        if let Some(length) = input.content_length {
            result["content_length"] = serde_json::json!(length);
        }
        if let Some((source_pages, converted_pages)) = input.converted.pages {
            result["document"] = serde_json::json!({
                "source_pages": source_pages,
                "converted_pages": converted_pages,
            });
        }
        let encoded = serde_json::to_string(&result)
            .map_err(|_| internal("failed to encode Web Research result"))?;
        let over = encoded.len().saturating_sub(MAX_RESULT_BYTES);
        if over == 0 {
            return Ok(encoded);
        }
        if excerpt.is_empty() {
            return Err(invalid("Web Research result exceeds the output limit"));
        }
        // Escaping can only make the encoded excerpt longer than its text, so
        // removing the excess from the text fits within one more encoding.
        let mut end = excerpt.len().saturating_sub(over);
        while !excerpt.is_char_boundary(end) {
            end -= 1;
        }
        excerpt.truncate(end);
        truncated = true;
    }
}

fn validated_redirect(
    validated: &ValidatedFetch,
    current: &url::Url,
    location: &str,
) -> Result<url::Url, AgentError> {
    let next = current
        .join(location)
        .map_err(|_| invalid("Web Research redirect URL is invalid"))?;
    desk_diagnose_core::web_research::validate_public_url(next.as_str(), true)?;
    if !validated.same_approved_host(&next) {
        return Err(invalid(
            "Web Research redirects may not leave the approved host",
        ));
    }
    Ok(next)
}

async fn pinned_public_client(url: &url::Url) -> Result<reqwest::Client, AgentError> {
    let host = url
        .host_str()
        .ok_or_else(|| invalid("Web Research URL has no host"))?;
    let port = url.port_or_known_default().unwrap_or(443);
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| transport("Web Research DNS lookup failed"))?
        .filter(|address| {
            desk_utils::ssrf::check_transport(address.ip(), false, url.scheme() == "https", false)
                .is_ok()
        })
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err(invalid(
            "Web Research target did not resolve to a permitted public address",
        ));
    }
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .resolve_to_addrs(host, &addresses)
        .build()
        .map_err(|_| internal("Web Research client is unavailable"))
}

fn error(kind: AgentErrorKind, message: impl Into<String>, retryable: bool) -> AgentError {
    AgentError {
        kind,
        message: message.into(),
        retryable,
        safe_for_model: true,
        error_code: None,
    }
}

fn invalid(message: impl Into<String>) -> AgentError {
    error(AgentErrorKind::InvalidInput, message, false)
}

fn transport(message: impl Into<String>) -> AgentError {
    error(AgentErrorKind::TransportError, message, true)
}

fn internal(message: impl Into<String>) -> AgentError {
    error(AgentErrorKind::Internal, message, false)
}

#[cfg(test)]
mod tests;
