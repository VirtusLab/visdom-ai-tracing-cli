/// Shared git/worktree test fixtures used across multiple test modules.
///
/// This module is compiled only under `#[cfg(test)]` and is reachable as
/// `crate::test_helpers::...` from any test within the crate.
use std::path::Path;
use std::process::Command;

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
