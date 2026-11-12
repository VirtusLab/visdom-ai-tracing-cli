//! VIS-1257: a session that starts in an umbrella directory (not a git repo,
//! has `.tracevault/`) and later `cd`s into a child git repo must keep ONE
//! session directory for its whole life.
//!
//! Claude Code's hook `cwd` follows the session's shell `cd`, and the stream
//! hook used to resolve the session directory from that `cwd` on every event.
//! The two `.tracevault/sessions/<id>/` directories that resulted each had
//! their own `.stream_offset`, so every switch re-sent the transcript from
//! the other directory's offset.
//!
//! Runs the hook binary (via `CARGO_BIN_EXE_*`) against a raw-TCP mock server,
//! isolated to a tempdir like `session_end_e2e_test.rs`.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tracevault_protocol::streaming::StreamEventRequest;

const REPO_ID: &str = "11111111-1111-1111-1111-111111111111";
const SESSION_ID: &str = "vis1257-session";

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

/// Accept every request until none arrives for `ACCEPT_DEADLINE`. A stream
/// POST is answered as accepted and its body captured; anything else
/// (`session-start`'s policy and binding lookups) gets a 404.
fn spawn_server() -> (String, mpsc::Receiver<StreamEventRequest>) {
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
            if !(request_line.starts_with("POST /api/v1/") && request_line.contains("/stream")) {
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
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

/// `umbrella/` (no git, has `.tracevault/`) containing the git repo
/// `umbrella/child/` (its own `.tracevault/config.toml`), isolated
/// home/config/state dirs, and the session bound to `REPO_ID` the way
/// `tracevault repo switch` binds it — so every event is sent, wherever the
/// session's shell is.
struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    umbrella: PathBuf,
    child: PathBuf,
    project: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // Canonical, so the paths the CLI derives (`git rev-parse`) and the
        // ones the test builds agree (macOS `/var` → `/private/var`).
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        let state = root.join("state");
        let umbrella = root.join("umbrella");
        let child = umbrella.join("child");
        let project = root.join("claude-project");
        for d in [&home, &child, &project] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::create_dir_all(umbrella.join(".tracevault")).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
            vec!["commit", "-q", "--allow-empty", "-m", "init"],
        ] {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&child)
                .args(&args)
                .status()
                .expect("git failed to run")
                .success();
            assert!(ok, "git {args:?} must succeed");
        }
        std::fs::create_dir_all(child.join(".tracevault")).unwrap();
        std::fs::write(
            child.join(".tracevault").join("config.toml"),
            format!("repo_id = \"{REPO_ID}\"\n"),
        )
        .unwrap();
        let sessions = state.join("tracevault").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join(format!("{SESSION_ID}.toml")),
            format!("[active]\nrepo_id = \"{REPO_ID}\"\nupdated_at = \"t\"\n"),
        )
        .unwrap();
        Fixture {
            _tmp: tmp,
            home,
            state,
            umbrella,
            child,
            project,
        }
    }

    fn transcript(&self) -> PathBuf {
        self.project.join(format!("{SESSION_ID}.jsonl"))
    }

    fn session_dir(&self, root: &Path) -> PathBuf {
        root.join(".tracevault").join("sessions").join(SESSION_ID)
    }

    fn assistant(&self, n: usize) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.transcript())
            .unwrap();
        writeln!(f, "{}", serde_json::json!({"type": "assistant", "n": n})).unwrap();
    }

    fn run(&self, base: &str, cwd: &Path, args: &[&str], payload: serde_json::Value) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tracevault"))
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_STATE_HOME", &self.state)
            .env("TRACEVAULT_SERVER_URL", base)
            .env("TRACEVAULT_API_KEY", "tvk_test")
            .env_remove("CLAUDE_ENV_FILE")
            .env_remove("TRACEVAULT_SESSION_ID")
            .env_remove("AMP_CURRENT_THREAD_ID")
            .env_remove("TRACEVAULT_PROJECT")
            .env_remove("TRACEVAULT_PROJECT_ATTRIBUTION")
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

    /// `tracevault session-start` with the hook's `cwd` = `cwd`.
    fn session_start(&self, base: &str, cwd: &Path, source: &str) {
        self.run(
            base,
            cwd,
            &["session-start"],
            serde_json::json!({
                "session_id": SESSION_ID,
                "transcript_path": self.transcript().to_string_lossy(),
                "cwd": cwd.to_string_lossy(),
                "hook_event_name": "SessionStart",
                "source": source,
            }),
        );
    }

    /// A `PostToolUse` stream hook with the hook's `cwd` = `cwd`. Like Claude
    /// Code, the process itself runs in the session's launch directory; only
    /// the payload's `cwd` follows the shell.
    fn tool_use(&self, base: &str, cwd: &Path) {
        self.run(
            base,
            &self.umbrella,
            &["stream", "--event", "post-tool-use"],
            serde_json::json!({
                "session_id": SESSION_ID,
                "transcript_path": self.transcript().to_string_lossy(),
                "cwd": cwd.to_string_lossy(),
                "hook_event_name": "PostToolUse",
                "tool_name": "Bash",
            }),
        );
    }
}

