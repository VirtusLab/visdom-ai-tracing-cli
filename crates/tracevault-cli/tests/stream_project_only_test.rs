//! VIS-1255 end to end: a session in an umbrella directory (not a git repo,
//! holding a `.tracevault/`) with only a PROJECT bound is recorded
//! project-only, queues per project while the server is unreachable or
//! refuses, and moves to the repo-scoped path when `repo switch` binds a repo
//! mid-session — without losing a transcript line or sending one to two
//! different server session rows.
//!
//! Runs the real `tracevault stream` hook binary against a raw-TCP mock
//! server, in the harness style of `stream_refused_project_test.rs`, but
//! captures each request BODY too: the transcript lines and their offset are
//! what "no loss, no duplication" is about. Everything the hook reads is
//! isolated to a tempdir (`XDG_STATE_HOME`, `XDG_CONFIG_HOME`, `HOME`,
//! `TRACEVAULT_*`).
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tracevault_protocol::hooks::HookResponse;
use tracevault_protocol::streaming::StreamEventRequest;

const REPO_ID: &str = "22222222-2222-2222-2222-222222222222";
const SESSION_ID: &str = "vis1255-umbrella-session";
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

/// One captured request: its request line and its parsed JSON body.
#[derive(Debug)]
struct Captured {
    line: String,
    body: StreamEventRequest,
}

/// Serve `responses` to successive connections, capturing each request line
/// and body. Stage one response MORE than expected: a stray extra request is
/// then answered and captured, and fails the count assertion.
fn spawn_server(responses: Vec<String>) -> (String, mpsc::Receiver<Captured>) {
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
            let body: StreamEventRequest =
                serde_json::from_slice(&body).expect("request body is a StreamEventRequest");
            let _ = tx.send(Captured {
                line: request_line.trim_end().to_string(),
                body,
            });
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

fn unavailable() -> String {
    http("503 Service Unavailable", r#"{"error":"down"}"#)
}

fn not_found() -> String {
    http("404 Not Found", r#"{"error":"project not found"}"#)
}

fn allow_stdout() -> String {
    format!(
        "{}\n",
        serde_json::to_string(&HookResponse::allow()).unwrap()
    )
}

/// An umbrella dir (no git) with a `.tracevault/`, a transcript file, and
/// isolated home/config/state dirs.
struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    umbrella: PathBuf,
    transcript: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let umbrella = tmp.path().join("umbrella");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(umbrella.join(".tracevault")).unwrap();
        let transcript = tmp.path().join("transcript.jsonl");
        std::fs::write(&transcript, "").unwrap();
        Fixture {
            _tmp: tmp,
            home,
            state,
            umbrella,
            transcript,
        }
    }

    fn session_dir(&self) -> PathBuf {
        self.umbrella
            .join(".tracevault")
            .join("sessions")
            .join(SESSION_ID)
    }

    fn project_queue(&self, pid: uuid::Uuid) -> PathBuf {
        self.session_dir()
            .join(format!("pending-project-{pid}.jsonl"))
    }

    fn repo_queue(&self) -> PathBuf {
        self.session_dir().join(format!("pending-{REPO_ID}.jsonl"))
    }

    fn offset(&self) -> Option<i64> {
        std::fs::read_to_string(self.session_dir().join(".stream_offset"))
            .ok()
            .map(|s| s.trim().parse().unwrap())
    }

    /// Write the session state file (`project switch` / `repo switch` write
    /// the same file, `$XDG_STATE_HOME/tracevault/sessions/<id>.toml`).
    fn write_session_state(&self, pid: uuid::Uuid, repo: bool) {
        let dir = self.state.join("tracevault").join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let mut toml = String::new();
        if repo {
            toml.push_str(&format!(
                "[active]\nrepo_id = \"{REPO_ID}\"\nupdated_at = \"\"\n\n"
            ));
        }
        toml.push_str(&format!(
            "[active_project]\nproject_id = \"{pid}\"\nproject_name = \"p\"\nupdated_at = \"\"\n"
        ));
        std::fs::write(dir.join(format!("{SESSION_ID}.toml")), toml).unwrap();
    }

    /// Append transcript lines `{"n": k}` for each k in `ns`.
    fn append_transcript(&self, ns: std::ops::RangeInclusive<u32>) {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.transcript)
            .unwrap();
        for n in ns {
            writeln!(f, "{}", serde_json::json!({ "n": n })).unwrap();
        }
    }

    fn run_hook(&self, base: &str) -> (i32, String, String) {
        let event = serde_json::json!({
            "session_id": SESSION_ID,
            "transcript_path": self.transcript.to_string_lossy(),
            "cwd": self.umbrella.to_string_lossy(),
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
        });
        self.run_hook_event(base, "claude-code", event)
    }

    /// The OpenCode (fileless) shape: records arrive inline on the event and
    /// cannot be re-read, and `.stream_offset` is a record counter that
    /// advances even when the event is queued.
    fn run_inline_hook(&self, base: &str, ns: std::ops::RangeInclusive<u32>) {
        let records: Vec<_> = ns.map(|n| serde_json::json!({ "n": n })).collect();
        let event = serde_json::json!({
            "session_id": SESSION_ID,
            "transcript_path": "",
            "transcript_records": records,
            "cwd": self.umbrella.to_string_lossy(),
            "hook_event_name": "PostToolUse",
            "tool_name": "bash",
        });
        self.run_hook_event(base, "opencode", event);
    }

    fn run_hook_event(
        &self,
        base: &str,
        agent: &str,
        event: serde_json::Value,
    ) -> (i32, String, String) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tracevault"))
            .args(["stream", "--event", "post-tool-use", "--agent", agent])
            .current_dir(&self.umbrella)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_STATE_HOME", &self.state)
            .env("TRACEVAULT_SERVER_URL", base)
            .env("TRACEVAULT_API_KEY", "tvk_test")
            .env_remove("TRACEVAULT_SESSION_ID")
            .env_remove("AMP_CURRENT_THREAD_ID")
            .env_remove("TRACEVAULT_PROJECT")
            .env_remove("TRACEVAULT_PROJECT_ATTRIBUTION")
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
        let (code, stdout, stderr) = (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        );
        assert_eq!(code, 0, "the hook never fails; stderr:\n{stderr}");
        assert_eq!(stdout, allow_stdout(), "stderr:\n{stderr}");
        (code, stdout, stderr)
    }
}

