//! VIS-1108 end to end: Claude Code's `cost-state` line, written at exit
//! after `Stop`, reaches the server through the `SessionEnd` hook
//! (`tracevault stream --event session-end`) or, when that hook did not send
//! it, through the catch-up sweep in `tracevault session-start`. `stop` keeps
//! ending the session for every agent.
//!
//! Runs the hook binary (via `CARGO_BIN_EXE_*`) against a raw-TCP mock server,
//! isolated to a tempdir like `subagent_stream_e2e_test.rs`.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tracevault_protocol::streaming::{StreamEventRequest, StreamEventType};

const REPO_ID: &str = "11111111-1111-1111-1111-111111111111";
const SESSION_ID: &str = "vis1108-session";

const COST_STATE: &str = r#"{"type":"cost-state","sessionId":"vis1108-session","totalCostUSD":0.1249792,"modelUsage":{"claude-opus-5-5":{"outputTokens":4,"costUSD":0.1249792}},"hasUnknownModelCost":false}"#;

/// How long the mock server waits for a connection that never arrives
/// before its thread gives up.
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

/// Accept every request until no new connection arrives for
/// `ACCEPT_DEADLINE`. A stream POST is answered as accepted and its body
/// captured; anything else (`session-start`'s policy and binding lookups)
/// gets a 404.
fn spawn_server() -> (String, mpsc::Receiver<StreamEventRequest>) {
    spawn_server_with(false)
}

/// [`spawn_server`], but with `failing` every stream POST is captured and
/// then answered with a 503.
fn spawn_server_with(failing: bool) -> (String, mpsc::Receiver<StreamEventRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    let body = serde_json::to_string(&tracevault_protocol::streaming::StreamEventResponse {
        session_db_id: uuid::Uuid::nil(),
        event_db_id: Some(uuid::Uuid::nil()),
        status: "accepted".to_string(),
    })
    .unwrap();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    thread::spawn(move || {
        while let Some(stream) = accept_with_deadline(&listener) {
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
            let mut stream = reader.into_inner();
            // Repo- or project-scoped.
            if !(request_line.starts_with("POST /api/v1/") && request_line.contains("/stream")) {
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                continue;
            }
            if let Ok(req) = serde_json::from_slice(&body) {
                let _ = tx.send(req);
            }
            if failing {
                let _ = stream.write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                continue;
            }
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{addr}"), rx)
}

/// A URL nothing listens on: every send fails.
fn dead_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}", listener.local_addr().unwrap())
}

/// A server that accepts every connection and never answers, for
/// `ACCEPT_DEADLINE` after the last one.
fn stalled_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        let mut held = Vec::new();
        while let Some(stream) = accept_with_deadline(&listener) {
            held.push(stream);
        }
    });
    format!("http://{addr}")
}