/// The `n` of every transcript line the server received, in order.
fn sent_lines(rx: &mpsc::Receiver<StreamEventRequest>) -> Vec<u64> {
    rx.try_iter()
        .flat_map(|r| r.transcript_lines.unwrap_or_default())
        .filter_map(|l| l["n"].as_u64())
        .collect()
}

/// Umbrella → child repo → umbrella, one new transcript line each time.
fn walk(fx: &Fixture, base: &str) {
    fx.assistant(1);
    fx.tool_use(base, &fx.umbrella);
    fx.assistant(2);
    fx.tool_use(base, &fx.child);
    fx.assistant(3);
    fx.tool_use(base, &fx.umbrella);
}

#[test]
fn a_session_that_cds_into_a_child_repo_keeps_one_session_dir() {
    let fx = Fixture::new();
    let (base, rx) = spawn_server();
    fx.session_start(&base, &fx.umbrella, "startup");

    walk(&fx, &base);

    assert_eq!(
        sent_lines(&rx),
        vec![1, 2, 3],
        "every transcript line is sent exactly once, in order"
    );
    assert!(fx
        .session_dir(&fx.umbrella)
        .join(".stream_offset")
        .is_file());
    assert!(
        !fx.session_dir(&fx.child).exists(),
        "the child repo must not get a second session dir"
    );
    // The origin marker still follows the event's worktree, so
    // `check`/`verify-start` in the child repo can tell the session is there.
    assert_eq!(
        std::fs::read_to_string(fx.session_dir(&fx.umbrella).join("origin")).unwrap(),
        fx.umbrella.to_string_lossy()
    );
}

#[test]
fn a_resume_from_the_child_repo_does_not_move_the_session_dir() {
    let fx = Fixture::new();
    let (base, rx) = spawn_server();
    fx.session_start(&base, &fx.umbrella, "startup");
    fx.assistant(1);
    fx.tool_use(&base, &fx.umbrella);

    // `SessionStart` fires again on resume / clear / compact, from wherever
    // the shell is by then.
    fx.session_start(&base, &fx.child, "compact");
    fx.assistant(2);
    fx.tool_use(&base, &fx.child);

    assert_eq!(sent_lines(&rx), vec![1, 2]);
    assert!(!fx.session_dir(&fx.child).exists());
}

/// A session with no recorded start (begun before this CLI version, or by an
/// agent without a `SessionStart` hook) resolves its directory from each
/// event's `cwd`, as before.
#[test]
fn a_session_without_a_recorded_start_keeps_resolving_from_cwd() {
    let fx = Fixture::new();
    let (base, _rx) = spawn_server();

    walk(&fx, &base);

    assert!(fx
        .session_dir(&fx.umbrella)
        .join(".stream_offset")
        .is_file());
    assert!(fx.session_dir(&fx.child).join(".stream_offset").is_file());
}

/// A session started where no `.tracevault/` resolves (no git repo, none in
/// an ancestor) records nothing: anchoring there would create a stray
/// `.tracevault/` and hide the session from the repo it then works in.
#[test]
fn a_session_started_outside_any_tracevault_dir_is_not_anchored() {
    let fx = Fixture::new();
    let (base, rx) = spawn_server();
    let outside = fx.home.join("scratch");
    std::fs::create_dir_all(&outside).unwrap();
    fx.session_start(&base, &outside, "startup");

    fx.assistant(1);
    fx.tool_use(&base, &fx.child);

    assert_eq!(sent_lines(&rx), vec![1]);
    assert!(fx.session_dir(&fx.child).join(".stream_offset").is_file());
    assert!(!outside.join(".tracevault").exists());
}
