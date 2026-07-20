/// Shared git/worktree test fixtures used across multiple test modules.
///
/// This module is compiled only under `#[cfg(test)]` and is reachable as
/// `crate::test_helpers::...` from any test within the crate.
use std::path::Path;
use std::process::Command;
use tokio::sync::{Mutex, MutexGuard};

/// Serializes tests that mutate process-wide env vars consulted by
/// config/credential resolution (`XDG_CONFIG_HOME`, `TRACEVAULT_SERVER_URL`/
/// `_ORG_SLUG`/`_API_KEY`, and anything else `dirs::config_dir()` or
/// `resolve_credentials`/`org_slug_for` depend on). `cargo test` runs
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
