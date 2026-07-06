//! Tests ApiClient::resolve_repo against a one-shot raw HTTP server.
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tracevault_cli::api_client::ApiClient;

/// How long a test waits for the captured request before failing (rather than
/// hanging forever if the client never connects).
const RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// Spawn a one-shot server that returns `response` (a full HTTP response) to
/// the first request. Captures the HTTP request line (the first line, which
/// carries the method + path + query) and sends it over the returned channel
/// before writing the response. Returns (base_url, request_receiver).
fn spawn_once(response: &'static str) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            // Read exactly the request line — a single `read()` could return a
            // partial buffer and make the query-string assertions flaky.
            let mut reader = BufReader::new(stream);
            let mut request_line = String::new();
            let _ = reader.read_line(&mut request_line);
            let _ = tx.send(request_line);
            let mut stream = reader.into_inner();
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
    let request = rx.recv_timeout(RECV_TIMEOUT).expect("no request captured");
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
    // Drain the receiver (we don't assert, just consume it) with a timeout so
    // the test can't hang if the request was never sent.
    let _ = rx.recv_timeout(RECV_TIMEOUT);
}
