//! VIS-1078 end to end: Claude Code subagent transcripts
//! (`<session>/subagents/agent-<id>.jsonl`) reach the server through the real
//! `tracevault stream` hooks and `tracevault flush`: under the parent session,
//! one file per request, each line once, and a session without subagents
//! sends what it always did.
//!
//! Runs the hook binary (via `CARGO_BIN_EXE_*`) against a raw-TCP mock server,
//! isolated to a tempdir like `stream_refused_project_test.rs`.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tracevault_protocol::streaming::{StreamEventRequest, StreamEventType};

const REPO_ID: &str = "11111111-1111-1111-1111-111111111111";
const SESSION_ID: &str = "vis1078-parent-session";

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
/// `ACCEPT_DEADLINE`, answering each with an accepted stream response and
/// capturing its JSON body.
fn spawn_server() -> (String, mpsc::Receiver<StreamEventRequest>) {
    spawn_server_failing(&[])
}

/// [`spawn_server`], but the requests at the 0-based positions in `fail` get
/// a 503 and are not captured: only what the server accepted is.
fn spawn_server_failing(fail: &[usize]) -> (String, mpsc::Receiver<StreamEventRequest>) {
    let fail = fail.to_vec();
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
        let mut n = 0;
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
            assert!(
                request_line.starts_with(&format!("POST /api/v1/repos/{REPO_ID}/stream ")),
                "{request_line}"
            );
            let failed = fail.contains(&n);
            n += 1;
            let mut stream = reader.into_inner();
            if failed {
                let _ = stream.write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                continue;
            }
            if let Ok(req) = serde_json::from_slice(&body) {
                let _ = tx.send(req);
            }
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{addr}"), rx)
}

/// A URL nothing listens on: every send fails and is queued.
fn dead_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}", listener.local_addr().unwrap())
}

