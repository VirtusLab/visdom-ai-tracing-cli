//! Regression test: bodyless POSTs must carry an explicit `Content-Length: 0`.
//!
//! reqwest/hyper omit the header entirely for a POST with no body, and strict
//! frontends (e.g. Google Front End) reject such requests with
//! `411 Length Required`. `tracevault login` starts with a bodyless
//! `POST /api/v1/auth/device`, so a missing header broke login outright.

use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracevault_cli::api_client::ApiClient;

/// Bind an ephemeral port and spawn a one-shot server that captures the first
/// request's raw bytes, replies with `response`, and yields the captured text.
async fn spawn_capture(response: Vec<u8>) -> (SocketAddr, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        // Read only up to the end of the header block; a bodyless request has
        // nothing after it, so waiting for EOF would deadlock against the client
        // waiting for our response.
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        sock.write_all(&response).await.unwrap();
        sock.flush().await.unwrap();
        String::from_utf8_lossy(&buf).to_string()
    });
    (addr, handle)
}

fn http_ok(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes()
}

#[tokio::test]
async fn device_start_sends_content_length_header() {
    let body = r#"{"token":"t","verification_url":"/u","expires_in":600}"#;
    let (addr, server) = spawn_capture(http_ok(body)).await;

    let client = ApiClient::new(&format!("http://{addr}"), None);
    let result = client.device_start().await;
    assert!(result.is_ok(), "device_start failed: {:?}", result.err());

    let request = server.await.unwrap();
    assert!(
        request.to_lowercase().contains("content-length: 0"),
        "device_start POST must send an explicit Content-Length; got:\n{request}"
    );
}

#[tokio::test]
async fn logout_sends_content_length_header() {
    let (addr, server) = spawn_capture(http_ok("{}")).await;

    let client = ApiClient::new(&format!("http://{addr}"), Some("dummy-key"));
    let result = client.logout().await;
    assert!(result.is_ok(), "logout failed: {:?}", result.err());

    let request = server.await.unwrap();
    assert!(
        request.to_lowercase().contains("content-length: 0"),
        "logout POST must send an explicit Content-Length; got:\n{request}"
    );
}