fn captured(rx: &mpsc::Receiver<Captured>) -> Vec<Captured> {
    rx.try_iter().collect()
}

/// The `n` of every transcript line in a request, in order.
fn line_ns(req: &StreamEventRequest) -> Vec<u64> {
    req.transcript_lines
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|l| l["n"].as_u64().unwrap())
        .collect()
}

fn queued(path: &std::path::Path) -> Vec<StreamEventRequest> {
    match std::fs::read_to_string(path) {
        Ok(s) => s
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("queue line parses"))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => panic!("cannot read queue: {e}"),
    }
}

/// No repo binding, project bound, umbrella dir: the event goes to the bare
/// project endpoint (no `repo_id`). With the server down it is queued in
/// `pending-project-<pid>.jsonl`, the offset does not move, and the next
/// event drains the queue before sending its own.
#[test]
fn umbrella_session_records_project_only_and_drains_its_queue() {
    let fx = Fixture::new();
    let pid = uuid::Uuid::from_u128(0x1255);
    fx.write_session_state(pid, false);
    let bare = format!("POST /api/v1/projects/{pid}/stream ");

    // Run 1: accepted, project-only.
    fx.append_transcript(1..=2);
    let (base, rx) = spawn_server(vec![accepted(), accepted()]);
    fx.run_hook(&base);
    let reqs = captured(&rx);
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert!(reqs[0].line.starts_with(&bare), "no repo_id: {reqs:?}");
    assert_eq!(line_ns(&reqs[0].body), vec![1, 2]);
    let after_run1 = fx.offset().expect("offset advanced");

    // Run 2: server down — queued per project, offset unchanged.
    fx.append_transcript(3..=3);
    let (base, rx) = spawn_server(vec![unavailable(), accepted()]);
    fx.run_hook(&base);
    assert_eq!(captured(&rx).len(), 1);
    let q = queued(&fx.project_queue(pid));
    assert_eq!(q.len(), 1, "queued in the project file");
    assert_eq!(line_ns(&q[0]), vec![3]);
    assert_eq!(fx.offset(), Some(after_run1), "offset must not advance");
    assert!(!fx.repo_queue().exists());

    // Run 3: server back — the queued event drains first, then the live one
    // re-reads line 3 from the unchanged offset (same chunk_index, same
    // server row: deduped) together with line 4.
    fx.append_transcript(4..=4);
    let (base, rx) = spawn_server(vec![accepted(), accepted(), accepted()]);
    fx.run_hook(&base);
    let reqs = captured(&rx);
    assert_eq!(reqs.len(), 2, "queued + live: {reqs:?}");
    for r in &reqs {
        assert!(r.line.starts_with(&bare), "{reqs:?}");
    }
    assert_eq!(line_ns(&reqs[0].body), vec![3]);
    assert_eq!(line_ns(&reqs[1].body), vec![3, 4]);
    assert_eq!(
        reqs[0].body.transcript_offset, reqs[1].body.transcript_offset,
        "line 3 carries the same offset both times, so the server dedupes it"
    );
    assert!(queued(&fx.project_queue(pid)).is_empty(), "queue drained");
    assert!(fx.offset().unwrap() > after_run1);
}

