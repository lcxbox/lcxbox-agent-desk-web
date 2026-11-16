use super::*;
use desk_document_conversion::sandbox::{SandboxRequest, SandboxResponse};
use desk_utils::process_sandbox::{SandboxFailure, SandboxSpec};
use futures_util::stream;

type Chunk = Result<bytes::Bytes, std::io::Error>;

fn head(content_type: &str, content_length: Option<u64>) -> ResponseHead {
    ResponseHead {
        requested_url: "https://example.com/docs".into(),
        final_url: url::Url::parse("https://example.com/docs/page").unwrap(),
        content_type: content_type.into(),
        content_length,
    }
}

fn chunks(parts: Vec<Vec<u8>>) -> impl Stream<Item = Chunk> + Unpin {
    stream::iter(parts.into_iter().map(|part| Ok(bytes::Bytes::from(part))))
}

fn permits() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(CONVERSION_PERMITS))
}

fn unused_runner(
    _: &SandboxRequest,
    _: &[u8],
    _: &SandboxSpec,
) -> Result<(SandboxResponse, Vec<u8>), SandboxFailure> {
    panic!("text never reaches the document sandbox")
}

async fn read_page(
    content_type: &str,
    parts: Vec<Vec<u8>>,
    max_fetch_bytes: usize,
    runner: &'static sandbox::Runner,
) -> Result<serde_json::Value, AgentError> {
    let encoded = page_from_response(
        head(content_type, None),
        chunks(parts),
        max_fetch_bytes,
        runner,
        permits(),
    )
    .await?;
    assert!(encoded.len() <= MAX_RESULT_BYTES);
    // Every result passes the validator both hosts apply before retention
    // and model use.
    desk_diagnose_core::provider_preflight::read::limits::validate_output(
        &desk_diagnose_core::ai_assistant::ai_assistant_provider_registry(),
        &desk_diagnose_core::chat::ToolCall {
            id: "fetch".into(),
            name: desk_diagnose_core::web_research::WEB_FETCH_TOOL_NAME.into(),
            arguments_json: r#"{"url":"https://example.com/docs"}"#.into(),
        },
        &ToolRunOutput {
            format: desk_diagnose_core::seam::ToolOutputFormat::WebDocument,
            content: encoded.clone(),
            image_data_url: None,
            document_preview: None,
        },
        &desk_agent_protocol::capability_grant::CapabilityGrantLimits {
            max_bytes_per_call: 256 * 1024,
            max_items_per_call: 1,
            max_calls: 1,
        },
    )
    .unwrap();
    Ok(serde_json::from_str(&encoded).unwrap())
}

#[test]
fn shared_constants_agree_with_the_converter() {
    assert_eq!(
        desk_diagnose_core::web_research::READABLE_MEDIA_TYPES,
        readable::READABLE_MEDIA_TYPES
    );
    assert_eq!(
        desk_diagnose_core::web_research::MAX_FETCH_DOCUMENT_PAGES,
        desk_document_conversion::MAX_SOURCE_PAGES
    );
    assert_eq!(
        media_type("Text/HTML; Charset=\"GBK\""),
        ("text/html".into(), Some("GBK".into()))
    );
}

#[tokio::test]
async fn html_becomes_markdown_with_absolute_links() {
    let html = r#"<html><head><title>Quarterly review</title></head><body><nav>Menu</nav><main><h1>Revenue</h1><p>See <a href="../report">the report</a>.</p><ul><li>One</li><li>Two</li></ul></main></body></html>"#;
    let value = read_page(
        "text/html; charset=utf-8",
        vec![html.into()],
        64 * 1024,
        &unused_runner,
    )
    .await
    .unwrap();
    let excerpt = value["excerpt"].as_str().unwrap();
    assert!(excerpt.contains("# Revenue"), "{excerpt}");
    assert!(
        excerpt.contains("(https://example.com/report)"),
        "{excerpt}"
    );
    assert!(!excerpt.contains("Menu"));
    assert_eq!(value["title"], "Quarterly review");
    assert_eq!(value["content_type"], "text/html");
    assert_eq!(value["truncated"], false);
    assert!(value.get("document").is_none());
}