/// A temp git repo bound to `REPO_ID`, isolated home/config/state dirs, and a
/// Claude Code project dir holding the session transcript and its subagents.
struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    repo: PathBuf,
    transcript: PathBuf,
    subagents: PathBuf,
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
        let project = tmp.path().join("claude-project");
        let transcript = project.join(format!("{SESSION_ID}.jsonl"));
        let subagents = project.join(SESSION_ID).join("subagents");
        std::fs::create_dir_all(&project).unwrap();
        Fixture {
            _tmp: tmp,
            home,
            state,
            repo,
            transcript,
            subagents,
        }
    }

    fn session_dir(&self) -> PathBuf {
        self.repo
            .join(".tracevault")
            .join("sessions")
            .join(SESSION_ID)
    }

    fn main_line(&self, n: usize) {
        append(
            &self.transcript,
            &format!("{}\n", serde_json::json!({"type": "assistant", "n": n})),
        );
    }

    fn subagent_line(&self, agent: &str, n: usize) {
        std::fs::create_dir_all(&self.subagents).unwrap();
        append(
            &self.subagents.join(format!("agent-{agent}.jsonl")),
            &format!(
                "{}\n",
                serde_json::json!({"type": "assistant", "agentId": agent, "sessionId": SESSION_ID, "n": n})
            ),
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
            .env_remove("TRACEVAULT_SESSION_ID")
            .env_remove("TRACEVAULT_PROJECT")
            .env_remove("TRACEVAULT_PROJECT_ATTRIBUTION");
        cmd
    }

    /// Run `tracevault stream --event <event>` once against `base`.
    fn hook(&self, base: &str, event: &str, hook_event_name: &str) {
        let out = self
            .spawn_hook(base, event, hook_event_name)
            .wait_with_output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    }

    /// Start `tracevault stream --event <event>` without waiting for it.
    fn spawn_hook(&self, base: &str, event: &str, hook_event_name: &str) -> std::process::Child {
        let payload = serde_json::json!({
            "session_id": SESSION_ID,
            "transcript_path": self.transcript.to_string_lossy(),
            "cwd": self.repo.to_string_lossy(),
            "hook_event_name": hook_event_name,
            "tool_name": "Bash",
            "tool_use_id": format!("toolu_{}", uuid::Uuid::new_v4().simple()),
        });
        let mut child = self
            .command(base)
            .args(["stream", "--event", event])
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
        child
    }

    fn queue(&self) -> PathBuf {
        self.session_dir().join(format!("pending-{REPO_ID}.jsonl"))
    }

    fn queued(&self) -> Vec<StreamEventRequest> {
        std::fs::read_to_string(self.queue())
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn main_offset(&self) -> Option<String> {
        std::fs::read_to_string(self.session_dir().join(".stream_offset")).ok()
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

fn agent_of(line: &serde_json::Value) -> Option<&str> {
    line.get("agentId").and_then(|a| a.as_str())
}

/// `(agentId, n)` of every line sent, after asserting that each request is
/// under the parent session and holds the lines of one file only.
fn sent_lines(reqs: &[StreamEventRequest]) -> Vec<(Option<String>, u64)> {
    let mut sent = Vec::new();
    for req in reqs {
        assert_eq!(req.session_id, SESSION_ID);
        let lines = lines(req);
        let Some(first) = lines.first() else {
            continue;
        };
        assert!(
            lines.iter().all(|l| agent_of(l) == agent_of(first)),
            "a request mixes transcripts: {lines:?}"
        );
        if agent_of(first).is_some() {
            assert_eq!(req.event_type, StreamEventType::Transcript);
        }
        sent.extend(
            lines
                .iter()
                .map(|l| (agent_of(l).map(String::from), l["n"].as_u64().unwrap())),
        );
    }
    sent.sort();
    sent
}

fn expected(lines: &[(Option<&str>, u64)]) -> Vec<(Option<String>, u64)> {
    let mut v: Vec<_> = lines
        .iter()
        .map(|(a, n)| (a.map(String::from), *n))
        .collect();
    v.sort();
    v
}

#[test]
fn hooks_send_every_subagent_line_once_under_the_parent_session() {
    let fx = Fixture::new();
    let (base, rx) = spawn_server();

    fx.main_line(0);
    fx.subagent_line("a1", 0);
    fx.subagent_line("a1", 1);
    fx.subagent_line("b2", 0);
    fx.hook(&base, "post-tool-use", "PostToolUse");

    fx.main_line(1);
    fx.subagent_line("a1", 2);
    fx.hook(&base, "post-tool-use", "PostToolUse");

    // SubagentStop sends the subagent files only: the main transcript's new
    // line stays unread and its offset unchanged.
    fx.main_line(2);
    fx.subagent_line("b2", 1);
    let before = fx.main_offset();
    let first_two_hooks = captured(&rx);
    fx.hook(&base, "subagent-stop", "SubagentStop");
    let subagent_stop = captured(&rx);
    assert_eq!(fx.main_offset(), before);
    assert_eq!(subagent_stop.len(), 1);
    assert_eq!(
        sent_lines(&subagent_stop),
        expected(&[(Some("b2"), 1)]),
        "only the subagent's new line"
    );

    fx.subagent_line("a1", 3);
    fx.hook(&base, "stop", "Stop");
    fx.hook(&base, "stop", "Stop");

    let all: Vec<_> = first_two_hooks
        .into_iter()
        .chain(subagent_stop)
        .chain(captured(&rx))
        .collect();
    assert_eq!(
        sent_lines(&all),
        expected(&[
            (None, 0),
            (None, 1),
            (None, 2),
            (Some("a1"), 0),
            (Some("a1"), 1),
            (Some("a1"), 2),
            (Some("a1"), 3),
            (Some("b2"), 0),
            (Some("b2"), 1),
        ]),
        "each line exactly once"
    );
}

#[test]
fn a_session_without_subagents_sends_only_its_event() {
    let fx = Fixture::new();
    let (base, rx) = spawn_server();
    fx.main_line(0);
    fx.main_line(1);

    fx.hook(&base, "post-tool-use", "PostToolUse");

    let reqs = captured(&rx);
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].event_type, StreamEventType::ToolUse);
    assert_eq!(reqs[0].transcript_offset, Some(0));
    assert_eq!(sent_lines(&reqs), expected(&[(None, 0), (None, 1)]));
    assert!(!fx.session_dir().join(".subagent_offsets").exists());
}

#[test]
fn flush_delivers_queued_subagent_lines() {
    let fx = Fixture::new();
    fx.main_line(0);
    fx.subagent_line("a1", 0);
    fx.subagent_line("b2", 0);
    fx.subagent_line("b2", 1);
    fx.hook(&dead_server(), "stop", "Stop");

    let (base, rx) = spawn_server();
    let out = fx.command(&base).arg("flush").output().unwrap();
    assert!(out.status.success(), "{out:?}");

    let reqs = captured(&rx);
    assert_eq!(
        sent_lines(&reqs),
        expected(&[(None, 0), (Some("a1"), 0), (Some("b2"), 0), (Some("b2"), 1)])
    );
    for req in reqs
        .iter()
        .filter(|r| r.event_type == StreamEventType::Transcript)
    {
        assert_eq!(req.transcript_offset, Some(0), "offset within its own file");
    }
}

/// A subagent file whose offset is corrupt is skipped, untouched; the hook
/// still sends the other files and advances their offsets.
#[test]
fn a_corrupt_subagent_offset_does_not_block_the_other_files() {
    let fx = Fixture::new();
    fx.main_line(0);
    fx.subagent_line("a1", 0);
    fx.subagent_line("b2", 0);
    let offsets = fx.session_dir().join(".subagent_offsets");
    std::fs::create_dir_all(&offsets).unwrap();
    std::fs::write(offsets.join("agent-a1"), "garbage").unwrap();

    let (base, rx) = spawn_server();
    fx.hook(&base, "post-tool-use", "PostToolUse");

    assert_eq!(
        sent_lines(&captured(&rx)),
        expected(&[(None, 0), (Some("b2"), 0)])
    );
    assert_eq!(
        std::fs::read_to_string(offsets.join("agent-a1")).unwrap(),
        "garbage"
    );
    let b2_len = std::fs::metadata(fx.subagents.join("agent-b2.jsonl"))
        .unwrap()
        .len();
    assert_eq!(
        std::fs::read_to_string(offsets.join("agent-b2")).unwrap(),
        b2_len.to_string()
    );
}

/// A split subagent batch whose later part fails is queued; the subagent
/// then writes more and the next hook re-reads the file, splitting it at other
/// boundaries. Every line must still reach the server under one chunk_index
/// (`transcript_offset` + its index in the request), whichever request carried
/// it, so the server's dedup sees one chunk per line.
#[test]
fn a_split_subagent_batch_keeps_its_chunk_indexes_across_a_failed_part_and_a_retry() {
    let fx = Fixture::new();
    let big_line = |n: usize| {
        format!(
            "{}\n",
            serde_json::json!({"agentId": "a1", "sessionId": SESSION_ID, "n": n, "text": "y".repeat(5_000)})
        )
    };
    std::fs::create_dir_all(&fx.subagents).unwrap();
    let a1 = fx.subagents.join("agent-a1.jsonl");
    fx.main_line(0);
    for n in 0..300 {
        append(&a1, &big_line(n));
    }

    // Request 0 is the hook's own event, 1 the first a1 part; 2 fails, and
    // every part after it is queued.
    let (base, rx) = spawn_server_failing(&[2]);
    fx.hook(&base, "post-tool-use", "PostToolUse");
    let mut accepted = captured(&rx);
    assert_eq!(accepted.len(), 2, "the event and the first part");

    for n in 300..350 {
        append(&a1, &big_line(n));
    }
    let (base, rx) = spawn_server();
    fx.hook(&base, "stop", "Stop");
    accepted.extend(captured(&rx));

    let mut chunks: std::collections::BTreeMap<u64, std::collections::BTreeSet<i64>> =
        Default::default();
    for req in accepted
        .iter()
        .filter(|r| r.event_type == StreamEventType::Transcript)
    {
        let offset = req.transcript_offset.unwrap();
        for (i, line) in lines(req).iter().enumerate() {
            assert_eq!(agent_of(line), Some("a1"));
            chunks
                .entry(line["n"].as_u64().unwrap())
                .or_default()
                .insert(offset + i as i64);
        }
    }
    assert_eq!(
        chunks.keys().copied().collect::<Vec<_>>(),
        (0..350).collect::<Vec<_>>(),
        "every line delivered"
    );
    for (n, indexes) in &chunks {
        assert_eq!(indexes.len(), 1, "line {n} sent under {indexes:?}");
    }
}

/// A server that accepts connections but never answers must not hold a
/// final hook past the agent's 10 s hook timeout, which would kill it after
/// it emptied the queue file and before the subagent sweep: the queued
/// events are re-queued and the new subagent lines queued, in time.
#[test]
fn a_stalled_server_cannot_make_a_final_hook_lose_queued_or_new_subagent_lines() {
    let fx = Fixture::new();
    fx.main_line(0);
    fx.subagent_line("a1", 0);
    fx.hook(&dead_server(), "post-tool-use", "PostToolUse");
    assert_eq!(fx.queued().len(), 2, "the event and the a1 batch");

    // Connections complete in the kernel backlog; nothing ever answers.
    let stalled = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", stalled.local_addr().unwrap());
    fx.subagent_line("a1", 1);
    let started = Instant::now();
    fx.hook(&base, "stop", "Stop");
    assert!(
        started.elapsed() < Duration::from_secs(9),
        "took {:?}",
        started.elapsed()
    );

    let queued = fx.queued();
    let sent = sent_lines(&queued);
    for line in [(None, 0), (Some("a1"), 0), (Some("a1"), 1)] {
        assert!(
            expected(&[line]).iter().all(|l| sent.contains(l)),
            "{line:?} not queued: {sent:?}"
        );
    }
    drop(stalled);
}

/// Hooks of one session overlap (parallel tool calls, a SubagentStop during a
/// PostToolUse) while subagents append and the server fails some requests.
/// Once a final hook and `flush` have run, every subagent line has reached
/// the server under exactly one chunk_index.
#[test]
fn overlapping_hooks_deliver_every_subagent_line_under_one_chunk_index() {
    let fx = Fixture::new();
    fx.main_line(0);
    let mut accepted = Vec::new();
    let mut next = [0usize; 2];
    for round in 0..4 {
        for (i, agent) in ["a1", "b2"].iter().enumerate() {
            for _ in 0..3 {
                fx.subagent_line(agent, next[i]);
                next[i] += 1;
            }
        }
        let (base, rx) = if round % 2 == 1 {
            spawn_server_failing(&[1, 4])
        } else {
            spawn_server()
        };
        let hooks: Vec<_> = [
            ("post-tool-use", "PostToolUse"),
            ("post-tool-use", "PostToolUse"),
            ("subagent-stop", "SubagentStop"),
        ]
        .iter()
        .map(|(event, name)| fx.spawn_hook(&base, event, name))
        .collect();
        // A subagent keeps writing while they run.
        fx.subagent_line("a1", next[0]);
        next[0] += 1;
        for hook in hooks {
            let out = hook.wait_with_output().unwrap();
            assert!(out.status.success(), "{out:?}");
        }
        accepted.extend(captured(&rx));
    }
    let (base, rx) = spawn_server();
    fx.hook(&base, "stop", "Stop");
    let out = fx.command(&base).arg("flush").output().unwrap();
    assert!(out.status.success(), "{out:?}");
    accepted.extend(captured(&rx));

    let mut chunks: std::collections::BTreeMap<(String, u64), std::collections::BTreeSet<i64>> =
        Default::default();
    for req in &accepted {
        let offset = req.transcript_offset.unwrap_or(0);
        for (i, line) in lines(req).iter().enumerate() {
            if let Some(agent) = agent_of(line) {
                assert_eq!(req.session_id, SESSION_ID);
                chunks
                    .entry((agent.to_string(), line["n"].as_u64().unwrap()))
                    .or_default()
                    .insert(offset + i as i64);
            }
        }
    }
    let mut want: Vec<(String, u64)> = (0..next[0] as u64)
        .map(|n| ("a1".to_string(), n))
        .chain((0..next[1] as u64).map(|n| ("b2".to_string(), n)))
        .collect();
    want.sort();
    assert_eq!(chunks.keys().cloned().collect::<Vec<_>>(), want);
    for (line, indexes) in &chunks {
        assert_eq!(indexes.len(), 1, "{line:?} sent under {indexes:?}");
    }
}