/// A temp git repo bound to `REPO_ID`, isolated home/config/state dirs, and a
/// Claude Code project dir for the session transcripts.
struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    repo: PathBuf,
    project: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let repo = tmp.path().join("repo");
        let project = tmp.path().join("claude-project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&project).unwrap();
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
            project,
        }
    }

    fn transcript(&self, session: &str) -> PathBuf {
        self.project.join(format!("{session}.jsonl"))
    }

    fn session_dir(&self, session: &str) -> PathBuf {
        self.repo.join(".tracevault").join("sessions").join(session)
    }

    fn line(&self, session: &str, line: &str) {
        append(&self.transcript(session), &format!("{line}\n"));
    }

    fn assistant(&self, session: &str, n: usize) {
        self.line(
            session,
            &serde_json::json!({"type": "assistant", "n": n}).to_string(),
        );
    }

    fn command(&self, base: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tracevault"));
        cmd.current_dir(&self.repo)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_STATE_HOME", &self.state)
            .env("TRACEVAULT_SERVER_URL", base)
            .env("TRACEVAULT_API_KEY", "tvk_test")
            .env_remove("CLAUDE_ENV_FILE")
            .env_remove("TRACEVAULT_SESSION_ID")
            .env_remove("TRACEVAULT_PROJECT")
            .env_remove("TRACEVAULT_PROJECT_ATTRIBUTION");
        cmd
    }

    fn run(&self, base: &str, args: &[&str], payload: serde_json::Value) -> Output {
        let mut child = self
            .command(base)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to run the tracevault binary");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        out
    }

    /// Run `tracevault stream --event <event>` for `session`.
    fn hook(&self, base: &str, session: &str, event: &str, hook_event_name: &str) -> Output {
        self.run(
            base,
            &["stream", "--event", event],
            serde_json::json!({
                "session_id": session,
                "transcript_path": self.transcript(session).to_string_lossy(),
                "cwd": self.repo.to_string_lossy(),
                "hook_event_name": hook_event_name,
            }),
        )
    }

    /// Run `tracevault session-start` for `session`.
    fn session_start(&self, base: &str, session: &str) -> Output {
        self.run(
            base,
            &["session-start"],
            serde_json::json!({
                "session_id": session,
                "transcript_path": self.transcript(session).to_string_lossy(),
                "cwd": self.repo.to_string_lossy(),
                "hook_event_name": "SessionStart",
                "source": "startup",
            }),
        )
    }

    fn offset(&self, session: &str) -> Option<u64> {
        std::fs::read_to_string(self.session_dir(session).join(".stream_offset"))
            .ok()
            .map(|s| s.trim().parse().unwrap())
    }

    fn transcript_len(&self, session: &str) -> u64 {
        std::fs::metadata(self.transcript(session)).unwrap().len()
    }
}

