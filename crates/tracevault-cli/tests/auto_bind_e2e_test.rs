//! VIS-1048 end to end: with only `init --global` + `login` (no
//! `.tracevault/config.toml`, no `repo switch`, no user default), a session
//! started in a registered repo's checkout is bound to that repo by its hooks
//! and its events are sent under it; an unregistered remote stays unbound and
//! is reported once.
//!
//! Runs the real `tracevault session-start` / `tracevault stream` hook
//! binaries against a raw-TCP mock server, in the harness style of
//! `stream_refused_project_test.rs`. Everything the hooks read is isolated to
//! a tempdir: session state (`XDG_STATE_HOME`), user config and defaults
//! (`XDG_CONFIG_HOME`, `HOME`), and the server (`TRACEVAULT_*`).
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const REMOTE_ID: &str = "44000761-8d22-4256-bd2c-27a0ba278c6f";
const REPO_ID: &str = "11111111-1111-4111-8111-111111111111";
const SESSION_ID: &str = "vis1048-auto-bind-session";

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
/// Stage one response more than expected so a stray request is captured and
/// fails the count assertion instead of hanging the hook.
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

fn resolved() -> String {
    http(
        "200 OK",
        &format!(
            r#"{{"remote_id":"{REMOTE_ID}","name":"repo","normalized_url":"github.com/org/repo","clone_status":"ready"}}"#
        ),
    )
}

fn detail() -> String {
    http(
        "200 OK",
        &format!(
            r#"{{"name":"repo","normalized_url":"github.com/org/repo","clone_status":"ready","repos":[{{"id":"{REPO_ID}","name":"repo"}}]}}"#
        ),
    )
}

