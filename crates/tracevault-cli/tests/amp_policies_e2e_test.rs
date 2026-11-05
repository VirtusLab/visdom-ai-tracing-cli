//! VIS-1199 end to end, with the real CLI: what the Amp plugin runs for a
//! thread's turns, and `tracevault repo switch` run from Amp's shell.
//!
//! - The first turn's `session-start` (an Amp-shaped hook event, the thread
//!   id as the session id) binds the session to its checkout's repo and
//!   injects that repo's policies.
//! - A later turn's `user-prompt` injects nothing while the repo stays.
//! - `repo switch` with only `$AMP_CURRENT_THREAD_ID` (no
//!   `TRACEVAULT_SESSION_ID`, no `--session-id`) rebinds that same session.
//! - The next turn's `user-prompt` injects the new repo's policies, once.
//!
//! The server is a raw-TCP mock (the harness of `auto_bind_e2e_test.rs`),
//! answering staged responses in order; everything the CLI reads is isolated
//! to a tempdir.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const THREAD: &str = "T-00000000-0000-7000-8000-0000000000aa";
const REMOTE_A: &str = "44000761-8d22-4256-bd2c-27a0ba278c6f";
const REPO_A: &str = "11111111-1111-4111-8111-111111111111";
const REMOTE_B: &str = "55000761-8d22-4256-bd2c-27a0ba278c6f";
const REPO_B: &str = "22222222-2222-4222-8222-222222222222";

fn accept_with_deadline(listener: &TcpListener) -> Option<std::net::TcpStream> {
    let deadline = Instant::now() + Duration::from_secs(5);
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
/// Tests stage one response more than they expect, so a stray request shows
/// in the captured list instead of hanging the command.
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

fn resolved(remote_id: &str, name: &str) -> String {
    http(
        "200 OK",
        &format!(
            r#"{{"remote_id":"{remote_id}","name":"{name}","normalized_url":"github.com/org/{name}","clone_status":"ready"}}"#
        ),
    )
}

fn detail(repo_id: &str, name: &str) -> String {
    http(
        "200 OK",
        &format!(
            r#"{{"name":"{name}","normalized_url":"github.com/org/{name}","clone_status":"ready","repos":[{{"id":"{repo_id}","name":"{name}"}}]}}"#
        ),
    )
}

fn policies(text: &str) -> String {
    http(
        "200 OK",
        &serde_json::json!({"format": "markdown", "content": text}).to_string(),
    )
}

fn stray() -> String {
    http(
        "500 Internal Server Error",
        r#"{"error":"unexpected request"}"#,
    )
}

struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    repo_a: PathBuf,
    repo_b: PathBuf,
}