fn append(path: &Path, text: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

/// Everything the server received. Called after the process exited, so every
/// request it made is already in the channel.
fn captured(rx: &mpsc::Receiver<StreamEventRequest>) -> Vec<StreamEventRequest> {
    rx.try_iter().collect()
}

fn lines(req: &StreamEventRequest) -> Vec<serde_json::Value> {
    req.transcript_lines.clone().unwrap_or_default()
}

fn types(req: &StreamEventRequest) -> Vec<String> {
    lines(req)
        .iter()
        .map(|l| l["type"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// A session whose turn `Stop` already sent, then the `cost-state` Claude
/// Code writes at exit.
fn ended_session(fx: &Fixture, base: &str, session: &str) {
    fx.assistant(session, 1);
    fx.hook(base, session, "stop", "Stop");
    fx.line(session, COST_STATE);
}

#[test]
fn session_end_sends_the_final_cost_state_without_ending_the_session_again() {
    let fx = Fixture::new();
    let (base, rx) = spawn_server();

    fx.assistant(SESSION_ID, 1);
    fx.hook(&base, SESSION_ID, "stop", "Stop");
    let stop = captured(&rx);
    assert_eq!(stop.len(), 1);
    assert_eq!(stop[0].event_type, StreamEventType::SessionEnd);
    let after_stop = fx.offset(SESSION_ID).unwrap();

    fx.line(SESSION_ID, COST_STATE);
    let out = fx.hook(&base, SESSION_ID, "session-end", "SessionEnd");
    let sent = captured(&rx);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].event_type, StreamEventType::Transcript);
    assert_eq!(sent[0].hook_event_name.as_deref(), Some("SessionEnd"));
    assert_eq!(sent[0].tool.as_deref(), Some("claude-code"));
    assert_eq!(types(&sent[0]), ["cost-state"]);
    assert_eq!(lines(&sent[0])[0]["totalCostUSD"], 0.1249792);
    assert_eq!(sent[0].transcript_offset, Some(after_stop as i64));
    assert_eq!(fx.offset(SESSION_ID), Some(fx.transcript_len(SESSION_ID)));
    let response: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(response.is_object(), "the hook answers like every hook");
}

#[test]
fn session_end_sends_every_new_line_from_the_offset() {
    let fx = Fixture::new();
    let (base, rx) = spawn_server();
    fx.assistant(SESSION_ID, 1);
    fx.hook(&base, SESSION_ID, "post-tool-use", "PostToolUse");
    captured(&rx);

    // Lines `Stop` did not send, and one after the report (2 of 40 sampled
    // transcripts have lines after it).
    fx.assistant(SESSION_ID, 2);
    fx.line(SESSION_ID, COST_STATE);
    fx.assistant(SESSION_ID, 3);
    fx.hook(&base, SESSION_ID, "session-end", "SessionEnd");

    let sent = captured(&rx);
    assert_eq!(sent.len(), 1);
    assert_eq!(types(&sent[0]), ["assistant", "cost-state", "assistant"]);
    assert_eq!(fx.offset(SESSION_ID), Some(fx.transcript_len(SESSION_ID)));
}

#[test]
fn session_end_without_a_new_line_sends_nothing() {
    let fx = Fixture::new();
    let (base, rx) = spawn_server();
    fx.assistant(SESSION_ID, 1);
    fx.hook(&base, SESSION_ID, "stop", "Stop");
    captured(&rx);
    let offset = fx.offset(SESSION_ID);

    fx.hook(&base, SESSION_ID, "session-end", "SessionEnd");
    assert!(captured(&rx).is_empty());
    assert_eq!(fx.offset(SESSION_ID), offset);
}

#[test]
fn session_end_that_cannot_send_leaves_the_offset_for_a_retry() {
    let fx = Fixture::new();
    let (base, rx) = spawn_server();
    ended_session(&fx, &base, SESSION_ID);
    captured(&rx);
    let offset = fx.offset(SESSION_ID);

    fx.hook(&dead_server(), SESSION_ID, "session-end", "SessionEnd");
    assert_eq!(fx.offset(SESSION_ID), offset, "the sweep can still find it");
}

#[test]
fn stop_still_ends_the_session_for_every_agent() {
    for agent in ["claude-code", "codex", "gsd", "opencode"] {
        let fx = Fixture::new();
        let (base, rx) = spawn_server();
        fx.assistant(SESSION_ID, 1);
        let mut payload = serde_json::json!({
            "session_id": SESSION_ID,
            "transcript_path": fx.transcript(SESSION_ID).to_string_lossy(),
            "cwd": fx.repo.to_string_lossy(),
            "hook_event_name": "Stop",
        });
        if agent == "opencode" {
            payload["transcript_path"] = "".into();
            payload["transcript_records"] = serde_json::json!([]);
        }
        fx.run(
            &base,
            &["stream", "--event", "stop", "--agent", agent],
            payload,
        );
        let sent = captured(&rx);
        assert_eq!(sent.len(), 1, "{agent}: {sent:?}");
        assert_eq!(sent[0].event_type, StreamEventType::SessionEnd, "{agent}");
        assert_eq!(sent[0].tool.as_deref(), Some(agent), "{agent}");
    }
}

#[test]
fn session_start_sends_what_an_earlier_session_end_left_unsent() {
    let fx = Fixture::new();
    let (base, rx) = spawn_server();
    // Ended, but its SessionEnd hook never ran (failed, or ended before the
    // upgrade that installs it).
    ended_session(&fx, &base, "ended");
    // Still running in another terminal: unsent lines but no report.
    fx.assistant("running", 1);
    fx.hook(&base, "running", "post-tool-use", "PostToolUse");
    fx.assistant("running", 2);
    let running_offset = fx.offset("running");
    captured(&rx);

    let out = fx.session_start(&base, "new-session");

    let sent = captured(&rx);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].session_id, "ended");
    assert_eq!(sent[0].event_type, StreamEventType::Transcript);
    assert_eq!(sent[0].hook_event_name.as_deref(), Some("SessionEnd"));
    assert_eq!(types(&sent[0]), ["cost-state"]);
    assert_eq!(
        sent[0].billing_mode, None,
        "this process's credentials say nothing about that session's"
    );
    assert_eq!(fx.offset("ended"), Some(fx.transcript_len("ended")));
    assert_eq!(
        fx.offset("running"),
        running_offset,
        "left to its own hooks"
    );
    let response: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(response.is_object(), "session-start still answers");

    // Sent once: the next session start finds nothing to do.
    fx.session_start(&base, "another-session");
    assert!(captured(&rx).is_empty());
}

