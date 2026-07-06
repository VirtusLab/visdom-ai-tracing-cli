//! Tests ApiClient::resolve_repo against a one-shot raw HTTP server.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use tracevault_cli::api_client::ApiClient;

/// Spawn a one-shot server that returns `response` (a full HTTP response) to
/// the first request. Captures the request line and sends it (as lossy UTF-8)
/// over the returned channel before writing the response.
/// Returns (base_url, request_receiver).
fn spawn_once(response: &'static str) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 2048];
            if let Ok(n) = stream.read(&mut buf) {
                let request_str = String::from_utf8_lossy(&buf[..n]).to_string();
                let _ = tx.send(request_str);
            }
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{addr}"), rx)
}

#[tokio::test]
async fn resolve_repo_returns_id_on_200() {
    let body = r#"{"repo_id":"44000761-8d22-4256-bd2c-27a0ba278c6f"}"#;
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    // Leak to get 'static for the thread closure.
    let (base, rx) = spawn_once(Box::leak(resp.into_boxed_str()));
    let client = ApiClient::new(&base, Some("tok"));
    let got = client
        .resolve_repo("org", "git@github.com:org/repo.git")
        .await
        .unwrap();
    assert_eq!(
        got,
        Some("44000761-8d22-4256-bd2c-27a0ba278c6f".parse().unwrap())
    );

    // Verify the git_url was percent-encoded in the request.
    let request = rx.recv().unwrap();
    assert!(request.contains("/api/v1/orgs/org/repos/resolve?git_url="));
    assert!(
        request.contains("%40"),
        "Expected %40 (encoded @) in request, got: {}",
        request
    );
    assert!(
        !request.contains("git@github.com"),
        "Request should not contain raw git@github.com, got: {}",
        request
    );
}

#[tokio::test]
async fn resolve_repo_returns_none_on_404() {
    let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let (_base, rx) = spawn_once(resp);
    let client = ApiClient::new(&_base, Some("tok"));
    let got = client
        .resolve_repo("org", "git@github.com:no/such.git")
        .await
        .unwrap();
    assert_eq!(got, None);
    // Drain the receiver (we don't assert, just consume it).
    let _ = rx.recv();
}
