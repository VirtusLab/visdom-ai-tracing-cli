//! VIS-316 end to end: a declared project the server refuses is queued in the
//! repo's pending file, never re-attributed via the repo-scoped endpoint, and
//! drains under whatever project is bound when the binding is fixed.
//!
//! Runs the real `tracevault stream` hook binary (via `CARGO_BIN_EXE_*`)
//! against a raw-TCP mock server, in the harness style of
//! `stream_event_project_test.rs`. Everything the hook reads is isolated to a
//! tempdir: the session state (`XDG_STATE_HOME`), the user config and
//! defaults (`XDG_CONFIG_HOME`, `HOME`), and the server (`TRACEVAULT_*`).
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tracevault_protocol::hooks::HookResponse;
use tracevault_protocol::streaming::{StreamEventRequest, StreamEventType};

const REPO_ID: &str = "11111111-1111-1111-1111-111111111111";
const SESSION_ID: &str = "vis316-refused-session";

/// How long the mock server waits for a connection that never arrives
/// before giving up, so its thread exits instead of blocking in `accept()`.
/// Mirrors `ACCEPT_DEADLINE` in `stream_event_project_test.rs`.
const ACCEPT_DEADLINE: Duration = Duration::from_secs(5);

fn accept_with_deadline(listener: &TcpListener) -> Option<std::net::TcpStream> {
    let deadline = Instant::now() + ACCEPT_DEADLINE;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                return Some(stream);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return None;
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

/// Serve `responses` to successive connections, capturing each request line.
/// The whole request (headers and JSON body) is read before answering, so the
/// client never sees its write end reset. Stage one response MORE than the
/// expected request count: a stray extra request (e.g. a repo-scoped
/// fallback) is then answered and captured, and fails the count assertion.
fn spawn_server(responses: Vec<String>) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for response in responses {
            let Some(stream) = accept_with_deadline(&listener) else {
                break;
            };
            let mut reader = BufReader::new(stream);
            let mut request_line = String::new();
            let _ = reader.read_line(&mut request_line);
            let mut content_length = 0usize;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                    break;
                }
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        content_length = value.trim().parse().unwrap_or(0);
                    }
                }
            }
            let mut body = vec![0u8; content_length];
            let _ = reader.read_exact(&mut body);
            let _ = tx.send(request_line.trim_end().to_string());
            let mut stream = reader.into_inner();
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{addr}"), rx)
}

fn http(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn accepted() -> String {
    let body = serde_json::to_string(&tracevault_protocol::streaming::StreamEventResponse {
        session_db_id: uuid::Uuid::nil(),
        event_db_id: Some(uuid::Uuid::nil()),
        status: "accepted".to_string(),
    })
    .unwrap();
    http("200 OK", &body)
}

fn refused() -> String {
    http(
        "400 Bad Request",
        r#"{"error":"repo is not a member of project"}"#,
    )
}

fn transient_failure() -> String {
    format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        "internal error".len(),
        "internal error"
    )
}

/// A minimal, parseable pending-queue line stamped with `event_uuid` so tests
/// can tell events apart after a drain/re-queue round trip.
fn sample_pending_event(event_uuid: uuid::Uuid) -> String {
    let req = StreamEventRequest {
        protocol_version: 1,
        tool: Some("claude-code".to_string()),
        event_type: StreamEventType::ToolUse,
        session_id: SESSION_ID.to_string(),
        timestamp: chrono::Utc::now(),
        hook_event_name: Some("PostToolUse".to_string()),
        tool_name: None,
        tool_use_id: None,
        tool_input: None,
        tool_response: None,
        tool_is_error: None,
        event_index: None,
        event_uuid: Some(event_uuid),
        transcript_lines: None,
        transcript_offset: None,
        model: None,
        cwd: None,
        final_stats: None,
        flow_id: None,
        labels: None,
        params: None,
    };
    serde_json::to_string(&req).unwrap()
}