#[test]
fn session_start_stops_at_the_first_session_the_server_refuses() {
    let fx = Fixture::new();
    let (base, rx) = spawn_server();
    ended_session(&fx, &base, "older");
    ended_session(&fx, &base, "newer");
    let now = std::time::SystemTime::now();
    for (session, age) in [("older", 120), ("newer", 60)] {
        std::fs::File::options()
            .write(true)
            .open(fx.transcript(session))
            .unwrap()
            .set_modified(now - Duration::from_secs(age))
            .unwrap();
    }
    captured(&rx);
    let offsets = (fx.offset("older"), fx.offset("newer"));

    let (failing, rx) = spawn_server_with(true);
    fx.session_start(&failing, "new-session");

    let tried = captured(&rx);
    assert_eq!(tried.len(), 1, "the older session is not tried: {tried:?}");
    assert_eq!(tried[0].session_id, "newer", "most recent first");
    assert_eq!((fx.offset("older"), fx.offset("newer")), offsets);
    let queued = fx
        .session_dir("newer")
        .join(format!("pending-{REPO_ID}.jsonl"));
    assert!(
        std::fs::read_to_string(queued)
            .unwrap()
            .contains("cost-state"),
        "the refused part is queued"
    );
    assert!(!fx
        .session_dir("older")
        .join(format!("pending-{REPO_ID}.jsonl"))
        .exists());
}

#[test]
fn session_start_with_a_process_scoped_project_leaves_other_sessions_alone() {
    let fx = Fixture::new();
    let (base, rx) = spawn_server();
    ended_session(&fx, &base, "ended");
    captured(&rx);
    let offset = fx.offset("ended");

    let mut child = fx
        .command(&base)
        .env("TRACEVAULT_PROJECT", "22222222-2222-2222-2222-222222222222")
        .arg("session-start")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            serde_json::json!({
                "session_id": "new-session",
                "cwd": fx.repo.to_string_lossy(),
                "hook_event_name": "SessionStart",
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    assert!(child.wait_with_output().unwrap().status.success());

    assert!(captured(&rx).is_empty());
    assert_eq!(fx.offset("ended"), offset);
}

/// Set (or, with `None`, remove) the attribution `session`'s hooks recorded.
fn set_recorded_attribution(fx: &Fixture, session: &str, attribution: Option<serde_json::Value>) {
    let path = fx.session_dir(session).join("metadata.json");
    let mut meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    match attribution {
        Some(a) => meta["attribution"] = a,
        None => {
            meta.as_object_mut().unwrap().remove("attribution");
        }
    }
    std::fs::write(&path, meta.to_string()).unwrap();
}

#[test]
fn hooks_record_the_attribution_they_send_under() {
    let fx = Fixture::new();
    let (base, rx) = spawn_server();
    ended_session(&fx, &base, "ended");
    captured(&rx);
    let meta: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fx.session_dir("ended").join("metadata.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        meta["attribution"],
        serde_json::json!({"repo_id": REPO_ID, "project_id": null})
    );
}

#[test]
fn session_start_leaves_a_session_recorded_under_another_attribution() {
    let fx = Fixture::new();
    let (base, rx) = spawn_server();
    ended_session(&fx, &base, "ended");
    captured(&rx);
    // Its hooks sent under a project that no longer resolves for it (the
    // default project was switched since).
    set_recorded_attribution(
        &fx,
        "ended",
        Some(serde_json::json!({
            "repo_id": REPO_ID,
            "project_id": "22222222-2222-2222-2222-222222222222",
        })),
    );
    let offset = fx.offset("ended");

    fx.session_start(&base, "new-session");
    assert!(captured(&rx).is_empty());
    assert_eq!(fx.offset("ended"), offset);
}

#[test]
fn session_start_sends_a_repo_session_an_older_cli_recorded() {
    let fx = Fixture::new();
    let (base, rx) = spawn_server();
    ended_session(&fx, &base, "ended");
    captured(&rx);
    set_recorded_attribution(&fx, "ended", None);

    fx.session_start(&base, "new-session");
    let sent = captured(&rx);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(types(&sent[0]), ["cost-state"]);
}

/// Set the transcript of `session` as last written `age_secs` ago.
fn age(fx: &Fixture, session: &str, age_secs: u64) {
    std::fs::File::options()
        .write(true)
        .open(fx.transcript(session))
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(age_secs))
        .unwrap();
}

#[test]
fn sessions_skipped_for_their_attribution_do_not_hold_back_older_ones() {
    // As many newer sessions as one run sends, each recorded under a project
    // that no longer resolves, and an older one that can be sent.
    const SENT_PER_RUN: usize = 10;
    let fx = Fixture::new();
    let (base, rx) = spawn_server();
    ended_session(&fx, &base, "older");
    age(&fx, "older", 600);
    for i in 0..SENT_PER_RUN {
        let id = format!("moved-{i}");
        ended_session(&fx, &base, &id);
        age(&fx, &id, 60 + i as u64);
        set_recorded_attribution(
            &fx,
            &id,
            Some(serde_json::json!({
                "repo_id": REPO_ID,
                "project_id": "22222222-2222-2222-2222-222222222222",
            })),
        );
    }
    captured(&rx);

    fx.session_start(&base, "new-session");
    let sent = captured(&rx);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].session_id, "older");
}

