//! Tests ApiClient::stream_event_for_project against a one-shot raw HTTP
//! server, mirroring the harness in resolve_remote_test.rs.
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tracevault_cli::api_client::ApiClient;
use tracevault_protocol::streaming::{StreamEventRequest, StreamEventType};

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

fn sample_stream_event_request() -> StreamEventRequest {
    StreamEventRequest {
        protocol_version: 2,
        tool: Some("claude-code".to_string()),
        event_type: StreamEventType::ToolUse,
        session_id: "sess-1".into(),
        timestamp: chrono::Utc::now(),
        hook_event_name: Some("PostToolUse".into()),
        tool_name: None,
        tool_use_id: None,
        tool_input: None,
        tool_response: None,
        tool_is_error: None,
        event_index: None,
        event_uuid: None,
        transcript_lines: None,
        transcript_offset: None,
        model: None,
        cwd: None,
        final_stats: None,
        flow_id: None,
        labels: None,
        params: None,
    }
}

#[tokio::test]
async fn stream_event_for_project_targets_project_endpoint() {
    let response_body =
        serde_json::to_string(&tracevault_protocol::streaming::StreamEventResponse {
            session_db_id: uuid::Uuid::nil(),
            event_db_id: Some(uuid::Uuid::nil()),
            status: "accepted".to_string(),
        })
        .unwrap();
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response_body.len(),
        response_body
    );
    let (base, rx) = spawn_once(Box::leak(resp.into_boxed_str()));
    let client = ApiClient::new(&base, Some("tok"));
    let project_id = uuid::Uuid::nil();
    let req = sample_stream_event_request();

    let got = client
        .stream_event_for_project(
            "org",
            project_id,
            "11111111-1111-1111-1111-111111111111",
            &req,
        )
        .await
        .unwrap();
    assert_eq!(got.status, "accepted");

    let line = rx.recv_timeout(RECV_TIMEOUT).expect("no request captured");
    assert!(
        line.starts_with(
            "POST /api/v1/orgs/org/projects/00000000-0000-0000-0000-000000000000/stream?repo_id=11111111-1111-1111-1111-111111111111 "
        ),
        "got: {line}"
    );
}
