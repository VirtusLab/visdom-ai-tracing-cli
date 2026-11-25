//! A hook's memory and its queue must not depend on how long the server has
//! been away. Before, every hook that failed to send queued the whole unsent
//! end of the transcript again and read the whole queue into memory: a
//! transcript of a few megabytes grew a queue of gigabytes. Now a line is
//! queued once and the transcript's read offset moves past it.
//!
//! Runs the hook binary (via `CARGO_BIN_EXE_*`) against a raw-TCP mock server,
//! isolated to a tempdir like `session_end_e2e_test.rs`.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tracevault_protocol::streaming::{StreamEventRequest, StreamEventType};

const REPO_ID: &str = "11111111-1111-1111-1111-111111111111";
const SESSION_ID: &str = "queue-session";

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

/// A server that accepts the first `accepting` stream POSTs and answers every
/// later one with a 503. Only accepted requests are captured: they are what
/// the server stored.
fn spawn_server(accepting: usize) -> (String, mpsc::Receiver<StreamEventRequest>) {
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
        let mut accepted = 0usize;
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
            if !(request_line.starts_with("POST /api/v1/") && request_line.contains("/stream")) {
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                continue;
            }
            if accepted >= accepting {
                let _ = stream.write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                continue;
            }
            accepted += 1;
            if let Ok(req) = serde_json::from_slice(&body) {
                let _ = tx.send(req);
            }
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{addr}"), rx)
}

const ALWAYS: usize = usize::MAX;
const NEVER: usize = 0;

/// A temp git repo bound to `REPO_ID`, with isolated home/config/state dirs.
struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    repo: PathBuf,
    transcript: PathBuf,
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
        std::fs::create_dir_all(repo.join(".visdom")).unwrap();
        std::fs::write(
            repo.join(".visdom").join("tracing.toml"),
            format!("repo_id = \"{REPO_ID}\"\n"),
        )
        .unwrap();
        let transcript = tmp.path().join(format!("{SESSION_ID}.jsonl"));
        Fixture {
            _tmp: tmp,
            home,
            state,
            repo,
            transcript,
        }
    }

    fn session_dir(&self) -> PathBuf {
        self.repo.join(".visdom").join("sessions").join(SESSION_ID)
    }

    fn queue(&self) -> PathBuf {
        self.session_dir().join(format!("pending-{REPO_ID}.jsonl"))
    }

    /// Append `count` transcript lines of about `bytes` each, numbered on
    /// from the lines already there.
    fn lines(&self, count: usize, bytes: usize) {
        let from = std::fs::read_to_string(&self.transcript)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.transcript)
            .unwrap();
        for n in from..from + count {
            let line = serde_json::json!({"type": "assistant", "n": n, "pad": "x".repeat(bytes)});
            writeln!(f, "{line}").unwrap();
        }
    }

    fn transcript_len(&self) -> u64 {
        std::fs::metadata(&self.transcript).unwrap().len()
    }

    fn offset(&self) -> u64 {
        std::fs::read_to_string(self.session_dir().join(".stream_offset"))
            .map(|s| s.trim().parse().unwrap())
            .unwrap_or(0)
    }

    /// Run a `PostToolUse` hook of the session.
    fn hook(&self, base: &str) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tracevault"))
            .current_dir(&self.repo)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_STATE_HOME", &self.state)
            .env("TRACEVAULT_SERVER_URL", base)
            .env("TRACEVAULT_API_KEY", "tvk_test")
            .env_remove("CLAUDE_ENV_FILE")
            .env_remove("TRACEVAULT_SESSION_ID")
            .env_remove("VISDOM_SESSION_ID")
            .env_remove("VISDOM_GATEWAY_SESSION")
            .env_remove("AMP_CURRENT_THREAD_ID")
            .env_remove("TRACEVAULT_PROJECT")
            .env_remove("TRACEVAULT_PROJECT_ATTRIBUTION")
            .args(["stream", "--event", "post-tool-use"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to run the tracevault binary");
        let payload = serde_json::json!({
            "session_id": SESSION_ID,
            "transcript_path": self.transcript.to_string_lossy(),
            "cwd": self.repo.to_string_lossy(),
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_use_id": "toolu_1",
            "tool_input": {"command": "ls"},
        });
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
    }
}