#[tokio::test]
async fn declared_charsets_are_decoded() {
    // "中文网页" in GBK.
    let gbk = vec![0xD6, 0xD0, 0xCE, 0xC4, 0xCD, 0xF8, 0xD2, 0xB3];
    let value = read_page(
        "text/plain;charset=gbk",
        vec![gbk],
        64 * 1024,
        &unused_runner,
    )
    .await
    .unwrap();
    assert_eq!(value["excerpt"], "中文网页");
}

#[tokio::test]
async fn long_text_is_truncated_at_the_download_and_output_limits() {
    let line = "plain text line\n".repeat(1024);
    let parts = (0..200)
        .map(|_| line.clone().into_bytes())
        .collect::<Vec<_>>();
    let value = read_page("text/plain", parts, 1024 * 1024, &unused_runner)
        .await
        .unwrap();
    assert_eq!(
        value["body_bytes"],
        desk_diagnose_core::web_research::MAX_TEXT_DOWNLOAD_BYTES
    );
    assert_eq!(value["truncated"], true);
    // The output budget, then the result limit, shorten the excerpt.
    let short = read_page(
        "text/plain",
        vec![line.clone().into_bytes()],
        100,
        &unused_runner,
    )
    .await
    .unwrap();
    assert!(short["excerpt"].as_str().unwrap().len() <= 100);
    assert_eq!(short["truncated"], true);
    let quoted = "\"quoted\" ".repeat(60_000);
    let value = read_page(
        "text/markdown",
        vec![quoted.into_bytes()],
        1024 * 1024,
        &unused_runner,
    )
    .await
    .unwrap();
    assert_eq!(value["truncated"], true);
}

fn extracted_pdf(
    request: &SandboxRequest,
    _: &[u8],
    _: &SandboxSpec,
) -> Result<(SandboxResponse, Vec<u8>), SandboxFailure> {
    let SandboxRequest::ExtractPdf { options } = request else {
        panic!("unexpected request {request:?}");
    };
    assert_eq!(
        options.pages,
        PdfPageSelection::Leading {
            max_pages: MAX_SELECTED_PAGES as u32
        }
    );
    Ok((
        SandboxResponse::Extracted {
            details: ConversionDetails::PdfExtraction {
                source_pages: 300,
                selected_page_ranges: vec![desk_document_conversion::PageRange {
                    start: 1,
                    end: 200,
                }],
                converted_pages: 200,
            },
            title: Some("Annual report".into()),
            published_at: None,
            truncated: false,
            warnings: vec![],
        },
        b"# Annual report".to_vec(),
    ))
}

#[tokio::test]
async fn a_pdf_is_extracted_in_the_sandbox_with_its_page_counts() {
    let value = read_page(
        "application/pdf",
        vec![b"%PDF-1.7 ...".to_vec()],
        64 * 1024,
        &extracted_pdf,
    )
    .await
    .unwrap();
    assert_eq!(value["excerpt"], "# Annual report");
    assert_eq!(value["content_type"], "application/pdf");
    assert_eq!(
        value["document"],
        serde_json::json!({"source_pages": 300, "converted_pages": 200})
    );
}