fn git_checkout(dir: &Path, origin: &str) {
    std::fs::create_dir_all(dir).unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
        vec!["commit", "-q", "--allow-empty", "-m", "init"],
        vec!["remote", "add", "origin", origin],
    ] {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(&args)
            .status()
            .expect("git failed to run")
            .success();
        assert!(ok, "git {args:?} must succeed");
    }
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        std::fs::create_dir_all(&home).unwrap();
        let repo_a = tmp.path().join("a");
        let repo_b = tmp.path().join("b");
        git_checkout(&repo_a, "git@github.com:org/a.git");
        git_checkout(&repo_b, "git@github.com:org/b.git");
        Fixture {
            _tmp: tmp,
            home,
            state,
            repo_a,
            repo_b,
        }
    }

    /// `tracevault <args>` in `cwd` with `stdin`, as the Amp plugin or Amp's
    /// shell would run it: returns (exit code, stdout, stderr).
    fn run(
        &self,
        args: &[&str],
        cwd: &Path,
        stdin: &str,
        amp_thread: Option<&str>,
        base: &str,
    ) -> (i32, String, String) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tracevault"));
        cmd.args(args)
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
            .env_remove("CLAUDE_ENV_FILE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(thread) = amp_thread {
            cmd.env("AMP_CURRENT_THREAD_ID", thread);
        }
        let mut child = cmd.spawn().expect("failed to run the tracevault binary");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// What the Amp plugin runs on `agent.start`, with the hook event it
    /// writes (`assets/amp-plugin/index.ts`, `policyContext`); returns the
    /// injected context, if any.
    fn turn(&self, command: &str, prompt: Option<&str>, base: &str) -> Option<String> {
        let hook = match command {
            "session-start" => "SessionStart",
            _ => "UserPromptSubmit",
        };
        let mut event = serde_json::json!({
            "session_id": THREAD,
            "transcript_path": "",
            "cwd": self.repo_a.to_string_lossy(),
            "hook_event_name": hook,
        });
        if let Some(prompt) = prompt {
            event["prompt"] = prompt.into();
        }
        let (code, stdout, stderr) =
            self.run(&[command], &self.repo_a, &event.to_string(), None, base);
        assert_eq!(code, 0, "stderr: {stderr}");
        let first_line = stdout.lines().next().unwrap_or_default();
        let answer: serde_json::Value = serde_json::from_str(first_line)
            .unwrap_or_else(|e| panic!("{command} answered {stdout:?}: {e}"));
        answer["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .map(str::to_string)
    }

    fn session_state(&self) -> String {
        std::fs::read_to_string(
            self.state
                .join("tracevault/sessions")
                .join(format!("{THREAD}.toml")),
        )
        .unwrap_or_default()
    }
}

#[test]
fn amp_thread_gets_policies_once_per_repo_and_switches_from_its_shell() {
    let fx = Fixture::new();

    // Turn 1: session-start binds the thread to checkout a's repo and
    // injects its policies.
    let (base, rx) = spawn_server(vec![
        resolved(REMOTE_A, "a"),
        detail(REPO_A, "a"),
        policies("POLICY A"),
        stray(),
    ]);
    let context = fx.turn("session-start", None, &base);
    let requests: Vec<String> = rx.try_iter().collect();
    assert_eq!(context.as_deref(), Some("POLICY A"), "{requests:?}");
    assert!(
        fx.session_state().contains(REPO_A),
        "{}",
        fx.session_state()
    );

    // Turn 2: same repo, nothing to inject, no request.
    let (base, rx) = spawn_server(vec![stray()]);
    assert_eq!(fx.turn("user-prompt", Some("next"), &base), None);
    assert_eq!(rx.try_iter().count(), 0);

    // In Amp's shell: `repo switch` to checkout b, with only the thread id.
    let (base, rx) = spawn_server(vec![
        resolved(REMOTE_B, "b"),
        detail(REPO_B, "b"),
        policies("POLICY B"),
        stray(),
    ]);
    let (code, stdout, stderr) = fx.run(
        &["repo", "switch", &fx.repo_b.to_string_lossy()],
        &fx.repo_a,
        "",
        Some(THREAD),
        &base,
    );
    let requests: Vec<String> = rx.try_iter().collect();
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}\n{requests:?}");
    assert!(
        stdout.contains(&format!("bound session {THREAD} to repo {REPO_B}")),
        "{stdout}"
    );
    assert!(
        fx.session_state().contains(REPO_B),
        "{}",
        fx.session_state()
    );

    // Turn 3: the repo changed, so b's policies are injected.
    let (base, rx) = spawn_server(vec![policies("POLICY B"), stray()]);
    assert_eq!(
        fx.turn("user-prompt", Some("again"), &base).as_deref(),
        Some("POLICY B")
    );
    let requests: Vec<String> = rx.try_iter().collect();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(requests[0].contains(REPO_B), "{requests:?}");

    // Turn 4: once only.
    let (base, rx) = spawn_server(vec![stray()]);
    assert_eq!(fx.turn("user-prompt", Some("and again"), &base), None);
    assert_eq!(rx.try_iter().count(), 0);
}