/// A temp git repo bound to `REPO_ID`, plus isolated home/config/state dirs.
struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    repo: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
            vec!["commit", "-q", "--allow-empty", "-m", "init"],
        ] {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(&args)
                .status()
                .expect("git failed to run")
                .success();
            assert!(ok, "git {args:?} must succeed");
        }
        std::fs::create_dir_all(repo.join(".tracevault")).unwrap();
        std::fs::write(
            repo.join(".tracevault").join("config.toml"),
            format!("repo_id = \"{REPO_ID}\"\n"),
        )
        .unwrap();
        Fixture {
            _tmp: tmp,
            home,
            state,
            repo,
        }
    }

    /// Bind the session's `active_project`, where `session_state::load`
    /// reads it (`$XDG_STATE_HOME/tracevault/sessions/<id>.toml`).
    fn bind_session_project(&self, pid: uuid::Uuid) {
        let dir = self.state.join("tracevault").join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{SESSION_ID}.toml")),
            format!(
                "[active_project]\nproject_id = \"{pid}\"\nproject_name = \"p\"\nupdated_at = \"\"\n"
            ),
        )
        .unwrap();
    }

    fn queue_path(&self) -> PathBuf {
        self.repo
            .join(".tracevault")
            .join("sessions")
            .join(SESSION_ID)
            .join(format!("pending-{REPO_ID}.jsonl"))
    }

    /// Pre-seed the pending queue file with `lines` (already-serialized
    /// `StreamEventRequest` JSON), one per line, as if a previous run had
    /// buffered them.
    fn seed_queue(&self, lines: &[String]) {
        let path = self.queue_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut content = lines.join("\n");
        content.push('\n');
        std::fs::write(&path, content).unwrap();
    }

    /// Run the hook once against `base`: returns (exit code, stdout, stderr).
    fn run_hook(&self, base: &str) -> (i32, String, String) {
        let event = serde_json::json!({
            "session_id": SESSION_ID,
            "transcript_path": "",
            "cwd": self.repo.to_string_lossy(),
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_tracevault"))
            .args(["stream", "--event", "post-tool-use"])
            .current_dir(&self.repo)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_STATE_HOME", &self.state)
            .env("TRACEVAULT_SERVER_URL", base)
            .env("TRACEVAULT_API_KEY", "tvk_test")
            .env_remove("TRACEVAULT_SESSION_ID")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to run the tracevault binary");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(event.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

/// Every request the server captured. Called after the hook process exited,
/// so every request it made is already in the channel (the server sends the
/// line before answering).
fn captured(rx: &mpsc::Receiver<String>) -> Vec<String> {
    rx.try_iter().collect()
}

/// The queue's events; every line must parse (no corruption).
fn queued(path: &Path) -> Vec<StreamEventRequest> {
    match std::fs::read_to_string(path) {
        Ok(s) => s
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str::<StreamEventRequest>(l)
                    .unwrap_or_else(|e| panic!("corrupt queue line {l:?}: {e}"))
            })
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => panic!("cannot read queue: {e}"),
    }
}

fn allow_stdout() -> String {
    format!(
        "{}\n",
        serde_json::to_string(&HookResponse::allow()).unwrap()
    )
}

#[test]
fn refused_project_is_queued_in_the_repo_file_and_drains_under_the_fixed_binding() {
    let fx = Fixture::new();
    let first_pid = uuid::Uuid::from_u128(0xA1);
    fx.bind_session_project(first_pid);
    let refused_path = format!("POST /api/v1/projects/{first_pid}/stream?repo_id={REPO_ID} ");

    // Run 1: the declared project is refused. One request, to the project
    // endpoint; the event lands in the repo's queue.
    let (base, rx) = spawn_server(vec![refused(), accepted()]);
    let (code, stdout, stderr) = fx.run_hook(&base);
    assert_eq!(code, 0, "the hook never fails; stderr:\n{stderr}");
    assert_eq!(
        stdout,
        allow_stdout(),
        "stdout must be exactly the allow JSON"
    );
    let requests = captured(&rx);
    assert_eq!(requests.len(), 1, "exactly one request: {requests:?}");
    assert!(requests[0].starts_with(&refused_path), "{requests:?}");
    assert_eq!(
        stderr.matches("tracevault: error:").count(),
        1,
        "the refusal is printed once: {stderr}"
    );
    assert_eq!(queued(&fx.queue_path()).len(), 1, "the event is queued");

    // Run 2: still refused. The drain stops at the first queued event, so
    // there is still one request, and the queue now holds both events intact.
    let (base, rx) = spawn_server(vec![refused(), accepted()]);
    let (code, stdout, stderr) = fx.run_hook(&base);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, allow_stdout());
    let requests = captured(&rx);
    assert_eq!(requests.len(), 1, "exactly one request: {requests:?}");
    assert!(requests[0].starts_with(&refused_path), "{requests:?}");
    let events = queued(&fx.queue_path());
    assert_eq!(events.len(), 2, "both events queued");
    assert_ne!(
        events[0].event_uuid, events[1].event_uuid,
        "two distinct events, not one duplicated"
    );

    // Run 3: the user rebinds to a project the server accepts. Both queued
    // events and the live one go to the NEW project, and the queue empties.
    let second_pid = uuid::Uuid::from_u128(0xB2);
    fx.bind_session_project(second_pid);
    let (base, rx) = spawn_server(vec![accepted(), accepted(), accepted(), accepted()]);
    let (code, stdout, stderr) = fx.run_hook(&base);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, allow_stdout());
    let requests = captured(&rx);
    assert_eq!(requests.len(), 3, "two queued + one live: {requests:?}");
    for r in &requests {
        assert!(
            r.starts_with(&format!(
                "POST /api/v1/projects/{second_pid}/stream?repo_id={REPO_ID} "
            )),
            "every event goes to the rebound project: {requests:?}"
        );
    }
    assert!(queued(&fx.queue_path()).is_empty(), "the queue is drained");
}

