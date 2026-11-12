//! VIS-1256 end to end: an umbrella directory (not a git repo) whose
//! `.tracevault/config.toml` carries `project_id` records every session
//! started under it to that project — from the directory itself and from a
//! non-git sub-directory — with no per-session step and no extra network call
//! (only the stream send itself reaches the server). A session's own
//! `project switch` still wins; a legacy `default_project` NAME is ignored
//! by ingest; a directory outside the workspace is unaffected.
//!
//! Runs the real `tracevault stream` hook binary against a raw-TCP mock
//! server (the harness of `stream_project_only_test.rs`), with everything the
//! hook reads isolated to a tempdir.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const SESSION_ID: &str = "vis1256-workspace-session";
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

/// Serve `responses` to successive connections, capturing each request
/// line. Stage one response MORE than expected so a stray request is
/// captured and fails the count assertion.
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

fn accepted() -> String {
    let body = serde_json::to_string(&tracevault_protocol::streaming::StreamEventResponse {
        session_db_id: uuid::Uuid::nil(),
        event_db_id: Some(uuid::Uuid::nil()),
        status: "accepted".to_string(),
    })
    .unwrap();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    state: PathBuf,
    umbrella: PathBuf,
    transcript: PathBuf,
}

impl Fixture {
    /// An umbrella dir whose `.tracevault/config.toml` holds `config`.
    fn new(config: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let home = root.join("home");
        let state = root.join("state");
        let umbrella = root.join("umbrella");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(umbrella.join(".tracevault")).unwrap();
        std::fs::write(umbrella.join(".tracevault/config.toml"), config).unwrap();
        let transcript = root.join("transcript.jsonl");
        std::fs::write(&transcript, "{\"n\":1}\n").unwrap();
        Fixture {
            _tmp: tmp,
            root,
            home,
            state,
            umbrella,
            transcript,
        }
    }

    /// `project switch` for this session: the session-level binding.
    fn switch_session_project(&self, pid: uuid::Uuid) {
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

    fn run_hook(&self, cwd: &Path, base: &str) -> String {
        let event = serde_json::json!({
            "session_id": SESSION_ID,
            "transcript_path": self.transcript.to_string_lossy(),
            "cwd": cwd.to_string_lossy(),
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_tracevault"))
            .args([
                "stream",
                "--event",
                "post-tool-use",
                "--agent",
                "claude-code",
            ])
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_STATE_HOME", &self.state)
            .env("TRACEVAULT_SERVER_URL", base)
            .env("TRACEVAULT_API_KEY", "tvk_test")
            .env_remove("TRACEVAULT_SESSION_ID")
            .env_remove("AMP_CURRENT_THREAD_ID")
            .env_remove("TRACEVAULT_PROJECT")
            .env_remove("TRACEVAULT_PROJECT_ATTRIBUTION")
            .env("GIT_CEILING_DIRECTORIES", &self.root)
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
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(
            out.status.code(),
            Some(0),
            "the hook never fails; stderr:\n{stderr}"
        );
        stderr
    }
}

const WORKSPACE_PID: uuid::Uuid = uuid::Uuid::from_u128(0x1256);

fn workspace_config() -> String {
    format!("project_id = \"{WORKSPACE_PID}\"\nproject_name = \"visdom\"\n")
}

/// From the umbrella directory itself and from a non-git sub-directory, the
/// event goes to the workspace project's bare endpoint (no repo), and that
/// send is the ONLY request: resolving the tier costs no network call.
#[test]
fn workspace_project_id_records_sessions_from_the_dir_and_below() {
    let fx = Fixture::new(&workspace_config());
    let sub = fx.umbrella.join("notes");
    std::fs::create_dir_all(&sub).unwrap();
    let bare = format!("POST /api/v1/projects/{WORKSPACE_PID}/stream ");

    for cwd in [fx.umbrella.clone(), sub] {
        // A fresh session dir per cwd would hide nothing: both resolve to the
        // umbrella root. Reset the transcript offset by using a new line.
        std::fs::write(&fx.transcript, "{\"n\":1}\n").unwrap();
        let _ = std::fs::remove_dir_all(fx.umbrella.join(".tracevault/sessions"));
        let (base, rx) = spawn_server(vec![accepted(), accepted()]);
        let stderr = fx.run_hook(&cwd, &base);
        let reqs: Vec<String> = rx.try_iter().collect();
        assert_eq!(reqs.len(), 1, "cwd {cwd:?}: {reqs:?}\nstderr:\n{stderr}");
        assert!(reqs[0].starts_with(&bare), "cwd {cwd:?}: {reqs:?}");
    }
}

/// A session's own `project switch` outranks the workspace config.
#[test]
fn session_project_switch_outranks_the_workspace_config() {
    let fx = Fixture::new(&workspace_config());
    let session_pid = uuid::Uuid::from_u128(0x5e55);
    fx.switch_session_project(session_pid);
    let (base, rx) = spawn_server(vec![accepted(), accepted()]);
    let stderr = fx.run_hook(&fx.umbrella, &base);
    let reqs: Vec<String> = rx.try_iter().collect();
    assert_eq!(reqs.len(), 1, "{reqs:?}\nstderr:\n{stderr}");
    assert!(
        reqs[0].starts_with(&format!("POST /api/v1/projects/{session_pid}/stream ")),
        "{reqs:?}"
    );
}

/// An unmigrated config (legacy `default_project` NAME only) contributes
/// nothing at capture: no lookup, no send.
#[test]
fn legacy_default_project_is_ignored_by_ingest() {
    let fx = Fixture::new("default_project = \"visdom\"\n");
    let (base, rx) = spawn_server(vec![accepted()]);
    let stderr = fx.run_hook(&fx.umbrella, &base);
    let reqs: Vec<String> = rx.try_iter().collect();
    assert!(reqs.is_empty(), "{reqs:?}\nstderr:\n{stderr}");
}

/// A directory outside the workspace is unaffected by its binding.
#[test]
fn sessions_outside_the_workspace_are_unaffected() {
    let fx = Fixture::new(&workspace_config());
    let outside = fx.root.join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    let (base, rx) = spawn_server(vec![accepted()]);
    let stderr = fx.run_hook(&outside, &base);
    let reqs: Vec<String> = rx.try_iter().collect();
    assert!(reqs.is_empty(), "{reqs:?}\nstderr:\n{stderr}");
}
