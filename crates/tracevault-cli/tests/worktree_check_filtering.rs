//! End-to-end: from a sibling worktree, `check` selects only that worktree's
//! sessions (plus unmarked legacy), excluding sibling A's marked sessions.
use std::path::Path;
use std::process::Command;

use tracevault_cli::commands::check::select_worktree_sessions;
use tracevault_cli::paths::worktree_toplevel;

/// Run a git command and assert it succeeded, so a failed setup step surfaces
/// immediately instead of letting the test silently validate the non-git
/// fallback path.
fn git_ok(args: &[&str]) {
    let status = Command::new("git").args(args).status().unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn init_git_repo(dir: &Path) {
    git_ok(&["-C", &dir.to_string_lossy(), "init"]);
    for (k, v) in [("user.email", "t@e.com"), ("user.name", "T")] {
        git_ok(&["-C", &dir.to_string_lossy(), "config", k, v]);
    }
    git_ok(&[
        "-C",
        &dir.to_string_lossy(),
        "commit",
        "--allow-empty",
        "-m",
        "init",
    ]);
}
fn add_worktree(repo: &Path, wt: &Path) {
    git_ok(&[
        "-C",
        &repo.to_string_lossy(),
        "worktree",
        "add",
        "--detach",
        &wt.to_string_lossy(),
    ]);
}

#[test]
fn check_from_sibling_excludes_other_worktree_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let wt = tmp.path().join("sibling");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);
    add_worktree(&repo, &wt);

    let primary_top = worktree_toplevel(&repo);
    let sibling_top = worktree_toplevel(&wt);

    let sessions = repo.join(".tracevault").join("sessions");
    for (id, origin) in [
        ("primary-sess", Some(primary_top.as_str())),
        ("sibling-sess", Some(sibling_top.as_str())),
        ("legacy-sess", None),
    ] {
        let d = sessions.join(id);
        std::fs::create_dir_all(&d).unwrap();
        if let Some(o) = origin {
            std::fs::write(d.join("origin"), o).unwrap();
        }
    }

    let dirs: Vec<_> = std::fs::read_dir(&sessions)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    let (kept, fell_back) = select_worktree_sessions(dirs, &sibling_top);

    assert!(!fell_back);
    let names: Vec<String> = kept
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"sibling-sess".to_string()));
    assert!(names.contains(&"legacy-sess".to_string()));
    assert!(
        !names.contains(&"primary-sess".to_string()),
        "primary worktree's marked session must be excluded when pushing from sibling"
    );
}