/// VIS-316 for the repo-less route: a deterministic 404 is an error that is
/// printed and buffered, never dropped — the offset stays put, so the lines
/// are re-read once the binding is fixed.
#[test]
fn refused_project_only_send_is_queued_not_dropped() {
    let fx = Fixture::new();
    let pid = uuid::Uuid::from_u128(0xBAD);
    fx.write_session_state(pid, false);
    fx.append_transcript(1..=2);

    let (base, rx) = spawn_server(vec![not_found(), accepted()]);
    let (_, _, stderr) = fx.run_hook(&base);
    let reqs = captured(&rx);
    assert_eq!(reqs.len(), 1, "exactly one request, no fallback: {reqs:?}");
    assert!(reqs[0]
        .line
        .starts_with(&format!("POST /api/v1/projects/{pid}/stream ")));
    assert_eq!(
        stderr.matches("tracevault: error:").count(),
        1,
        "the refusal is printed once: {stderr}"
    );
    assert!(!stderr.contains("DROPPED"), "{stderr}");
    let q = queued(&fx.project_queue(pid));
    assert_eq!(q.len(), 1, "the refused event is queued");
    assert_eq!(line_ns(&q[0]), vec![1, 2]);
    assert_eq!(fx.offset(), None, "nothing was accepted, offset untouched");
}