fn queued(path: &Path) -> Vec<StreamEventRequest> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// The `n` of every transcript line the server stored, with the chunk_index
/// the server derives for it (`transcript_offset` + position in the request).
fn stored(reqs: &[StreamEventRequest]) -> Vec<(i64, u64)> {
    reqs.iter()
        .flat_map(|r| {
            let offset = r.transcript_offset.unwrap_or(0);
            r.transcript_lines
                .clone()
                .unwrap_or_default()
                .into_iter()
                .enumerate()
                .map(move |(i, l)| (offset + i as i64, l["n"].as_u64().unwrap()))
        })
        .collect()
}

/// Every line `0..count` exactly once, in order, under rising chunk_indexes.
fn assert_each_line_once(reqs: &[StreamEventRequest], count: u64) {
    let stored = stored(reqs);
    let ns: Vec<u64> = stored.iter().map(|(_, n)| *n).collect();
    assert_eq!(ns, (0..count).collect::<Vec<_>>());
    assert!(stored.windows(2).all(|w| w[0].0 < w[1].0), "{stored:?}");
}

fn tool_events(reqs: &[StreamEventRequest]) -> usize {
    reqs.iter()
        .filter(|r| r.event_type == StreamEventType::ToolUse)
        .count()
}

/// While the server is away every transcript line is queued once: later
/// hooks add their own small event and nothing else, and nothing is lost or
/// sent twice once the server is back.
#[test]
fn a_server_that_is_away_does_not_grow_the_queue_with_the_transcript() {
    let fx = Fixture::new();
    // 1.6 MB: several requests' worth.
    fx.lines(40, 40_000);
    let (down, _) = spawn_server(NEVER);

    fx.hook(&down);
    let after_one = std::fs::metadata(fx.queue()).unwrap().len();
    assert!(after_one < 2 * fx.transcript_len());
    assert_eq!(
        fx.offset(),
        fx.transcript_len(),
        "the lines are in the queue, not to be read again"
    );
    let first = queued(&fx.queue());
    assert_eq!(stored(&first).len(), 40);

    for hooks in 2..=5 {
        fx.hook(&down);
        let queue = queued(&fx.queue());
        assert_eq!(queue.len(), first.len() + hooks - 1, "one event per hook");
        assert_eq!(stored(&queue).len(), 40, "no line a second time");
    }
    assert!(
        std::fs::metadata(fx.queue()).unwrap().len() < after_one + 10_000,
        "four more hooks added four small events"
    );
    assert_eq!(
        std::fs::read_dir(fx.session_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("draining"))
            .count(),
        0,
        "no claimed queue is left behind"
    );

    let (up, rx) = spawn_server(ALWAYS);
    fx.hook(&up);
    let reqs: Vec<StreamEventRequest> = rx.try_iter().collect();
    assert_eq!(tool_events(&reqs), 6, "the five queued events and this one");
    assert_each_line_once(&reqs, 40);
    assert_eq!(fx.offset(), fx.transcript_len());
    assert!(!fx.queue().exists());
}

/// A batch the server takes only part of: the rest is queued once and sent
/// by the next hook. No line twice, none missing.
#[test]
fn a_partly_accepted_batch_is_finished_from_the_queue() {
    let fx = Fixture::new();
    fx.lines(40, 40_000);
    let (flaky, rx) = spawn_server(2);

    fx.hook(&flaky);
    let first: Vec<StreamEventRequest> = rx.try_iter().collect();
    assert_eq!(first.len(), 2);
    let stored_first = stored(&first).len();
    assert!(stored_first > 0 && stored_first < 40);
    assert_eq!(
        stored(&queued(&fx.queue())).len(),
        40 - stored_first,
        "exactly the lines the server did not take"
    );
    assert_eq!(fx.offset(), fx.transcript_len());

    fx.lines(1, 100);
    let (up, rx) = spawn_server(ALWAYS);
    fx.hook(&up);
    let mut reqs = first;
    reqs.extend(rx.try_iter());
    assert_each_line_once(&reqs, 41);
    assert_eq!(tool_events(&reqs), 2);
    assert_eq!(fx.offset(), fx.transcript_len());
    assert!(!fx.queue().exists());
}