fn policies() -> String {
    http("200 OK", r#"{"format":"markdown","content":"POLICY TEXT"}"#)
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

fn not_registered() -> String {
    http("404 Not Found", r#"{"error":"No remote for that URL"}"#)
}

/// A checkout with an origin remote and no `.tracevault/` (global install),
/// plus isolated home/state dirs.
struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    repo: PathBuf,
}

impl Fixture {
    fn new(origin: &str) -> Self {
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
            vec!["remote", "add", "origin", origin],
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
        Fixture {
            _tmp: tmp,
            home,
            state,
            repo,
        }
    }

    /// Run `tracevault <args>` as a hook for `hook_event_name`, with the
    /// server and key in `TRACEVAULT_*`: returns (exit code, stdout, stderr).
    fn run(&self, args: &[&str], hook_event_name: &str, base: &str) -> (i32, String, String) {
        self.run_with(args, hook_event_name, base, true)
    }

    /// [`Self::run`]; with `env_credentials` false the server and key come
    /// from the credentials file `tracevault login` writes instead.
    fn run_with(
        &self,
        args: &[&str],
        hook_event_name: &str,
        base: &str,
        env_credentials: bool,
    ) -> (i32, String, String) {
        let event = serde_json::json!({
            "session_id": SESSION_ID,
            "transcript_path": "",
            "cwd": self.repo.to_string_lossy(),
            "hook_event_name": hook_event_name,
            "tool_name": "Bash",
        });
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tracevault"));
        if env_credentials {
            cmd.env("TRACEVAULT_SERVER_URL", base)
                .env("TRACEVAULT_API_KEY", "tvk_test");
        } else {
            let dir = self.home.join(".config").join("tracevault");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("credentials.json"),
                serde_json::json!({ "server_url": base, "token": "tvk_test" }).to_string(),
            )
            .unwrap();
            cmd.env_remove("TRACEVAULT_SERVER_URL")
                .env_remove("TRACEVAULT_API_KEY");
        }
        let mut child = cmd
            .args(args)
            .current_dir(&self.repo)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_STATE_HOME", &self.state)
            .env_remove("TRACEVAULT_SESSION_ID")
            .env_remove("TRACEVAULT_PROJECT")
            .env_remove("TRACEVAULT_PROJECT_ATTRIBUTION")
            .env_remove("CLAUDE_ENV_FILE")
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

    fn session_start(&self, base: &str) -> (i32, String, String) {
        self.run(&["session-start"], "SessionStart", base)
    }

    fn stream(&self, base: &str) -> (i32, String, String) {
        self.run(&["stream", "--event", "post-tool-use"], "PostToolUse", base)
    }

    fn session_state(&self) -> String {
        std::fs::read_to_string(
            self.state
                .join("tracevault")
                .join("sessions")
                .join(format!("{SESSION_ID}.toml")),
        )
        .unwrap_or_default()
    }
}

#[test]
fn session_start_binds_a_registered_checkout_and_events_follow() {
    let fx = Fixture::new("git@github.com:org/repo.git");

    let (base, rx) = spawn_server(vec![resolved(), detail(), policies(), accepted()]);
    let (code, stdout, stderr) = fx.session_start(&base);
    assert_eq!(code, 0, "stderr: {stderr}");
    let requests: Vec<String> = rx.try_iter().collect();
    assert_eq!(
        requests.len(),
        3,
        "resolve + detail + policies: {requests:?}"
    );
    assert!(
        requests[0].starts_with("GET /api/v1/remotes/resolve?"),
        "{requests:?}"
    );
    assert!(requests[2].contains(REPO_ID), "{requests:?}");
    // Bound → the repo's policies are injected, as after a `repo switch`.
    assert!(stdout.contains("POLICY TEXT"), "stdout: {stdout}");
    assert!(!stdout.contains("systemMessage"), "stdout: {stdout}");
    assert!(
        fx.session_state().contains(REPO_ID),
        "{}",
        fx.session_state()
    );

    // The next event goes to that repo with no further lookup.
    let (base, rx) = spawn_server(vec![accepted(), accepted()]);
    let (code, _, stderr) = fx.stream(&base);
    assert_eq!(code, 0, "stderr: {stderr}");
    let requests: Vec<String> = rx.try_iter().collect();
    assert_eq!(
        requests,
        vec![format!("POST /api/v1/repos/{REPO_ID}/stream HTTP/1.1")]
    );
}

/// The acceptance path as a user has it: `login` wrote a credentials file,
/// nothing is in the environment.
#[test]
fn a_login_credentials_file_is_enough_to_bind() {
    let fx = Fixture::new("git@github.com:org/repo.git");

    let (base, rx) = spawn_server(vec![resolved(), detail(), policies(), accepted()]);
    let (code, stdout, stderr) = fx.run_with(&["session-start"], "SessionStart", &base, false);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(rx.try_iter().count(), 3);
    assert!(stdout.contains("POLICY TEXT"), "stdout: {stdout}");
    assert!(fx.session_state().contains(REPO_ID));
}

#[test]
fn the_stream_hook_binds_a_session_that_missed_session_start() {
    let fx = Fixture::new("https://github.com/org/repo.git");

    let (base, rx) = spawn_server(vec![resolved(), detail(), accepted(), accepted()]);
    let (code, _, stderr) = fx.stream(&base);
    assert_eq!(code, 0, "stderr: {stderr}");
    let requests: Vec<String> = rx.try_iter().collect();
    assert_eq!(requests.len(), 3, "{requests:?}");
    assert_eq!(
        requests[2],
        format!("POST /api/v1/repos/{REPO_ID}/stream HTTP/1.1")
    );
}

#[test]
fn an_unregistered_remote_stays_unbound_and_says_so_once() {
    let fx = Fixture::new("git@github.com:org/unregistered.git");

    let (base, rx) = spawn_server(vec![not_registered(), accepted()]);
    let (code, stdout, stderr) = fx.session_start(&base);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(rx.try_iter().count(), 1, "one lookup");
    let out: serde_json::Value = serde_json::from_str(stdout.trim()).expect("hook JSON");
    let message = out["systemMessage"].as_str().expect("systemMessage");
    assert!(message.contains("not registered"), "{message}");
    assert!(out.get("hookSpecificOutput").is_none(), "{stdout}");

    // Later hooks neither look it up again nor repeat the message, and the
    // event is not sent anywhere.
    let (base, rx) = spawn_server(vec![accepted()]);
    let (code, stdout, stderr) = fx.stream(&base);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(rx.try_iter().count(), 0);
    assert!(!stderr.contains("not registered"), "stderr: {stderr}");
    let (_, stdout2, _) = fx.session_start(&base);
    assert!(!stdout.contains("systemMessage") && !stdout2.contains("systemMessage"));
    assert_eq!(rx.try_iter().count(), 0);
}

#[test]
fn an_explicit_repo_switch_binding_is_never_overridden() {
    let fx = Fixture::new("git@github.com:org/repo.git");
    let switched = "22222222-2222-4222-8222-222222222222";
    let dir = fx.state.join("tracevault").join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{SESSION_ID}.toml")),
        format!("[active]\nrepo_id = \"{switched}\"\nupdated_at = \"\"\n"),
    )
    .unwrap();

    let (base, rx) = spawn_server(vec![accepted(), accepted()]);
    let (code, _, stderr) = fx.stream(&base);
    assert_eq!(code, 0, "stderr: {stderr}");
    let requests: Vec<String> = rx.try_iter().collect();
    assert_eq!(
        requests,
        vec![format!("POST /api/v1/repos/{switched}/stream HTTP/1.1")]
    );
}