/// `repo switch` mid-session: once a repo binds, events go to the
/// repo-scoped path (project-scoped with `repo_id`, since the project is
/// still bound) and continue from where the project-only sends stopped.
/// Lines already accepted project-only are not re-sent; lines that were
/// still queued project-only are drained into the REPO target (the queue is
/// adopted), so none is delivered to the repo-less row as well as the repo
/// row, and none is lost.
#[test]
fn repo_switch_mid_session_moves_to_the_repo_path_without_loss_or_duplication() {
    let fx = Fixture::new();
    let pid = uuid::Uuid::from_u128(0x5117C4);
    fx.write_session_state(pid, false);

    // Phase 1: project-only, accepted (lines 1-2).
    fx.append_transcript(1..=2);
    let (base, rx) = spawn_server(vec![accepted(), accepted()]);
    fx.run_hook(&base);
    let reqs = captured(&rx);
    assert_eq!(reqs.len(), 1);
    assert_eq!(line_ns(&reqs[0].body), vec![1, 2]);

    // Phase 2: project-only, server down (line 3) — queued per project.
    fx.append_transcript(3..=3);
    let (base, rx) = spawn_server(vec![unavailable(), accepted()]);
    fx.run_hook(&base);
    assert_eq!(captured(&rx).len(), 1);
    assert_eq!(queued(&fx.project_queue(pid)).len(), 1);

    // Phase 3: `repo switch` binds a repo; line 4 arrives.
    fx.write_session_state(pid, true);
    fx.append_transcript(4..=4);
    let (base, rx) = spawn_server(vec![accepted(), accepted(), accepted()]);
    fx.run_hook(&base);
    let reqs = captured(&rx);
    assert_eq!(reqs.len(), 2, "adopted queue + live: {reqs:?}");
    let repo_path = format!("POST /api/v1/projects/{pid}/stream?repo_id={REPO_ID} ");
    for r in &reqs {
        assert!(
            r.line.starts_with(&repo_path),
            "every send after the switch is repo-scoped: {reqs:?}"
        );
    }
    assert_eq!(line_ns(&reqs[0].body), vec![3], "the queued event, adopted");
    assert_eq!(line_ns(&reqs[1].body), vec![3, 4], "re-read from offset");
    assert_eq!(
        reqs[0].body.transcript_offset, reqs[1].body.transcript_offset,
        "line 3 has one chunk_index on ONE row (the repo row), so it dedupes"
    );
    assert!(
        !fx.project_queue(pid).exists() || queued(&fx.project_queue(pid)).is_empty(),
        "the project-only queue is adopted, not stranded for a later flush \
         to deliver to the repo-less row"
    );
    assert!(queued(&fx.repo_queue()).is_empty());

    // Every line reached the server, and lines 1-2 (accepted project-only)
    // were never re-sent after the switch.
    let after_switch: Vec<u64> = reqs.iter().flat_map(|r| line_ns(&r.body)).collect();
    assert!(!after_switch.contains(&1) && !after_switch.contains(&2));

    // Phase 4: a further event only carries the new line.
    fx.append_transcript(5..=5);
    let (base, rx) = spawn_server(vec![accepted(), accepted()]);
    fx.run_hook(&base);
    let reqs = captured(&rx);
    assert_eq!(reqs.len(), 1);
    assert!(reqs[0].line.starts_with(&repo_path));
    assert_eq!(line_ns(&reqs[0].body), vec![5]);
}

/// A repo switch whose send then FAILS moves the adopted project-only events
/// into the repo's queue (in order, older first) instead of putting them back
/// in the project file.
#[test]
fn adopted_queue_migrates_to_the_repo_queue_on_failure() {
    let fx = Fixture::new();
    let pid = uuid::Uuid::from_u128(0xADD);
    fx.write_session_state(pid, false);
    fx.append_transcript(1..=1);
    let (base, rx) = spawn_server(vec![unavailable(), accepted()]);
    fx.run_hook(&base);
    assert_eq!(captured(&rx).len(), 1);
    assert_eq!(queued(&fx.project_queue(pid)).len(), 1);

    fx.write_session_state(pid, true);
    fx.append_transcript(2..=2);
    let (base, rx) = spawn_server(vec![unavailable(), accepted()]);
    fx.run_hook(&base);
    assert_eq!(captured(&rx).len(), 1, "drain stops at the first failure");
    assert!(
        queued(&fx.project_queue(pid)).is_empty(),
        "nothing left behind"
    );
    let q = queued(&fx.repo_queue());
    assert_eq!(q.len(), 2, "adopted event + live event: {q:?}");
    assert_eq!(line_ns(&q[0]), vec![1], "older (adopted) event first");
    assert_eq!(line_ns(&q[1]), vec![1, 2]);
}

