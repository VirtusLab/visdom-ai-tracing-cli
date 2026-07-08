//! End-to-end test for the user-level context layer (Task 10).
//!
//! Exercises the full *enabled* path, which prior tasks only covered at the
//! unit level (see Task 5's report): a real temp git repo standing in for a
//! `tracevault init`-ed project, `context source --path <file>` wiring
//! `config.toml`'s `user_context` at an explicit file, `context set --user`
//! writing that file through the real command, a repo-level context with an
//! overriding param and a `null` tombstone, and `Context::effective` merging
//! all of it with the documented precedence (user < repo < worktree).

use std::path::Path;
use std::process::Command;
use tracevault_cli::commands::context::{run_set, run_source};
use tracevault_cli::config::TracevaultConfig;
use tracevault_cli::context::{resolve_context_paths, Context};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Initialize a bare-minimum git repo: `git init` + empty commit.
/// Mirrors the helper used by `tests/worktree_context_test.rs`.
fn init_git_repo(dir: &Path) {
    let status = Command::new("git")
        .args(["-C", &dir.to_string_lossy(), "init"])
        .status()
        .expect("git init failed");
    assert!(status.success(), "git init must succeed");

    for (key, val) in [
        ("user.email", "test@example.com"),
        ("user.name", "Test User"),
    ] {
        Command::new("git")
            .args(["-C", &dir.to_string_lossy(), "config", key, val])
            .status()
            .expect("git config failed");
    }

    let status = Command::new("git")
        .args([
            "-C",
            &dir.to_string_lossy(),
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .status()
        .expect("git commit failed");
    assert!(status.success(), "empty init commit must succeed");
}

/// Build a `tracevault init`-shaped project: a real git repo with a
/// `.tracevault/` dir and a default `config.toml` (user-context disabled,
/// matching the compat default for a config that hasn't opted in yet).
fn init_project(repo_dir: &Path) {
    std::fs::create_dir_all(repo_dir).unwrap();
    init_git_repo(repo_dir);
    std::fs::create_dir_all(repo_dir.join(".tracevault")).unwrap();
    std::fs::write(
        TracevaultConfig::config_path(repo_dir),
        TracevaultConfig::default().to_toml(),
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// Enabled path: user layer merges under repo overrides
// ---------------------------------------------------------------------------

#[test]
fn user_context_enabled_via_source_merges_under_repo_overrides() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    init_project(&repo_dir);

    // Before enabling, the config's user-context is unset (compat default) —
    // which means it inherits the user-level config rather than being forced off.
    let config = TracevaultConfig::load(&repo_dir).unwrap();
    assert!(
        config.user_context.is_none(),
        "user_context must default to unset until explicitly enabled"
    );

    // The user-level context file lives outside the repo (cross-repo, like
    // `~/.config/tracevault/context.json`) — point `context source` at it.
    let user_home = tempfile::tempdir().unwrap();
    let user_ctx_path = user_home.path().join("context.json");

    run_source(
        &repo_dir,
        false,
        false,
        Some(user_ctx_path.display().to_string()),
        false,
    )
    .unwrap();

    // config.toml now resolves the user layer to our explicit path.
    let config = TracevaultConfig::load(&repo_dir).unwrap();
    let user_layer = config.user_context.and_then(|uc| uc.resolve());
    assert_eq!(
        user_layer.as_deref(),
        Some(user_ctx_path.as_path()),
        "context source --path must be reflected in the resolved user layer"
    );

    // Write the user-level context through the real `context set --user`
    // command (not by poking the file directly): a personal label plus a
    // param the repo will later override.
    run_set(
        &repo_dir,
        Some("user-flow".to_string()),
        vec!["personal".to_string()],
        vec!["owner=alice".to_string()],
        false,
        true, // --user
    )
    .unwrap();
    assert_eq!(
        Context::load_from(&user_ctx_path).labels,
        vec!["personal".to_string()],
        "context set --user must write the resolved user-context file"
    );

    // Write the repo-level (global) context: overrides `owner` and adds its
    // own label.
    let repo_ctx = Context {
        flow_id: Some("repo-flow".to_string()),
        labels: vec!["repo-label".to_string()],
        params: [("owner".to_string(), Some("repo-team".to_string()))]
            .into_iter()
            .collect(),
    };
    let paths = resolve_context_paths(&repo_dir);
    repo_ctx.save_global(&paths).unwrap();

    // Merge: user < repo (no worktree layer in this scenario).
    let effective = Context::effective(&repo_dir, user_layer.as_deref());

    // flow: repo (higher precedence) overrides user.
    assert_eq!(
        effective.flow_id,
        Some("repo-flow".to_string()),
        "repo flow must override the user-layer flow"
    );

    // labels: union of user + repo.
    assert!(
        effective.labels.contains(&"personal".to_string()),
        "user label must survive into the effective context"
    );
    assert!(
        effective.labels.contains(&"repo-label".to_string()),
        "repo label must survive into the effective context"
    );

    // params: repo overrides the user-set `owner`.
    assert_eq!(
        effective.params.get("owner").map(String::as_str),
        Some("repo-team"),
        "repo param must win over the user-layer value for the same key"
    );
}

// ---------------------------------------------------------------------------
// Enabled path: a repo-level `null` tombstone drops an inherited user param
// ---------------------------------------------------------------------------

#[test]
fn repo_tombstone_drops_inherited_user_param() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    init_project(&repo_dir);

    let user_home = tempfile::tempdir().unwrap();
    let user_ctx_path = user_home.path().join("context.json");

    run_source(
        &repo_dir,
        false,
        false,
        Some(user_ctx_path.display().to_string()),
        false,
    )
    .unwrap();
    let user_layer = TracevaultConfig::load(&repo_dir)
        .unwrap()
        .user_context
        .and_then(|uc| uc.resolve());
    assert!(user_layer.is_some(), "user layer must be enabled");

    // User sets a param the repo doesn't want inherited by downstream events
    // (e.g. a personal API host or a scratch label).
    run_set(
        &repo_dir,
        None,
        vec![],
        vec!["secret=shh".to_string(), "keep=user-value".to_string()],
        false,
        true, // --user
    )
    .unwrap();

    // Repo tombstones `secret` (JSON `null`) while leaving `keep` untouched.
    let mut repo_params = std::collections::BTreeMap::new();
    repo_params.insert("secret".to_string(), None); // tombstone
    let repo_ctx = Context {
        flow_id: None,
        labels: vec![],
        params: repo_params,
    };
    let paths = resolve_context_paths(&repo_dir);
    repo_ctx.save_global(&paths).unwrap();

    // Sanity: the tombstone really is a JSON null on disk.
    let raw = std::fs::read_to_string(paths.global_path()).unwrap();
    assert!(
        raw.contains("\"secret\": null"),
        "repo-level removal must be stored as a null tombstone, not a deleted key"
    );

    let effective = Context::effective(&repo_dir, user_layer.as_deref());

    assert!(
        !effective.params.contains_key("secret"),
        "repo-level null tombstone must drop the inherited user param entirely"
    );
    assert_eq!(
        effective.params.get("keep").map(String::as_str),
        Some("user-value"),
        "a param the repo doesn't touch must still flow through from the user layer"
    );
}
