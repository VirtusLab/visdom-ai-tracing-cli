//! Tests ApiClient::resolve_project against a one-shot raw HTTP server.
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tracevault_cli::api_client::{ApiClient, ResolveProjectOutcome};

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
async fn resolve_project_returns_resolved_on_200() {
    let body = r#"{"project_id":"11111111-1111-1111-1111-111111111111"}"#;
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    let (base, rx) = spawn_once(Box::leak(resp.into_boxed_str()));
    let client = ApiClient::new(&base, Some("tok"));
    let outcome = client
        .resolve_project("git@github.com:acme/app.git")
        .await
        .unwrap();
    match outcome {
        ResolveProjectOutcome::Resolved(id) => {
            assert_eq!(id.to_string(), "11111111-1111-1111-1111-111111111111");
        }
        other => panic!("expected Resolved, got {other:?}"),
    }
    let request = rx.recv_timeout(RECV_TIMEOUT).expect("no request");
    assert!(request.contains("/api/v1/projects/resolve?git_url=git%40github.com%3Aacme%2Fapp.git"));
    assert!(
        request.contains("%40"),
        "git_url must be percent-encoded: {request}"
    );
}

#[tokio::test]
async fn resolve_project_returns_none_on_domain_404() {
    // A genuine domain 404 carries the server's JSON error envelope
    // (`{"error": "..."}`, as every `AppError` renders) — the shape a real
    // (non-skewed) server sends for "no project".
    let body = r#"{"error":"No project for that URL"}"#;
    let resp = format!(
        "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    let (base, rx) = spawn_once(Box::leak(resp.into_boxed_str()));
    let client = ApiClient::new(&base, Some("tok"));
    let outcome = client
        .resolve_project("git@github.com:acme/app.git")
        .await
        .unwrap();
    match outcome {
        ResolveProjectOutcome::None => {}
        other => panic!("expected None, got {other:?}"),
    }
    // Assert a request actually went out (else a buggy resolve_project that
    // returned None without any I/O would still pass this test).
    let request = rx.recv_timeout(RECV_TIMEOUT).expect("no request captured");
    assert!(request.contains("/api/v1/projects/resolve?git_url="));
}

#[tokio::test]
async fn resolve_project_errs_on_bare_404_version_skew() {
    // A bare 404 with no JSON error body is axum's built-in "no route
    // matched" fallback — the shape an OLD server sends for a de-slugged
    // path it doesn't have yet. This must NOT be read as "no project"; it
    // must surface as an error the user can diagnose as a version skew.
    let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let (base, _rx) = spawn_once(resp);
    let client = ApiClient::new(&base, Some("tok"));
    let err = client
        .resolve_project("git@github.com:acme/app.git")
        .await
        .expect_err("a bare 404 body must not be treated as 'no project'");
    assert!(
        err.to_string().to_lowercase().contains("version mismatch"),
        "expected a version-mismatch hint; got: {err}"
    );
}

#[tokio::test]
async fn resolve_project_returns_ambiguous_on_409() {
    let resp = "HTTP/1.1 409 Conflict\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let (base, rx) = spawn_once(resp);
    let client = ApiClient::new(&base, Some("tok"));
    let outcome = client
        .resolve_project("git@github.com:acme/app.git")
        .await
        .unwrap();
    match outcome {
        ResolveProjectOutcome::Ambiguous => {}
        other => panic!("expected Ambiguous, got {other:?}"),
    }
    let request = rx.recv_timeout(RECV_TIMEOUT).expect("no request captured");
    assert!(request.contains("/api/v1/projects/resolve?git_url="));
}