/// Codex R1.3: inline (OpenCode) records queued project-only exist ONLY in
/// the queue — the record counter already advanced past them. After a repo
/// switch they must reach the repo target exactly once, with their original
/// offset, ahead of the live records.
#[test]
fn inline_records_queued_project_only_survive_a_repo_switch_exactly_once() {
    let fx = Fixture::new();
    let pid = uuid::Uuid::from_u128(0x1A1);
    fx.write_session_state(pid, false);

    // Offline, project-only: records 1-2 queued, counter advances to 2.
    let (base, rx) = spawn_server(vec![unavailable(), accepted()]);
    fx.run_inline_hook(&base, 1..=2);
    assert_eq!(captured(&rx).len(), 1);
    let q = queued(&fx.project_queue(pid));
    assert_eq!(q.len(), 1);
    assert_eq!(line_ns(&q[0]), vec![1, 2]);
    assert_eq!(q[0].transcript_offset, Some(0));
    assert_eq!(fx.offset(), Some(2), "the inline counter advances on queue");

    // Repo bound; records 3-4 arrive live.
    fx.write_session_state(pid, true);
    let (base, rx) = spawn_server(vec![accepted(), accepted(), accepted()]);
    fx.run_inline_hook(&base, 3..=4);
    let reqs = captured(&rx);
    assert_eq!(reqs.len(), 2, "adopted queue + live: {reqs:?}");
    let repo_path = format!("POST /api/v1/projects/{pid}/stream?repo_id={REPO_ID} ");
    assert!(
        reqs.iter().all(|r| r.line.starts_with(&repo_path)),
        "{reqs:?}"
    );
    assert_eq!(line_ns(&reqs[0].body), vec![1, 2]);
    assert_eq!(reqs[0].body.transcript_offset, Some(0), "original offset");
    assert_eq!(line_ns(&reqs[1].body), vec![3, 4]);
    assert_eq!(reqs[1].body.transcript_offset, Some(2));
    assert!(queued(&fx.project_queue(pid)).is_empty());
    assert!(queued(&fx.repo_queue()).is_empty());
    assert_eq!(fx.offset(), Some(4));
}

/// Neither a repo nor a project: the hook no-ops, sends nothing, queues
/// nothing.
#[test]
fn nothing_bound_is_a_no_op() {
    let fx = Fixture::new();
    fx.append_transcript(1..=1);
    let (base, rx) = spawn_server(vec![accepted()]);
    fx.run_hook(&base);
    assert!(captured(&rx).is_empty(), "no request without any binding");
    let queues: Vec<_> = std::fs::read_dir(fx.session_dir())
        .map(|d| {
            d.filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().starts_with("pending"))
                .collect()
        })
        .unwrap_or_default();
    assert!(queues.is_empty(), "{queues:?}");
}

impl Fixture {
    /// Run `tracevault <args> --session-id <SESSION_ID>` from the umbrella
    /// with the same isolated environment as the hook; returns stdout.
    fn run_status(&self, args: &[&str]) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_tracevault"))
            .args(args)
            .args(["--session-id", SESSION_ID])
            .current_dir(&self.umbrella)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_STATE_HOME", &self.state)
            // No server: the recording verdict is local, like the hook's gate.
            .env_remove("TRACEVAULT_SERVER_URL")
            .env_remove("TRACEVAULT_API_KEY")
            .env_remove("TRACEVAULT_SESSION_ID")
            .env_remove("AMP_CURRENT_THREAD_ID")
            .env_remove("TRACEVAULT_PROJECT")
            .env_remove("TRACEVAULT_PROJECT_ATTRIBUTION")
            .output()
            .expect("failed to run the tracevault binary");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

/// `repo status` and `project status` report what ingest does: project-only
/// while no repo is bound, repo-scoped once `repo switch` binds one.
#[test]
fn repo_and_project_status_report_project_only_recording() {
    let fx = Fixture::new();
    let pid = uuid::Uuid::from_u128(0x57A7);
    fx.write_session_state(pid, false);
    let only = format!("recording: project {pid} only");
    for args in [&["repo", "status"][..], &["project", "status"][..]] {
        let out = fx.run_status(args);
        assert!(out.contains(&only), "{args:?}:\n{out}");
    }

    fx.write_session_state(pid, true);
    let scoped = format!("recording: repo {REPO_ID} in project {pid}");
    for args in [&["repo", "status"][..], &["project", "status"][..]] {
        let out = fx.run_status(args);
        assert!(out.contains(&scoped), "{args:?}:\n{out}");
    }
}
