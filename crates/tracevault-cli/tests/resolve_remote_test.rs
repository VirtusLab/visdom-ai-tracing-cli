//! Tests ApiClient::resolve_remote and ApiClient::get_remote_repos against a
//! one-shot raw HTTP server.
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
async fn resolve_remote_returns_remote_on_200() {
    let body = r#"{"remote_id":"44000761-8d22-4256-bd2c-27a0ba278c6f","name":"x","normalized_url":"github.com/o/x","clone_status":"ready"}"#;
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    let (base, rx) = spawn_once(Box::leak(resp.into_boxed_str()));
    let client = ApiClient::new(&base, Some("tok"));
    let got = client
        .resolve_remote("git@github.com:o/x.git")
        .await
        .unwrap();
    let got = got.expect("expected Some");
    assert_eq!(
        got.remote_id,
        "44000761-8d22-4256-bd2c-27a0ba278c6f"
            .parse::<uuid::Uuid>()
            .unwrap()
    );
    assert_eq!(got.normalized_url, "github.com/o/x");
    let request = rx.recv_timeout(RECV_TIMEOUT).expect("no request");
    assert!(request.contains("/api/v1/remotes/resolve?git_url="));
    assert!(
        request.contains("%40"),
        "git_url must be percent-encoded: {request}"
    );
}

#[tokio::test]
async fn resolve_remote_returns_none_on_domain_404() {
    // A genuine domain 404 — the server recognizes the route but has no
    // matching codebase — carries the server's JSON error envelope
    // (`{"error": "..."}`, as every `AppError` renders). This is the shape a
    // real (non-skewed) server sends for "not tracked".
    let body = r#"{"error":"No remote for that URL"}"#;
    let resp = format!(
        "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    let (base, rx) = spawn_once(Box::leak(resp.into_boxed_str()));
    let client = ApiClient::new(&base, Some("tok"));
    assert!(client
        .resolve_remote("git@github.com:o/x.git")
        .await
        .unwrap()
        .is_none());
    // Assert a request actually went out (else a buggy resolve_remote that
    // returned None without any I/O would still pass this test).
    let request = rx.recv_timeout(RECV_TIMEOUT).expect("no request captured");
    assert!(request.contains("/api/v1/remotes/resolve?git_url="));
}

#[tokio::test]
async fn resolve_remote_errs_on_bare_404_version_skew() {
    // A bare 404 with no JSON error body is axum's built-in "no route
    // matched" fallback — the shape an OLD server sends for a de-slugged
    // path it doesn't have yet. This must NOT be read as "not tracked"; it
    // must surface as an error the user can diagnose as a version skew.
    let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let (base, _rx) = spawn_once(resp);
    let client = ApiClient::new(&base, Some("tok"));
    let err = client
        .resolve_remote("git@github.com:o/x.git")
        .await
        .expect_err("a bare 404 body must not be treated as 'not tracked'");
    assert!(
        err.to_string().to_lowercase().contains("version mismatch"),
        "expected a version-mismatch hint; got: {err}"
    );
}

#[tokio::test]
async fn get_remote_repos_returns_repos_array() {
    // Server RemoteDetailResponse flattens the remote fields at top level + a `repos` array.
    let body = r#"{"id":"44000761-8d22-4256-bd2c-27a0ba278c6f","name":"x","normalized_url":"github.com/o/x","clone_url":"https://github.com/o/x.git","clone_status":"ready","clone_error":null,"last_fetched_at":null,"repo_count":2,"created_at":"2026-01-01T00:00:00Z","repos":[{"id":"11111111-1111-4111-8111-111111111111","name":"a"},{"id":"22222222-2222-4222-8222-222222222222","name":"b"}]}"#;
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    let (base, rx) = spawn_once(Box::leak(resp.into_boxed_str()));
    let client = ApiClient::new(&base, Some("tok"));
    let repos = client
        .get_remote_repos("44000761-8d22-4256-bd2c-27a0ba278c6f".parse().unwrap())
        .await
        .unwrap();
    assert_eq!(repos.len(), 2);
    assert_eq!(repos[0].name, "a");
    let request = rx.recv_timeout(RECV_TIMEOUT).expect("no request");
    assert!(request.contains("/api/v1/remotes/44000761-8d22-4256-bd2c-27a0ba278c6f"));
}