#[test]
fn one_session_start_sends_at_most_ten_sessions() {
    let fx = Fixture::new();
    let (base, rx) = spawn_server();
    for i in 0..12 {
        let id = format!("s{i:02}");
        ended_session(&fx, &base, &id);
        age(&fx, &id, 1000 - i as u64);
    }
    captured(&rx);

    fx.session_start(&base, "new-session");
    let mut sent: Vec<String> = captured(&rx).into_iter().map(|r| r.session_id).collect();
    sent.sort();
    let expected: Vec<String> = (2..12).map(|i| format!("s{i:02}")).collect();
    assert_eq!(sent, expected, "the 10 most recent");

    fx.session_start(&base, "next-session");
    let sent: Vec<String> = captured(&rx).into_iter().map(|r| r.session_id).collect();
    assert_eq!(sent, ["s01", "s00"], "the rest next time");
}

#[test]
fn session_start_stays_inside_its_timeout_when_the_server_stalls() {
    let fx = Fixture::new();
    let (base, rx) = spawn_server();
    ended_session(&fx, &base, "ended");
    captured(&rx);
    let offset = fx.offset("ended");

    let started = Instant::now();
    let out = fx.session_start(&stalled_server(), "new-session");
    let took = started.elapsed();
    assert!(took < Duration::from_secs(10), "took {took:?}");
    let response: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(response.is_object());
    assert_eq!(fx.offset("ended"), offset);
}

#[test]
fn session_start_never_fails_on_a_dead_server() {
    let fx = Fixture::new();
    let (base, rx) = spawn_server();
    ended_session(&fx, &base, "ended");
    captured(&rx);
    let offset = fx.offset("ended");

    let started = Instant::now();
    let out = fx.session_start(&dead_server(), "new-session");
    assert!(
        started.elapsed() < Duration::from_secs(9),
        "within the hook timeout"
    );
    let response: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(response.is_object());
    assert_eq!(
        fx.offset("ended"),
        offset,
        "retried by a later session start"
    );
}