/// VIS-316 (Copilot review, PR 53): when the pending-flush loop hits a
/// transient failure partway through, only the failed event and the ones
/// after it must be re-queued. Re-queueing the WHOLE pending batch (the old
/// bug) would duplicate the already-accepted event(s) on the next drain.
#[test]
fn pending_drain_failure_requeues_only_the_unsent_tail() {
    let fx = Fixture::new();
    let pid = uuid::Uuid::from_u128(0xC3);
    fx.bind_session_project(pid);

    let event_a = uuid::Uuid::from_u128(0xA);
    let event_b = uuid::Uuid::from_u128(0xB);
    fx.seed_queue(&[sample_pending_event(event_a), sample_pending_event(event_b)]);

    // A is accepted; B fails transiently. A third (spare) 200 is staged but
    // must never be consumed: the drain stops at B, and the live event C is
    // queued rather than sent once `send_failed` is set.
    let (base, rx) = spawn_server(vec![accepted(), transient_failure(), accepted()]);
    let (code, stdout, stderr) = fx.run_hook(&base);
    assert_eq!(code, 0, "the hook never fails; stderr:\n{stderr}");
    assert_eq!(
        stdout,
        allow_stdout(),
        "stdout must be exactly the allow JSON"
    );

    let requests = captured(&rx);
    assert_eq!(
        requests.len(),
        2,
        "exactly two requests: A (accepted) and B (transient failure); \
         the live event C must not be sent once the drain has failed: {requests:?}"
    );

    let events = queued(&fx.queue_path());
    assert_eq!(
        events.len(),
        2,
        "the queue holds B (re-queued) and C (the live event); A must not reappear: {events:?}"
    );
    assert_eq!(
        events[0].event_uuid,
        Some(event_b),
        "B is re-queued first, preserving order"
    );
    assert_ne!(
        events[0].event_uuid,
        Some(event_a),
        "A was already accepted by the server and must NOT be duplicated"
    );
    assert_ne!(
        events[1].event_uuid,
        Some(event_a),
        "A must not appear anywhere in the re-queued tail"
    );
    assert_ne!(
        events[1].event_uuid,
        Some(event_b),
        "the second entry is the live event C, not a second copy of B"
    );
}
