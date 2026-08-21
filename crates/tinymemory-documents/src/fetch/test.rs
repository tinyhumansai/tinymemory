//! Tests for URL intake.
//!
//! Only the guard and the argument handling are exercised here. Anything that
//! would actually reach the network is out of scope by the repository's testing
//! rules — the fetch itself is covered by `tinymemory-sources`' own reader
//! tests, which own the client this module borrows.

use super::*;

async fn local_response(response: impl Into<Vec<u8>>) -> reqwest::Response {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind controlled server");
    let address = listener.local_addr().expect("server address");
    let response = response.into();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept request");
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request).await.expect("read request");
        stream.write_all(&response).await.expect("write response");
    });
    let received = reqwest::get(format!("http://{address}/"))
        .await
        .expect("controlled response");
    server.await.expect("server task");
    received
}

#[tokio::test]
async fn a_malformed_url_is_rejected_before_anything_is_fetched() {
    let error = fetch_url("not a url").await.unwrap_err();
    assert!(matches!(error, MemoryError::Invalid(_)), "got {error:?}");
    assert!(error.to_string().contains("invalid url"), "got {error}");
}

#[tokio::test]
async fn loopback_and_link_local_targets_are_refused() {
    for url in [
        "http://127.0.0.1/",
        "http://localhost:6379/",
        "http://169.254.169.254/latest/meta-data/",
        "http://[::1]/",
    ] {
        let error = fetch_url(url).await.unwrap_err();
        assert!(
            error.to_string().contains("not an allowed fetch target"),
            "{url} gave {error}"
        );
    }
}

#[tokio::test]
async fn a_non_http_scheme_is_refused() {
    for url in [
        "file:///etc/passwd",
        "ftp://example.com/x",
        "gopher://example.com/",
    ] {
        let error = fetch_url(url).await.unwrap_err();
        assert!(
            matches!(error, MemoryError::Invalid(_)),
            "{url} gave {error:?}"
        );
    }
}

#[test]
fn a_size_limit_failure_is_reported_as_budget_exceeded() {
    let error = read_error(
        "https://example.com/",
        "response body exceeds 8-byte limit (Content-Length=9)",
    );
    assert!(
        matches!(error, MemoryError::BudgetExceeded(_)),
        "got {error:?}"
    );
}

#[test]
fn an_interrupted_read_is_reported_as_unreachable_not_budget_exceeded() {
    let error = read_error(
        "https://example.com/",
        "failed to read response body: connection reset",
    );
    assert!(
        matches!(error, MemoryError::Unreachable(_)),
        "got {error:?}"
    );
}

#[tokio::test]
async fn completed_response_preserves_body_type_origin_and_filename() {
    let response = local_response(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/markdown; charset=utf-8\r\nContent-Length: 7\r\n\r\n# title",
    )
    .await;
    let url = reqwest::Url::parse("https://example.com/guides/readme.md").unwrap();
    let document = response_to_document(url.as_str(), url.clone(), response)
        .await
        .unwrap();
    assert_eq!(document.bytes, b"# title");
    assert_eq!(document.origin.as_deref(), Some(url.as_str()));
    assert_eq!(document.filename.as_deref(), Some("readme.md"));
    assert_eq!(
        document.declared_mime.as_deref(),
        Some("text/markdown; charset=utf-8")
    );
}

#[tokio::test]
async fn completed_response_handles_status_empty_body_and_filename_absence() {
    let response =
        local_response(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n").await;
    let url = reqwest::Url::parse("https://example.com/unavailable").unwrap();
    let error = response_to_document(url.as_str(), url.clone(), response)
        .await
        .unwrap_err();
    assert!(matches!(error, MemoryError::Backend(_)));

    let response = local_response(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
    let error = response_to_document(url.as_str(), url.clone(), response)
        .await
        .unwrap_err();
    assert!(matches!(error, MemoryError::Invalid(_)));

    let response = local_response(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ntext").await;
    let document = response_to_document(url.as_str(), url.clone(), response)
        .await
        .unwrap();
    assert_eq!(document.bytes, b"text");
    assert!(document.filename.is_none());
    assert!(document.declared_mime.is_none());
}

#[tokio::test]
async fn completed_response_maps_declared_oversize_to_budget_exceeded() {
    let response = local_response(
        [
            b"HTTP/1.1 200 OK\r\nContent-Length: ",
            (MAX_DOCUMENT_BYTES + 1).to_string().as_bytes(),
            b"\r\n\r\n",
        ]
        .concat(),
    )
    .await;
    let url = reqwest::Url::parse("https://example.com/huge.bin").unwrap();
    let error = response_to_document(url.as_str(), url.clone(), response)
        .await
        .unwrap_err();
    assert!(matches!(error, MemoryError::BudgetExceeded(_)));
}
