//! VIS-1258 end to end: a project-only session in an umbrella directory (not
//! a git repo) sends an event that touches a child checkout's file with that
//! checkout's repo — when the repo is in the bound project — and every other
//! event project-only. The checkout is looked up once per session, not per
//! event.
//!
//! Runs the real `tracevault stream` hook binary against a raw-TCP mock
//! server that serves the lookups (`remotes/resolve`, the remote's repos, the
//! project) and the stream posts in order. Everything the hook reads is
//! isolated to a tempdir (`XDG_STATE_HOME`, `XDG_CONFIG_HOME`, `HOME`,
//! `TRACEVAULT_*`).
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const SESSION_ID: &str = "vis1258-umbrella-session";
const REMOTE_ID: &str = "44000761-8d22-4256-bd2c-27a0ba278c6f";
const REPO_ID: &str = "22222222-2222-2222-2222-222222222222";
const OTHER_REPO_ID: &str = "33333333-3333-3333-3333-333333333333";
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
/// Stage one response MORE than expected: a stray extra request is then
/// answered and captured, and fails the count assertion.
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

fn resolved() -> String {
    http(
        "200 OK",
        &format!(
            r#"{{"remote_id":"{REMOTE_ID}","name":"child","normalized_url":"github.com/org/child","clone_status":"ready"}}"#
        ),
    )
}

fn remote_detail() -> String {
    http(
        "200 OK",
        &format!(
            r#"{{"name":"child","normalized_url":"github.com/org/child","clone_status":"ready","repos":[{{"id":"{REPO_ID}","name":"child"}}]}}"#
        ),
    )
}

