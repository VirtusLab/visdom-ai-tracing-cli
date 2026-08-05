/// Shared git/worktree test fixtures used across multiple test modules.
///
/// This module is compiled only under `#[cfg(test)]` and is reachable as
/// `crate::test_helpers::...` from any test within the crate.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tokio::sync::{Mutex, MutexGuard};

/// How long a test waits for a captured request before failing, rather than
/// hanging forever if the client never connects.
pub const RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// Format a raw HTTP/1.1 response with `status` (e.g. `"200 OK"`), a JSON
/// `body`, and a correct `Content-Length`.
///
/// `Connection: close` is deliberate: it stops the client from keeping the
/// socket alive, so [`spawn_seq`]'s one-accept-per-response loop lines up
/// with one request per response.
pub fn http_json(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        body.len(),
        body
    )
}

/// Spawn a throwaway HTTP server that answers each successive request with
/// the next entry of `responses`, then stops accepting.
///
/// Returns the server's base URL and a channel of captured requests, one per
/// served request, each rendered as `"<request line> | <body>"` — enough to
/// assert on the method, path, and form fields of an OAuth token request.
/// The whole request (headers AND body) is consumed before the response is
/// written, so a client POSTing a form never sees its write end reset.
///
/// Extends the one-shot pattern in `tests/resolve_remote_test.rs` for
/// multi-step flows such as the RFC 8628 poll loop, where the same endpoint
/// must answer `authorization_pending` and then success.
pub fn spawn_seq(responses: Vec<String>) -> (String, mpsc::Receiver<String>) {
    spawn_seq_with(move |_| responses)
}

/// [`spawn_seq`] for responses that must embed the server's own URL — an
/// OpenID discovery document has to self-report the issuer it is served
/// from, and that isn't known until an ephemeral port is bound. `build`
/// receives the base URL and returns the response sequence.
pub fn spawn_seq_with<F>(build: F) -> (String, mpsc::Receiver<String>)
where
    F: FnOnce(&str) -> Vec<String>,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let responses = build(&base);
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for response in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            let captured = read_request(&stream);
            // A dropped receiver (test already finished asserting) must not
            // stop us from answering the client.
            let _ = tx.send(captured);
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (base, rx)
}

/// Read one complete HTTP request off `stream` and render it as
/// `"<request line> | <body>"`.
fn read_request(stream: &std::net::TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().expect("clone test socket"));
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
        if let Some(v) = header
            .strip_prefix("Content-Length:")
            .or_else(|| header.strip_prefix("content-length:"))
        {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        let _ = reader.read_exact(&mut body);
    }
    format!(
        "{} | {}",
        request_line.trim_end(),
        String::from_utf8_lossy(&body)
    )
}

/// Serializes tests that mutate process-wide env vars consulted by
/// config/credential resolution (`XDG_CONFIG_HOME`, `TRACEVAULT_SERVER_URL`/
/// `_API_KEY`, and anything else `dirs::config_dir()` or
/// `resolve_credentials` depend on). `cargo test` runs
/// `#[test]`/`#[tokio::test]` functions across multiple threads by default,
/// and env vars are process-global, so two such tests running concurrently
/// could otherwise clobber each other's value mid-test. Every test that sets
/// one of these vars for its duration should acquire this lock first (via
/// [`lock_env_mutation`] from a `#[tokio::test]`, or [`lock_env_mutation_sync`]
/// from a plain `#[test]`) and hold the guard until its own env-restoring
/// guard drops, so at most one such test runs at a time. An async-aware
/// `tokio::sync::Mutex` (rather than `std::sync::Mutex`) is used because some
/// callers hold the guard across `.await` points.
static ENV_MUTATION_LOCK: Mutex<()> = Mutex::const_new(());

/// Acquire [`ENV_MUTATION_LOCK`] from an async test. Never poisons (unlike
/// `std::sync::Mutex`): a panic while holding the guard doesn't taint the
/// lock for subsequent tests.
pub async fn lock_env_mutation() -> MutexGuard<'static, ()> {
    ENV_MUTATION_LOCK.lock().await
}

/// Acquire [`ENV_MUTATION_LOCK`] from a plain (non-async) test. Panics if
/// called from within a Tokio runtime on the current thread — only use this
/// from a plain `#[test]` function.
pub fn lock_env_mutation_sync() -> MutexGuard<'static, ()> {
    ENV_MUTATION_LOCK.blocking_lock()
}

/// Test-only: set/remove env vars for the guard's lifetime and restore their
/// PRIOR values (or absence) on drop, so a test that happens to run in a
/// process where one of these vars is already set doesn't permanently erase
/// it for the rest of the run. Use together with [`lock_env_mutation`]/
/// [`lock_env_mutation_sync`] so concurrent env-mutating tests don't
/// interleave; acquire that lock first and hold it until this guard drops.
pub struct EnvVarGuard {
    prev: Vec<(String, Option<String>)>,
}

impl Default for EnvVarGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl EnvVarGuard {
    pub fn new() -> Self {
        Self { prev: Vec::new() }
    }

    /// Set `key` to `val` for the guard's lifetime, remembering whatever
    /// value (or absence) it had beforehand.
    pub fn set(&mut self, key: &str, val: impl AsRef<std::ffi::OsStr>) {
        self.prev.push((key.to_string(), std::env::var(key).ok()));
        // SAFETY: test-only env mutation; callers are expected to hold
        // `ENV_MUTATION_LOCK` (via `lock_env_mutation`/`lock_env_mutation_sync`)
        // for the guard's lifetime, so no other test observes this var
        // concurrently.
        unsafe {
            std::env::set_var(key, val);
        }
    }

    /// Remove `key` for the guard's lifetime, remembering whatever value (or
    /// absence) it had beforehand.
    pub fn remove(&mut self, key: &str) {
        self.prev.push((key.to_string(), std::env::var(key).ok()));
        // SAFETY: see `set` above.
        unsafe {
            std::env::remove_var(key);
        }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        for (k, v) in self.prev.drain(..).rev() {
            // SAFETY: see `set` above.
            unsafe {
                match v {
                    Some(val) => std::env::set_var(&k, val),
                    None => std::env::remove_var(&k),
                }
            }
        }
    }
}

/// Initialise a git repo at `dir` with an empty initial commit.
///
/// Configures `user.email` and `user.name` locally so the commit succeeds in
/// environments that have no global git identity set.
pub fn init_git_repo(dir: &Path) {
    let ok = Command::new("git")
        .args(["-C", &dir.to_string_lossy(), "init"])
        .status()
        .expect("git init failed")
        .success();
    assert!(ok, "git init must succeed");

    for (key, val) in [("user.email", "test@example.com"), ("user.name", "Test")] {
        Command::new("git")
            .args(["-C", &dir.to_string_lossy(), "config", key, val])
            .status()
            .expect("git config failed");
    }

    let ok = Command::new("git")
        .args([
            "-C",
            &dir.to_string_lossy(),
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .status()
        .expect("git commit failed")
        .success();
    assert!(ok, "init commit must succeed");
}

/// Add a detached linked worktree at `wt_dir` branching from `repo_dir`.
pub fn add_worktree(repo_dir: &Path, wt_dir: &Path) {
    let ok = Command::new("git")
        .args([
            "-C",
            &repo_dir.to_string_lossy(),
            "worktree",
            "add",
            "--detach",
            &wt_dir.to_string_lossy(),
        ])
        .status()
        .expect("git worktree add failed")
        .success();
    assert!(ok, "git worktree add must succeed");
}
