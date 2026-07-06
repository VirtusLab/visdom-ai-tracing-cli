//! Tests ApiClient::resolve_repo against a one-shot raw HTTP server.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use tracevault_cli::api_client::ApiClient;

/// Spawn a one-shot server that returns `response` (a full HTTP response) to
/// the first request, capturing the request line. Returns the bound base URL.
fn spawn_once(response: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn resolve_repo_returns_id_on_200() {
    let body = r#"{"repo_id":"44000761-8d22-4256-bd2c-27a0ba278c6f"}"#;
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    // Leak to get 'static for the thread closure.
    let base = spawn_once(Box::leak(resp.into_boxed_str()));
    let client = ApiClient::new(&base, Some("tok"));
    let got = client
        .resolve_repo("org", "git@github.com:org/repo.git")
        .await
        .unwrap();
    assert_eq!(
        got,
        Some("44000761-8d22-4256-bd2c-27a0ba278c6f".parse().unwrap())
    );
}

#[tokio::test]
async fn resolve_repo_returns_none_on_404() {
    let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let base = spawn_once(resp);
    let client = ApiClient::new(&base, Some("tok"));
    let got = client
        .resolve_repo("org", "git@github.com:no/such.git")
        .await
        .unwrap();
    assert_eq!(got, None);
}
