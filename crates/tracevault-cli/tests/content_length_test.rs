//! Regression test: the login flow's POSTs must carry an explicit
//! `Content-Length`.
//!
//! History: reqwest/hyper omit the header entirely for a POST with no body,
//! and strict frontends (e.g. Google Front End) reject such requests with
//! `411 Length Required`. The old server-brokered login started with a
//! bodyless `POST /api/v1/auth/device`, and the missing header broke login
//! outright.
//!
//! The device flow replaced those endpoints with form-encoded OAuth POSTs to
//! Keycloak, which get a `Content-Length` from their body — so the bug is now
//! structurally avoided rather than patched. These tests pin that: if a
//! future change ever posts one of these requests without a body (or with a
//! streaming body, which would produce `Transfer-Encoding: chunked` instead),
//! the 411 class of failure comes straight back.

use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracevault_cli::oidc::{self, Discovery};

/// Bind an ephemeral port and spawn a one-shot server that captures the first
/// request's raw bytes, replies with `response`, and yields the captured text.
async fn spawn_capture(response: Vec<u8>) -> (SocketAddr, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        // Read only up to the end of the header block; waiting for EOF would
        // deadlock against the client waiting for our response.
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

/// A `Discovery` whose endpoints all point at `addr`.
fn discovery_at(addr: SocketAddr) -> Discovery {
    Discovery {
        device_authorization_endpoint: Some(format!("http://{addr}/device")),
        token_endpoint: format!("http://{addr}/token"),
        revocation_endpoint: Some(format!("http://{addr}/revoke")),
    }
}

fn assert_length_delimited(request: &str, what: &str) {
    let lower = request.to_lowercase();
    assert!(
        lower.contains("content-length:"),
        "{what} must be length-delimited or a strict frontend answers 411; got:\n{request}"
    );
    assert!(
        !lower.contains("transfer-encoding: chunked"),
        "{what} must not be chunked; got:\n{request}"
    );
    assert!(
        lower.contains("content-type: application/x-www-form-urlencoded"),
        "{what} must be a form POST; got:\n{request}"
    );
}

#[tokio::test]
async fn device_start_post_is_length_delimited() {
    let body = r#"{"device_code":"dc","user_code":"ABCD-EFGH","verification_uri":"/u","expires_in":600,"interval":5}"#;
    let (addr, server) = spawn_capture(http_ok(body)).await;

    let client = reqwest::Client::new();
    let result = oidc::device_start(&client, &discovery_at(addr), "tracevault-cli").await;
    assert!(result.is_ok(), "device_start failed: {:?}", result.err());

    assert_length_delimited(&server.await.unwrap(), "the device authorization POST");
}

#[tokio::test]
async fn revoke_post_is_length_delimited() {
    let (addr, server) = spawn_capture(http_ok("{}")).await;

    let client = reqwest::Client::new();
    let result = oidc::revoke(&client, &discovery_at(addr), "tracevault-cli", "rt").await;
    assert!(result.is_ok(), "revoke failed: {:?}", result.err());

    assert_length_delimited(&server.await.unwrap(), "the revocation POST");
}