#[tokio::test]
async fn oversized_documents_fail_without_conversion() {
    let limit = desk_diagnose_core::web_research::MAX_DOCUMENT_DOWNLOAD_BYTES;
    // A declared length over the limit fails before the body is read.
    let error = page_from_response(
        head("application/pdf", Some(limit as u64 + 1)),
        stream::pending::<Chunk>(),
        64 * 1024,
        &unused_runner,
        permits(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, AgentErrorKind::InvalidInput);
    // So does a streamed body that runs past it.
    let parts = (0..17).map(|_| vec![b'%'; 1024 * 1024]).collect();
    let error = page_from_response(
        head("application/pdf", None),
        chunks(parts),
        64 * 1024,
        &unused_runner,
        permits(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, AgentErrorKind::InvalidInput);
    let error = read_page(
        "application/json",
        vec![b"{}".to_vec()],
        1024,
        &unused_runner,
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, AgentErrorKind::InvalidInput);
}

#[tokio::test(start_paused = true)]
async fn a_stalled_body_times_out() {
    let error = page_from_response(
        head("text/html", None),
        stream::pending::<Chunk>(),
        1024,
        &unused_runner,
        permits(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, AgentErrorKind::InvalidInput);
    assert!(error.message.contains("in time"), "{}", error.message);
}

fn unavailable_sandbox(
    _: &SandboxRequest,
    _: &[u8],
    _: &SandboxSpec,
) -> Result<(SandboxResponse, Vec<u8>), SandboxFailure> {
    Err(SandboxFailure::Unavailable("cannot execute".into()))
}

fn encrypted(
    _: &SandboxRequest,
    _: &[u8],
    _: &SandboxSpec,
) -> Result<(SandboxResponse, Vec<u8>), SandboxFailure> {
    Ok((
        SandboxResponse::Failed {
            code: "encrypted_pdf".into(),
            message: "needs a password".into(),
        },
        Vec::new(),
    ))
}

#[tokio::test]
async fn conversion_errors_separate_deployment_problems_from_content() {
    let pdf = || vec![b"%PDF-1.7".to_vec()];
    let error = read_page("application/pdf", pdf(), 1024, &unavailable_sandbox)
        .await
        .unwrap_err();
    assert_eq!(error.kind, AgentErrorKind::Internal);
    assert!(error.message.contains("sandbox"));
    let error = read_page("application/pdf", pdf(), 1024, &encrypted)
        .await
        .unwrap_err();
    assert_eq!(error.kind, AgentErrorKind::InvalidInput);
    assert!(error.message.contains("encrypted_pdf"));
}

#[tokio::test(start_paused = true)]
async fn waiting_for_a_permit_times_out_without_retry() {
    let error = conversion_permit(Arc::new(Semaphore::new(0)))
        .await
        .unwrap_err();
    assert_eq!(error.kind, AgentErrorKind::Timeout);
    assert!(!error.retryable);
}

static RELEASE: OnceLock<std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>> = OnceLock::new();

fn blocked(
    request: &SandboxRequest,
    source: &[u8],
    spec: &SandboxSpec,
) -> Result<(SandboxResponse, Vec<u8>), SandboxFailure> {
    let receiver = RELEASE.get().unwrap().lock().unwrap().take().unwrap();
    let _ = receiver.recv();
    extracted_pdf(request, source, spec)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_read_keeps_its_permit_until_the_conversion_ends() {
    let (release, receiver) = std::sync::mpsc::channel();
    RELEASE.set(std::sync::Mutex::new(Some(receiver))).unwrap();
    let permits = Arc::new(Semaphore::new(1));
    let read = tokio::spawn(page_from_response(
        head("application/pdf", None),
        chunks(vec![b"%PDF-1.7".to_vec()]),
        1024,
        &blocked,
        Arc::clone(&permits),
    ));
    while permits.available_permits() == 1 {
        tokio::task::yield_now().await;
    }
    read.abort();
    let _ = read.await;
    // The conversion still runs, so the permit is still held.
    assert_eq!(permits.available_permits(), 0);
    release.send(()).unwrap();
    while permits.available_permits() == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[test]
fn redirects_allow_same_host_protocols_and_ports_but_reject_other_authority() {
    let validated = desk_diagnose_core::web_research::validate_fetch_arguments(
        &desk_diagnose_core::chat::ToolCall {
            id: "fetch".into(),
            name: desk_diagnose_core::web_research::WEB_FETCH_TOOL_NAME.into(),
            arguments_json: r#"{"url":"https://example.com/docs"}"#.into(),
        },
    )
    .unwrap();
    for (location, allowed) in [
        ("/other?q=1", true),
        ("http://example.com:8080/news", true),
        ("https://EXAMPLE.com.:8443/news", true),
        ("https://sub.example.com/", false),
        ("https://example.com.attacker.example/", false),
        ("http://127.0.0.1/", false),
        ("ftp://example.com/", false),
        ("https://user:secret@example.com/", false),
        ("/docs#fragment", false),
    ] {
        assert_eq!(
            validated_redirect(&validated, validated.initial_url(), location).is_ok(),
            allowed,
            "{location}"
        );
    }
}

#[tokio::test]
async fn public_http_and_https_clients_build_requests_and_private_targets_fail() {
    for raw in [
        "http://93.184.216.34/docs",
        "https://93.184.216.34:8443/docs",
    ] {
        let url = desk_diagnose_core::web_research::validate_public_url(raw, true).unwrap();
        let client = pinned_public_client(&url).await.unwrap();
        assert_eq!(client.get(raw).build().unwrap().url(), &url);
    }
    for raw in ["http://127.0.0.1/", "https://10.0.0.1:8443/"] {
        assert!(
            pinned_public_client(&url::Url::parse(raw).unwrap())
                .await
                .is_err()
        );
    }
}