/// A queue cannot grow past its bound however long the server is away: the
/// lines that do not fit stay in the transcript, and the offset waits before
/// them until the queue has drained.
#[test]
fn a_full_queue_leaves_the_rest_in_the_transcript() {
    let fx = Fixture::new();
    // 40 MB of transcript against a 32 MiB queue, read 4 MiB at a time.
    fx.lines(400, 100_000);
    let (down, _) = spawn_server(NEVER);
    let cap = 32 * 1024 * 1024;

    for _ in 0..12 {
        fx.hook(&down);
        assert!(std::fs::metadata(fx.queue()).unwrap().len() <= cap);
    }
    let offset = fx.offset();
    assert!(offset > 0 && offset < fx.transcript_len(), "{offset}");
    let in_queue = stored(&queued(&fx.queue()));
    assert_eq!(
        in_queue.iter().map(|(_, n)| *n).collect::<Vec<_>>(),
        (0..in_queue.len() as u64).collect::<Vec<_>>(),
        "the first lines, each once"
    );

    let (up, rx) = spawn_server(ALWAYS);
    for _ in 0..40 {
        if fx.offset() == fx.transcript_len() && !fx.queue().exists() {
            break;
        }
        fx.hook(&up);
    }
    let reqs: Vec<StreamEventRequest> = rx.try_iter().collect();
    assert_each_line_once(&reqs, 400);
    assert_eq!(fx.offset(), fx.transcript_len());
    assert!(!fx.queue().exists());
}

/// A transcript larger than one hook reads (a resumed session, a long
/// outage) is caught up on over the next hooks.
#[test]
fn a_backlog_larger_than_one_hook_reads_is_sent_over_several() {
    let fx = Fixture::new();
    // 9 MB, against a 4 MiB read per hook.
    fx.lines(90, 100_000);
    let (up, rx) = spawn_server(ALWAYS);

    fx.hook(&up);
    let after_one = fx.offset();
    assert!(
        (4 * 1024 * 1024..5 * 1024 * 1024).contains(&after_one),
        "one hook reads about 4 MiB, not the file: {after_one}"
    );
    fx.hook(&up);
    fx.hook(&up);

    let reqs: Vec<StreamEventRequest> = rx.try_iter().collect();
    assert_each_line_once(&reqs, 90);
    assert_eq!(fx.offset(), fx.transcript_len());
    assert!(!fx.queue().exists());
}

/// A queue an earlier release left, repeating a transcript whose offset
/// never moved, shrinks to its events the first time a hook touches it,
/// server or no server; the lines are read from the transcript.
#[test]
fn a_queue_that_repeats_the_transcript_is_compacted_by_the_next_hook() {
    let fx = Fixture::new();
    fx.lines(4, 1_000);
    std::fs::create_dir_all(fx.session_dir()).unwrap();
    let record = |event_type: &str, lines: usize| {
        serde_json::json!({
            "protocol_version": 1,
            "tool": "claude-code",
            "event_type": event_type,
            "session_id": SESSION_ID,
            "timestamp": "2026-11-23T00:00:00Z",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_use_id": "toolu_old",
            "tool_input": {"command": "ls"},
            "event_uuid": uuid::Uuid::now_v7(),
            "transcript_offset": 0,
            "transcript_lines": (0..lines)
                .map(|n| serde_json::json!({"n": n, "pad": "x".repeat(10_000)}))
                .collect::<Vec<_>>(),
        })
        .to_string()
    };
    let mut old = String::new();
    for _ in 0..100 {
        old.push_str(&record("transcript", 40));
        old.push('\n');
    }
    for _ in 0..2 {
        old.push_str(&record("tool_use", 40));
        old.push('\n');
    }
    std::fs::write(fx.queue(), &old).unwrap();
    assert!(old.len() > 40_000_000);

    let (down, _) = spawn_server(NEVER);
    fx.hook(&down);

    let queue = queued(&fx.queue());
    assert_eq!(queue.len(), 3, "the two old events and this hook's");
    assert!(queue[..2].iter().all(|r| r.transcript_lines.is_none()));
    assert_eq!(stored(&queue[2..]).len(), 4, "the transcript's lines, once");
    assert!(std::fs::metadata(fx.queue()).unwrap().len() < 20_000);

    let (up, rx) = spawn_server(ALWAYS);
    fx.hook(&up);
    let reqs: Vec<StreamEventRequest> = rx.try_iter().collect();
    assert_eq!(tool_events(&reqs), 4);
    assert_each_line_once(&reqs, 4);
    assert!(!fx.queue().exists());
}