fn project(repos: &[&str]) -> String {
    let repos: Vec<String> = repos
        .iter()
        .map(|id| format!(r#"{{"id":"{id}"}}"#))
        .collect();
    http("200 OK", &format!(r#"{{"repos":[{}]}}"#, repos.join(",")))
}

/// An umbrella dir (no git) holding a `.tracevault/` and a child checkout
/// whose origin is `git@github.com:org/child.git`, plus a transcript file
/// and isolated home/config/state dirs.
struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    umbrella: PathBuf,
    child: PathBuf,
    transcript: PathBuf,
    pid: uuid::Uuid,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let umbrella = tmp.path().join("umbrella");
        let child = umbrella.join("child");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(umbrella.join(".tracevault")).unwrap();
        std::fs::create_dir_all(child.join("src")).unwrap();
        git(&child, &["init", "-q"]);
        git(
            &child,
            &["remote", "add", "origin", "git@github.com:org/child.git"],
        );
        let transcript = tmp.path().join("transcript.jsonl");
        std::fs::write(&transcript, "").unwrap();
        let pid = uuid::Uuid::from_u128(0x1258);
        // `project switch` writes the same session state file.
        let sessions = state.join("tracevault").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join(format!("{SESSION_ID}.toml")),
            format!(
                "[active_project]\nproject_id = \"{pid}\"\nproject_name = \"p\"\nupdated_at = \"\"\n"
            ),
        )
        .unwrap();
        Fixture {
            _tmp: tmp,
            home,
            state,
            umbrella,
            child,
            transcript,
            pid,
        }
    }

    fn state_file(&self) -> String {
        std::fs::read_to_string(
            self.state
                .join("tracevault")
                .join("sessions")
                .join(format!("{SESSION_ID}.toml")),
        )
        .unwrap()
    }

    fn append_transcript(&self, n: u32) {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.transcript)
            .unwrap();
        writeln!(f, "{}", serde_json::json!({ "n": n })).unwrap();
    }

    /// One `PostToolUse` from the umbrella dir, for `tool_input`.
    fn run_hook(&self, base: &str, tool_input: serde_json::Value) -> String {
        let event = serde_json::json!({
            "session_id": SESSION_ID,
            "transcript_path": self.transcript.to_string_lossy(),
            "cwd": self.umbrella.to_string_lossy(),
            "hook_event_name": "PostToolUse",
            "tool_name": "Edit",
            "tool_input": tool_input,
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_tracevault"))
            .args([
                "stream",
                "--event",
                "post-tool-use",
                "--agent",
                "claude-code",
            ])
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
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
        stderr
    }
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn edit(path: &Path) -> serde_json::Value {
    serde_json::json!({ "file_path": path.to_string_lossy(), "old_string": "a", "new_string": "b" })
}

/// Events that touch the child checkout carry its repo, looked up once; an
/// event that touches nothing in a checkout stays project-only.
#[test]
fn an_umbrella_session_attributes_child_checkout_events_to_the_child_repo() {
    let fx = Fixture::new();
    let pid = fx.pid;
    let to_repo = format!("POST /api/v1/projects/{pid}/stream?repo_id={REPO_ID} ");
    let bare = format!("POST /api/v1/projects/{pid}/stream ");

    // Event 1: an Edit in the child: lookup (3 requests), then the send with
    // the child's repo.
    fx.append_transcript(1);
    let (base, rx) = spawn_server(vec![
        resolved(),
        remote_detail(),
        project(&[REPO_ID]),
        accepted(),
        accepted(),
    ]);
    let stderr = fx.run_hook(&base, edit(&fx.child.join("src/main.rs")));
    let reqs: Vec<String> = rx.try_iter().collect();
    assert_eq!(reqs.len(), 4, "{reqs:?}\nstderr:\n{stderr}");
    assert!(
        reqs[0].starts_with("GET /api/v1/remotes/resolve?"),
        "{reqs:?}"
    );
    assert!(
        reqs[2].starts_with(&format!("GET /api/v1/projects/{pid} ")),
        "{reqs:?}"
    );
    assert!(reqs[3].starts_with(&to_repo), "{reqs:?}");
    let state = fx.state_file();
    assert!(!state.contains("[active]"), "never bound: {state}");
    // `check` / `verify-start` in the child find the session by this record.
    let worktrees = std::fs::read_to_string(
        fx.umbrella
            .join(".tracevault")
            .join("sessions")
            .join(SESSION_ID)
            .join("worktrees"),
    )
    .unwrap_or_default();
    let child_top = std::fs::canonicalize(&fx.child).unwrap();
    assert!(
        worktrees
            .lines()
            .any(|l| Path::new(l) == child_top.as_path()),
        "worktrees: {worktrees:?}"
    );

    // Event 2: the child again — from the cache, no lookup.
    fx.append_transcript(2);
    let (base, rx) = spawn_server(vec![accepted(), accepted()]);
    fx.run_hook(&base, edit(&fx.child.join("src/lib.rs")));
    let reqs: Vec<String> = rx.try_iter().collect();
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert!(reqs[0].starts_with(&to_repo), "{reqs:?}");

    // Event 3: a file outside any checkout — project-only. The umbrella dir
    // has no git remote, so deciding that asks the server nothing.
    fx.append_transcript(3);
    let (base, rx) = spawn_server(vec![accepted(), accepted()]);
    fx.run_hook(&base, edit(&fx.umbrella.join("notes.md")));
    let reqs: Vec<String> = rx.try_iter().collect();
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert!(reqs[0].starts_with(&bare), "{reqs:?}");
}

/// A checkout whose repo is not in the bound project is never attributed to
/// it: the event goes project-only, and the miss is remembered.
#[test]
fn a_child_repo_outside_the_project_stays_project_only() {
    let fx = Fixture::new();
    let pid = fx.pid;
    let bare = format!("POST /api/v1/projects/{pid}/stream ");

    fx.append_transcript(1);
    let (base, rx) = spawn_server(vec![
        resolved(),
        remote_detail(),
        project(&[OTHER_REPO_ID]),
        accepted(),
        accepted(),
    ]);
    fx.run_hook(&base, edit(&fx.child.join("src/main.rs")));
    let reqs: Vec<String> = rx.try_iter().collect();
    assert_eq!(reqs.len(), 4, "{reqs:?}");
    assert!(reqs[3].starts_with(&bare), "{reqs:?}");

    fx.append_transcript(2);
    let (base, rx) = spawn_server(vec![accepted(), accepted()]);
    fx.run_hook(&base, edit(&fx.child.join("src/main.rs")));
    let reqs: Vec<String> = rx.try_iter().collect();
    assert_eq!(reqs.len(), 1, "the miss is cached: {reqs:?}");
    assert!(reqs[0].starts_with(&bare), "{reqs:?}");
}

fn unavailable() -> String {
    http("503 Service Unavailable", r#"{"error":"down"}"#)
}

/// The queued events of `queue` (a `pending-*.jsonl` in the session dir).
fn queued(fx: &Fixture, queue: &str) -> usize {
    std::fs::read_to_string(
        fx.umbrella
            .join(".tracevault")
            .join("sessions")
            .join(SESSION_ID)
            .join(queue),
    )
    .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
    .unwrap_or(0)
}

/// A project-only event buffered after a failed send keeps its target: the
/// next event, inferred to the child repo, does not take it over; the next
/// project-only event delivers it, project-only.
#[test]
fn a_buffered_project_only_event_is_not_reattributed_to_an_inferred_repo() {
    let fx = Fixture::new();
    let pid = fx.pid;
    let to_repo = format!("POST /api/v1/projects/{pid}/stream?repo_id={REPO_ID} ");
    let bare = format!("POST /api/v1/projects/{pid}/stream ");
    let project_queue = format!("pending-project-{pid}.jsonl");
    let no_file = serde_json::json!({ "command": "ls" });

    // 1: project-only (the umbrella dir is no checkout), server down → queued.
    fx.append_transcript(1);
    let (base, rx) = spawn_server(vec![unavailable(), accepted()]);
    fx.run_hook(&base, no_file.clone());
    let reqs: Vec<String> = rx.try_iter().collect();
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert!(reqs[0].starts_with(&bare), "{reqs:?}");
    assert_eq!(queued(&fx, &project_queue), 1);

    // 2: an Edit in the child — sent with its repo; the queue stays.
    fx.append_transcript(2);
    let (base, rx) = spawn_server(vec![
        resolved(),
        remote_detail(),
        project(&[REPO_ID]),
        accepted(),
        accepted(),
    ]);
    fx.run_hook(&base, edit(&fx.child.join("src/main.rs")));
    let reqs: Vec<String> = rx.try_iter().collect();
    assert_eq!(reqs.len(), 4, "lookup + this event only: {reqs:?}");
    assert!(reqs[3].starts_with(&to_repo), "{reqs:?}");
    assert_eq!(queued(&fx, &project_queue), 1, "not adopted");

    // 3: project-only again — the queued event goes first, still project-only.
    fx.append_transcript(3);
    let (base, rx) = spawn_server(vec![accepted(), accepted(), accepted()]);
    fx.run_hook(&base, no_file);
    let reqs: Vec<String> = rx.try_iter().collect();
    assert_eq!(reqs.len(), 2, "{reqs:?}");
    assert!(reqs.iter().all(|r| r.starts_with(&bare)), "{reqs:?}");
    assert_eq!(queued(&fx, &project_queue), 0);
}

/// An event buffered for one inferred repo stays in that repo's queue when
/// the next event goes to another checkout's repo, and is delivered to its
/// own repo by the next event there.
#[test]
fn a_buffered_event_stays_with_its_inferred_repo() {
    let fx = Fixture::new();
    let pid = fx.pid;
    let other = fx.umbrella.join("other");
    std::fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q"]);
    git(
        &other,
        &["remote", "add", "origin", "git@github.com:org/other.git"],
    );
    let other_detail = http(
        "200 OK",
        &format!(
            r#"{{"name":"other","normalized_url":"github.com/org/other","clone_status":"ready","repos":[{{"id":"{OTHER_REPO_ID}","name":"other"}}]}}"#
        ),
    );
    let to_repo = format!("POST /api/v1/projects/{pid}/stream?repo_id={REPO_ID} ");
    let to_other = format!("POST /api/v1/projects/{pid}/stream?repo_id={OTHER_REPO_ID} ");
    let both = [REPO_ID, OTHER_REPO_ID];

    // 1: child's repo, server down → queued for that repo.
    fx.append_transcript(1);
    let (base, rx) = spawn_server(vec![
        resolved(),
        remote_detail(),
        project(&both),
        unavailable(),
        accepted(),
    ]);
    fx.run_hook(&base, edit(&fx.child.join("a.rs")));
    let reqs: Vec<String> = rx.try_iter().collect();
    assert_eq!(reqs.len(), 4, "{reqs:?}");
    assert_eq!(queued(&fx, &format!("pending-{REPO_ID}.jsonl")), 1);

    // 2: the other checkout's repo — its own send only.
    fx.append_transcript(2);
    let (base, rx) = spawn_server(vec![
        resolved(),
        other_detail,
        project(&both),
        accepted(),
        accepted(),
    ]);
    fx.run_hook(&base, edit(&other.join("b.rs")));
    let reqs: Vec<String> = rx.try_iter().collect();
    assert_eq!(reqs.len(), 4, "{reqs:?}");
    assert!(reqs[3].starts_with(&to_other), "{reqs:?}");
    assert_eq!(queued(&fx, &format!("pending-{REPO_ID}.jsonl")), 1);

    // 3: back in the child (cached) — the queued event goes first, to its repo.
    fx.append_transcript(3);
    let (base, rx) = spawn_server(vec![accepted(), accepted(), accepted()]);
    fx.run_hook(&base, edit(&fx.child.join("a.rs")));
    let reqs: Vec<String> = rx.try_iter().collect();
    assert_eq!(reqs.len(), 2, "{reqs:?}");
    assert!(reqs.iter().all(|r| r.starts_with(&to_repo)), "{reqs:?}");
    assert_eq!(queued(&fx, &format!("pending-{REPO_ID}.jsonl")), 0);
}

/// An explicit repo binding wins: nothing is looked up, nothing inferred.
#[test]
fn an_explicit_repo_binding_is_never_overridden() {
    let fx = Fixture::new();
    let pid = fx.pid;
    let sessions = fx.state.join("tracevault").join("sessions");
    std::fs::write(
        sessions.join(format!("{SESSION_ID}.toml")),
        format!(
            "[active]\nrepo_id = \"{OTHER_REPO_ID}\"\nupdated_at = \"\"\n\n\
             [active_project]\nproject_id = \"{pid}\"\nproject_name = \"p\"\nupdated_at = \"\"\n"
        ),
    )
    .unwrap();

    fx.append_transcript(1);
    let (base, rx) = spawn_server(vec![accepted(), accepted()]);
    fx.run_hook(&base, edit(&fx.child.join("src/main.rs")));
    let reqs: Vec<String> = rx.try_iter().collect();
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert!(
        reqs[0].starts_with(&format!(
            "POST /api/v1/projects/{pid}/stream?repo_id={OTHER_REPO_ID} "
        )),
        "{reqs:?}"
    );
}
