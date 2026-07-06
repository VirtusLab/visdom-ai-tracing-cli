/// Integration tests for worktree-aware context resolution and commands (Tasks W1 + W2, #258).
///
/// These tests create REAL temporary git repos and linked worktrees via
/// `std::process::Command` to exercise the actual `git rev-parse` codepath.
use std::path::Path;
use std::process::Command;
use tracevault_cli::commands::context::{run_clear, run_set, run_update};
use tracevault_cli::context::{resolve_context_paths, Context, WorktreeScope};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Initialize a bare-minimum git repo: `git init` + empty commit.
/// Returns the path to the repo root.
fn init_git_repo(dir: &Path) {
    let status = Command::new("git")
        .args(["-C", &dir.to_string_lossy(), "init"])
        .status()
        .expect("git init failed");
    assert!(status.success(), "git init must succeed");

    // Configure a throwaway identity so the commit doesn't fail in CI.
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

/// Add a linked worktree at `wt_dir` (detached HEAD).
fn add_worktree(repo_dir: &Path, wt_dir: &Path) {
    let status = Command::new("git")
        .args([
            "-C",
            &repo_dir.to_string_lossy(),
            "worktree",
            "add",
            "--detach",
            &wt_dir.to_string_lossy(),
        ])
        .status()
        .expect("git worktree add failed");
    assert!(status.success(), "git worktree add must succeed");
}

/// Helper: build a Context from components.
fn make_ctx(flow: Option<&str>, labels: &[&str], params: &[(&str, &str)]) -> Context {
    Context {
        flow_id: flow.map(str::to_string),
        labels: labels.iter().map(|s| s.to_string()).collect(),
        params: params
            .iter()
            .map(|(k, v)| (k.to_string(), Some(v.to_string())))
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Resolution tests using a real git repo
// ---------------------------------------------------------------------------

#[test]
fn resolution_from_primary_is_primary_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);

    // Create .tracevault/ so fallback walk-up also works, but git path should
    // still be used and point to repo_dir.
    std::fs::create_dir_all(repo_dir.join(".tracevault")).unwrap();

    let paths = resolve_context_paths(&repo_dir);

    // tracevault_dir must be repo_dir/.tracevault
    let expected_tv = repo_dir.canonicalize().unwrap().join(".tracevault");
    assert_eq!(
        paths.tracevault_dir.canonicalize().unwrap(),
        expected_tv,
        "tracevault_dir should be repo_dir/.tracevault"
    );

    // Scope must be Primary
    assert_eq!(
        paths.scope,
        WorktreeScope::Primary,
        "primary checkout must yield Primary scope"
    );

    // worktree_path must be None
    assert!(
        paths.worktree_path().is_none(),
        "Primary scope has no per-worktree path"
    );
}

#[test]
fn resolution_from_linked_worktree_is_linked_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wt_dir = tmp.path().join("wt");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);
    std::fs::create_dir_all(repo_dir.join(".tracevault")).unwrap();
    add_worktree(&repo_dir, &wt_dir);

    // Resolve from the LINKED worktree directory.
    let paths = resolve_context_paths(&wt_dir);

    // tracevault_dir must still be repo_dir/.tracevault (via common-dir parent).
    let expected_tv = repo_dir.canonicalize().unwrap().join(".tracevault");
    assert_eq!(
        paths.tracevault_dir.canonicalize().unwrap(),
        expected_tv,
        "tracevault_dir should always point to primary repo's .tracevault/"
    );

    // Scope must be Linked.
    match &paths.scope {
        WorktreeScope::Linked { key } => {
            assert!(!key.is_empty(), "worktree key must not be empty");
            // The key is the basename of `.git/worktrees/<key>`.
            // Verify the worktree path uses it.
            let expected_wt_path = expected_tv.join("worktrees").join(key).join("context.json");
            assert_eq!(
                paths
                    .worktree_path()
                    .unwrap()
                    .canonicalize()
                    .unwrap_or_else(|_| paths.worktree_path().unwrap()),
                // Can't canonicalize a non-existent file; compare without that.
                expected_wt_path,
                "worktree_path should be tracevault_dir/worktrees/<key>/context.json"
            );
        }
        WorktreeScope::Primary => panic!("linked worktree must resolve to Linked scope"),
    }
}

#[test]
fn linked_worktree_key_is_basename_of_git_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wt_dir = tmp.path().join("wt");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);
    add_worktree(&repo_dir, &wt_dir);

    let paths = resolve_context_paths(&wt_dir);

    // The key should equal the basename of the git-dir for the linked worktree,
    // which git stores at <primary>/.git/worktrees/<key>.
    // We can verify by checking that the key appears as a directory under .git/worktrees/.
    let worktrees_dir = repo_dir
        .canonicalize()
        .unwrap()
        .join(".git")
        .join("worktrees");
    if let WorktreeScope::Linked { key } = &paths.scope {
        assert!(
            worktrees_dir.join(key).exists(),
            "key '{}' must be a valid directory under .git/worktrees/",
            key
        );
    } else {
        panic!("expected Linked scope");
    }
}

// ---------------------------------------------------------------------------
// Merge rules (uses real git repo + linked worktree)
// ---------------------------------------------------------------------------

#[test]
fn merge_rules_flow_labels_params_with_real_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wt_dir = tmp.path().join("wt");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);
    std::fs::create_dir_all(repo_dir.join(".tracevault")).unwrap();
    add_worktree(&repo_dir, &wt_dir);

    // Write global context.
    let global = make_ctx(Some("g"), &["a"], &[("k", "1")]);
    let primary_paths = resolve_context_paths(&repo_dir);
    global.save_to(&primary_paths.global_path()).unwrap();

    // Write per-worktree context.
    let wt_ctx = make_ctx(Some("w"), &["b"], &[("k", "2"), ("m", "3")]);
    let linked_paths = resolve_context_paths(&wt_dir);
    let wt_path = linked_paths
        .worktree_path()
        .expect("must have a worktree path");
    wt_ctx.save_to(&wt_path).unwrap();

    // Load effective from the linked worktree dir.
    let effective = Context::effective(&wt_dir, None);

    // flow: worktree wins
    assert_eq!(
        effective.flow_id,
        Some("w".to_string()),
        "flow: worktree wins"
    );
    // labels: global ∪ worktree, global-first
    assert_eq!(
        effective.labels,
        vec!["a", "b"],
        "labels: global ∪ worktree"
    );
    // params: worktree overrides
    assert_eq!(effective.params["k"], "2", "params: worktree overrides k");
    assert_eq!(effective.params["m"], "3", "params: worktree adds m");
}

#[test]
fn effective_from_primary_equals_global() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);
    std::fs::create_dir_all(repo_dir.join(".tracevault")).unwrap();

    let global = make_ctx(Some("g"), &["a"], &[("k", "1")]);
    let paths = resolve_context_paths(&repo_dir);
    global.save_to(&paths.global_path()).unwrap();

    let effective = Context::effective(&repo_dir, None);
    // effective is now an EffectiveContext; from a primary checkout with no user
    // layer it must equal the single-layer fold of the global context.
    assert_eq!(
        effective,
        Context::merge_layers(&[&global]),
        "from primary, effective == global"
    );
}

#[test]
fn worktree_file_namespaced_does_not_affect_global() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wt_dir = tmp.path().join("wt");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);
    std::fs::create_dir_all(repo_dir.join(".tracevault")).unwrap();
    add_worktree(&repo_dir, &wt_dir);

    // Write global.
    let global = make_ctx(Some("global-flow"), &["global-label"], &[]);
    let primary_paths = resolve_context_paths(&repo_dir);
    global.save_to(&primary_paths.global_path()).unwrap();

    // Write worktree context.
    let wt_ctx = make_ctx(Some("wt-flow"), &["wt-label"], &[]);
    let linked_paths = resolve_context_paths(&wt_dir);
    wt_ctx
        .save_to(&linked_paths.worktree_path().unwrap())
        .unwrap();

    // From primary, global is unchanged.
    let loaded_global = Context::load_from(&primary_paths.global_path());
    assert_eq!(
        loaded_global.flow_id,
        Some("global-flow".to_string()),
        "global file must not be touched by worktree save"
    );
}

// ---------------------------------------------------------------------------
// Git-failure fallback
// ---------------------------------------------------------------------------

#[test]
fn git_failure_fallback_walks_up_to_tracevault() {
    // A non-git tempdir with a .tracevault/ should use that directory.
    let tmp = tempfile::tempdir().unwrap();
    let tv_dir = tmp.path().join(".tracevault");
    std::fs::create_dir_all(&tv_dir).unwrap();

    // Subdirectory inside the non-git tree.
    let subdir = tmp.path().join("sub/dir");
    std::fs::create_dir_all(&subdir).unwrap();

    let paths = resolve_context_paths(&subdir);

    let expected_tv = tmp.path().canonicalize().unwrap().join(".tracevault");
    // If we can canonicalize the resolved dir, check it; otherwise compare as-is.
    let resolved_tv = paths
        .tracevault_dir
        .canonicalize()
        .unwrap_or_else(|_| paths.tracevault_dir.clone());
    assert_eq!(
        resolved_tv, expected_tv,
        "fallback should walk up to nearest .tracevault/"
    );
    assert_eq!(
        paths.scope,
        WorktreeScope::Primary,
        "fallback must yield Primary scope"
    );
    assert!(
        paths.worktree_path().is_none(),
        "fallback has no worktree path"
    );
}

#[test]
fn git_failure_fallback_no_panic_on_no_tracevault() {
    // A non-git tempdir WITHOUT a .tracevault/ should not panic.
    let tmp = tempfile::tempdir().unwrap();
    // Ensure there's no .tracevault/ in any parent (tmp dirs are usually under /tmp).
    let paths = resolve_context_paths(tmp.path());
    // Should return Primary scope (last-resort path).
    assert_eq!(paths.scope, WorktreeScope::Primary);
    // tracevault_dir should end with ".tracevault".
    assert_eq!(paths.tracevault_dir.file_name().unwrap(), ".tracevault");
}

// ---------------------------------------------------------------------------
// save_to creates worktrees/<key>/ directory
// ---------------------------------------------------------------------------

#[test]
fn save_to_creates_nested_worktrees_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wt_dir = tmp.path().join("wt");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);
    std::fs::create_dir_all(repo_dir.join(".tracevault")).unwrap();
    add_worktree(&repo_dir, &wt_dir);

    let linked_paths = resolve_context_paths(&wt_dir);
    let wt_path = linked_paths
        .worktree_path()
        .expect("must have worktree path");

    assert!(!wt_path.exists(), "file must not exist before save");

    let ctx = make_ctx(Some("wt"), &["tag"], &[]);
    ctx.save_to(&wt_path).unwrap();

    assert!(wt_path.exists(), "save_to must create the file");
    let loaded = Context::load_from(&wt_path);
    assert_eq!(loaded.flow_id, Some("wt".to_string()));
}

// ---------------------------------------------------------------------------
// effective_with_parts
// ---------------------------------------------------------------------------

#[test]
fn effective_with_parts_returns_all_three() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wt_dir = tmp.path().join("wt");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);
    std::fs::create_dir_all(repo_dir.join(".tracevault")).unwrap();
    add_worktree(&repo_dir, &wt_dir);

    // Write contexts.
    let global = make_ctx(Some("g"), &["a"], &[("k", "1")]);
    let primary_paths = resolve_context_paths(&repo_dir);
    global.save_to(&primary_paths.global_path()).unwrap();

    let wt_ctx = make_ctx(Some("w"), &["b"], &[("k", "99")]);
    let linked_paths = resolve_context_paths(&wt_dir);
    wt_ctx
        .save_to(&linked_paths.worktree_path().unwrap())
        .unwrap();

    let (_paths, _user, loaded_global, loaded_wt, effective) =
        Context::effective_with_parts(&wt_dir, None);

    assert_eq!(loaded_global.flow_id, Some("g".to_string()));
    assert_eq!(
        loaded_wt.as_ref().and_then(|w| w.flow_id.as_deref()),
        Some("w")
    );
    assert_eq!(effective.flow_id, Some("w".to_string()));
    assert_eq!(effective.params["k"], "99");
}

// ===========================================================================
// Task W2 acceptance tests — subcommand wiring
// ===========================================================================

// ---------------------------------------------------------------------------
// W2-T1: `set` from a linked worktree writes the namespaced file only
// ---------------------------------------------------------------------------

#[test]
fn set_from_linked_worktree_writes_worktree_file_not_global() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wt_dir = tmp.path().join("wt");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);
    std::fs::create_dir_all(repo_dir.join(".tracevault")).unwrap();
    add_worktree(&repo_dir, &wt_dir);

    // Write a known global context first.
    let global_before = make_ctx(Some("global-flow"), &["global-label"], &[]);
    let primary_paths = resolve_context_paths(&repo_dir);
    global_before.save_to(&primary_paths.global_path()).unwrap();

    // Now `set` from the linked worktree (no --global).
    run_set(
        &wt_dir,
        Some("wt-flow".to_string()),
        vec!["wt-label".to_string()],
        vec![],
        false, // not --global
        false,
    )
    .unwrap();

    // Per-worktree file must exist and hold the new context.
    let linked_paths = resolve_context_paths(&wt_dir);
    let wt_path = linked_paths.worktree_path().expect("linked scope");
    assert!(
        wt_path.exists(),
        "per-worktree context.json must be created"
    );
    let wt_ctx = Context::load_from(&wt_path);
    assert_eq!(wt_ctx.flow_id, Some("wt-flow".to_string()));
    assert_eq!(wt_ctx.labels, vec!["wt-label"]);

    // Global file must be UNCHANGED.
    let global_after = Context::load_from(&primary_paths.global_path());
    assert_eq!(
        global_after.flow_id,
        Some("global-flow".to_string()),
        "global context must not be modified by a linked-worktree set"
    );
}

// ---------------------------------------------------------------------------
// W2-T2: `set --global` from a linked worktree writes the global file
// ---------------------------------------------------------------------------

#[test]
fn set_global_flag_from_linked_worktree_writes_global_file() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wt_dir = tmp.path().join("wt");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);
    std::fs::create_dir_all(repo_dir.join(".tracevault")).unwrap();
    add_worktree(&repo_dir, &wt_dir);

    // `set --global` from the linked worktree.
    run_set(
        &wt_dir,
        Some("forced-global-flow".to_string()),
        vec!["forced-label".to_string()],
        vec![],
        true, // --global
        false,
    )
    .unwrap();

    // Global file must hold the new context.
    let primary_paths = resolve_context_paths(&repo_dir);
    let global_ctx = Context::load_from(&primary_paths.global_path());
    assert_eq!(
        global_ctx.flow_id,
        Some("forced-global-flow".to_string()),
        "--global flag must write to the global file"
    );

    // Per-worktree file must NOT exist (we didn't write it).
    let linked_paths = resolve_context_paths(&wt_dir);
    let wt_path = linked_paths.worktree_path().expect("linked scope");
    assert!(
        !wt_path.exists(),
        "--global set must not create a per-worktree file"
    );
}

// ---------------------------------------------------------------------------
// W2-T3: `set` from the PRIMARY checkout writes the global file
// ---------------------------------------------------------------------------

#[test]
fn set_from_primary_writes_global_file() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);
    std::fs::create_dir_all(repo_dir.join(".tracevault")).unwrap();

    run_set(
        &repo_dir,
        Some("primary-flow".to_string()),
        vec!["primary-label".to_string()],
        vec![],
        false,
        false,
    )
    .unwrap();

    let paths = resolve_context_paths(&repo_dir);
    let global_ctx = Context::load_from(&paths.global_path());
    assert_eq!(
        global_ctx.flow_id,
        Some("primary-flow".to_string()),
        "set from primary must write to global file"
    );
}

// ---------------------------------------------------------------------------
// W2-T4: effective context in linked worktree = global merged with its own
// ---------------------------------------------------------------------------

#[test]
fn linked_worktree_effective_is_merge_of_global_and_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wt_dir = tmp.path().join("wt");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);
    std::fs::create_dir_all(repo_dir.join(".tracevault")).unwrap();
    add_worktree(&repo_dir, &wt_dir);

    // Write global context from primary.
    run_set(
        &repo_dir,
        Some("global-flow".to_string()),
        vec!["global-label".to_string()],
        vec!["shared=yes".to_string(), "global-only=1".to_string()],
        false,
        false,
    )
    .unwrap();

    // Write per-worktree context from linked worktree.
    run_set(
        &wt_dir,
        Some("wt-flow".to_string()),  // overrides global flow
        vec!["wt-label".to_string()], // added to global labels
        vec!["shared=override".to_string(), "wt-only=2".to_string()],
        false,
        false,
    )
    .unwrap();

    // Effective from linked worktree.
    let effective = Context::effective(&wt_dir, None);

    // flow: worktree wins
    assert_eq!(
        effective.flow_id,
        Some("wt-flow".to_string()),
        "worktree flow must win"
    );
    // labels: union
    assert!(
        effective.labels.contains(&"global-label".to_string()),
        "global label must be present"
    );
    assert!(
        effective.labels.contains(&"wt-label".to_string()),
        "wt label must be present"
    );
    // params: worktree overrides shared key
    assert_eq!(
        effective.params.get("shared").map(String::as_str),
        Some("override")
    );
    assert_eq!(
        effective.params.get("global-only").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        effective.params.get("wt-only").map(String::as_str),
        Some("2")
    );
}

// ---------------------------------------------------------------------------
// W2-T5: update from linked worktree, no --global, writes worktree file
// ---------------------------------------------------------------------------

#[test]
fn update_from_linked_worktree_writes_worktree_file() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wt_dir = tmp.path().join("wt");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);
    std::fs::create_dir_all(repo_dir.join(".tracevault")).unwrap();
    add_worktree(&repo_dir, &wt_dir);

    // Set an initial per-worktree context.
    run_set(
        &wt_dir,
        Some("initial".to_string()),
        vec!["a".to_string()],
        vec![],
        false,
        false,
    )
    .unwrap();

    // Update from the linked worktree.
    run_update(
        &wt_dir,
        Some("updated".to_string()),
        vec!["b".to_string()],
        vec![],
        vec![],
        vec![],
        false,
        false,
    )
    .unwrap();

    let linked_paths = resolve_context_paths(&wt_dir);
    let wt_ctx = Context::load_from(&linked_paths.worktree_path().unwrap());
    assert_eq!(wt_ctx.flow_id, Some("updated".to_string()));
    assert_eq!(wt_ctx.labels, vec!["a", "b"]);
}

// ---------------------------------------------------------------------------
// W2-T6: clear from linked worktree, no --global, DELETES the worktree file
// ---------------------------------------------------------------------------

#[test]
fn clear_from_linked_worktree_deletes_worktree_file_not_global() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wt_dir = tmp.path().join("wt");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);
    std::fs::create_dir_all(repo_dir.join(".tracevault")).unwrap();
    add_worktree(&repo_dir, &wt_dir);

    // Global context.
    run_set(
        &repo_dir,
        Some("global-flow".to_string()),
        vec!["g-label".to_string()],
        vec![],
        false,
        false,
    )
    .unwrap();

    // Linked worktree context.
    run_set(
        &wt_dir,
        Some("wt-flow".to_string()),
        vec![],
        vec![],
        false,
        false,
    )
    .unwrap();

    // Confirm the per-worktree file exists before clearing.
    let linked_paths = resolve_context_paths(&wt_dir);
    let wt_path = linked_paths
        .worktree_path()
        .expect("linked scope must have worktree path");
    assert!(
        wt_path.exists(),
        "per-worktree file must exist before clear"
    );

    // Clear from linked worktree (no --global).
    run_clear(&wt_dir, false, false).unwrap();

    // After clear, the per-worktree file must NOT exist (absent-vs-empty invariant).
    assert!(
        !wt_path.exists(),
        "clear on linked worktree must DELETE the per-worktree file, not write an empty one"
    );

    // load_worktree must return None (file absent → no per-worktree context).
    let wt_opt = Context::load_worktree(&linked_paths);
    assert!(
        wt_opt.is_none(),
        "load_worktree must return None after clear (file deleted)"
    );

    // Effective must fall back to global (since there is no per-worktree context).
    let effective = Context::effective(&wt_dir, None);
    assert_eq!(
        effective.flow_id,
        Some("global-flow".to_string()),
        "effective must fall back to global flow after worktree clear"
    );
    assert_eq!(
        effective.labels,
        vec!["g-label"],
        "effective must fall back to global labels after worktree clear"
    );

    // Global must be unchanged.
    let primary_paths = resolve_context_paths(&repo_dir);
    let global_ctx = Context::load_from(&primary_paths.global_path());
    assert_eq!(
        global_ctx.flow_id,
        Some("global-flow".to_string()),
        "global context must not be affected by linked-worktree clear"
    );
}

// ---------------------------------------------------------------------------
// W2-T6b: clear on a linked worktree where the file doesn't yet exist (no-op)
// ---------------------------------------------------------------------------

#[test]
fn clear_from_linked_worktree_with_no_file_is_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wt_dir = tmp.path().join("wt");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);
    std::fs::create_dir_all(repo_dir.join(".tracevault")).unwrap();
    add_worktree(&repo_dir, &wt_dir);

    // Do NOT write any per-worktree context (file absent from the start).
    let linked_paths = resolve_context_paths(&wt_dir);
    let wt_path = linked_paths.worktree_path().expect("linked scope");
    assert!(!wt_path.exists(), "sanity: file must be absent before test");

    // Clear must not error even if the file doesn't exist.
    run_clear(&wt_dir, false, false)
        .expect("clear must succeed even when per-worktree file is absent");

    // File must still be absent.
    assert!(
        !wt_path.exists(),
        "file must remain absent after clear on absent file"
    );

    // load_worktree must still return None.
    assert!(
        Context::load_worktree(&linked_paths).is_none(),
        "load_worktree must return None after no-op clear"
    );
}

// ---------------------------------------------------------------------------
// W2-T8: two linked worktrees stay isolated from each other
// ---------------------------------------------------------------------------

#[test]
fn two_linked_worktrees_are_isolated() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wt_a_dir = tmp.path().join("wt_a");
    let wt_b_dir = tmp.path().join("wt_b");

    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);
    std::fs::create_dir_all(repo_dir.join(".tracevault")).unwrap();
    add_worktree(&repo_dir, &wt_a_dir);
    add_worktree(&repo_dir, &wt_b_dir);

    // Write a shared global context from primary.
    run_set(
        &repo_dir,
        Some("global-flow".to_string()),
        vec!["global-label".to_string()],
        vec!["env=prod".to_string()],
        false,
        false,
    )
    .unwrap();

    // Set DIFFERENT context in worktree A.
    run_set(
        &wt_a_dir,
        Some("flow-A".to_string()),
        vec!["label-A".to_string()],
        vec!["wt=A".to_string()],
        false,
        false,
    )
    .unwrap();

    // Set DIFFERENT context in worktree B.
    run_set(
        &wt_b_dir,
        Some("flow-B".to_string()),
        vec!["label-B".to_string()],
        vec!["wt=B".to_string()],
        false,
        false,
    )
    .unwrap();

    // Verify effective contexts are distinct.
    let effective_a = Context::effective(&wt_a_dir, None);
    let effective_b = Context::effective(&wt_b_dir, None);

    // Worktree A: sees flow-A, label-A (not B).
    assert_eq!(
        effective_a.flow_id,
        Some("flow-A".to_string()),
        "worktree A must see its own flow"
    );
    assert!(
        effective_a.labels.contains(&"label-A".to_string()),
        "worktree A must see label-A"
    );
    assert!(
        !effective_a.labels.contains(&"label-B".to_string()),
        "worktree A must NOT see label-B"
    );
    assert_eq!(
        effective_a.params.get("wt").map(String::as_str),
        Some("A"),
        "worktree A param wt must be A"
    );

    // Worktree B: sees flow-B, label-B (not A).
    assert_eq!(
        effective_b.flow_id,
        Some("flow-B".to_string()),
        "worktree B must see its own flow"
    );
    assert!(
        effective_b.labels.contains(&"label-B".to_string()),
        "worktree B must see label-B"
    );
    assert!(
        !effective_b.labels.contains(&"label-A".to_string()),
        "worktree B must NOT see label-A"
    );
    assert_eq!(
        effective_b.params.get("wt").map(String::as_str),
        Some("B"),
        "worktree B param wt must be B"
    );

    // Both worktrees inherit the global label and param.
    assert!(
        effective_a.labels.contains(&"global-label".to_string()),
        "worktree A must inherit global label"
    );
    assert!(
        effective_b.labels.contains(&"global-label".to_string()),
        "worktree B must inherit global label"
    );

    // Each worktree wrote a DISTINCT namespaced file; neither modified global.
    let paths_a = resolve_context_paths(&wt_a_dir);
    let paths_b = resolve_context_paths(&wt_b_dir);
    let wt_path_a = paths_a
        .worktree_path()
        .expect("wt_a must have a worktree path");
    let wt_path_b = paths_b
        .worktree_path()
        .expect("wt_b must have a worktree path");

    // The two worktree paths must be different files.
    assert_ne!(
        wt_path_a, wt_path_b,
        "worktrees A and B must use distinct context file paths"
    );

    // Both per-worktree files exist.
    assert!(wt_path_a.exists(), "worktree A context file must exist");
    assert!(wt_path_b.exists(), "worktree B context file must exist");

    // Global must be unchanged (flow/labels/params set only from primary).
    let primary_paths = resolve_context_paths(&repo_dir);
    let global_ctx = Context::load_from(&primary_paths.global_path());
    assert_eq!(
        global_ctx.flow_id,
        Some("global-flow".to_string()),
        "global context must be unchanged after worktree-scoped sets"
    );
    assert_eq!(
        global_ctx.labels,
        vec!["global-label"],
        "global labels must be unchanged after worktree-scoped sets"
    );
}

// ---------------------------------------------------------------------------
// W2-T9: update --remove-label / --remove-param at LINKED worktree scope
// ---------------------------------------------------------------------------

#[test]
fn update_remove_label_and_param_at_worktree_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wt_dir = tmp.path().join("wt");

    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);
    std::fs::create_dir_all(repo_dir.join(".tracevault")).unwrap();
    add_worktree(&repo_dir, &wt_dir);

    // Set a known global context that we want to remain untouched.
    run_set(
        &repo_dir,
        None,
        vec!["global-only".to_string()],
        vec!["g=global".to_string()],
        false,
        false,
    )
    .unwrap();

    // Set initial per-worktree context with two labels and two params.
    run_set(
        &wt_dir,
        None,
        vec!["keep-label".to_string(), "drop-label".to_string()],
        vec!["keep=yes".to_string(), "drop=yes".to_string()],
        false,
        false,
    )
    .unwrap();

    // Remove one label and one param from the WORKTREE scope.
    run_update(
        &wt_dir,
        None,
        vec![],
        vec![],
        vec!["drop-label".to_string()], // --remove-label
        vec!["drop".to_string()],       // --remove-param
        false,
        false,
    )
    .unwrap();

    // Load the raw per-worktree file (not effective — we want to check mutations directly).
    let linked_paths = resolve_context_paths(&wt_dir);
    let wt_ctx = Context::load_from(&linked_paths.worktree_path().unwrap());

    assert!(
        wt_ctx.labels.contains(&"keep-label".to_string()),
        "keep-label must remain in the worktree file"
    );
    assert!(
        !wt_ctx.labels.contains(&"drop-label".to_string()),
        "drop-label must be removed from the worktree file"
    );
    assert_eq!(
        wt_ctx.params.get("keep"),
        Some(&Some("yes".to_string())),
        "keep param must remain in the worktree file"
    );
    // `--remove-param drop` now records a `None` tombstone (rather than deleting
    // the key) so the removal propagates through the layered merge.
    assert_eq!(
        wt_ctx.params.get("drop"),
        Some(&None),
        "drop param must be recorded as a tombstone in the worktree file"
    );

    // Global must be completely untouched.
    let primary_paths = resolve_context_paths(&repo_dir);
    let global_ctx = Context::load_from(&primary_paths.global_path());
    assert_eq!(
        global_ctx.labels,
        vec!["global-only"],
        "global labels must not be touched by worktree-scoped update"
    );
    assert_eq!(
        global_ctx.params.get("g"),
        Some(&Some("global".to_string())),
        "global params must not be touched by worktree-scoped update"
    );
}

// ---------------------------------------------------------------------------
// W2-T7: apply_context (hook) stamps the EFFECTIVE merged context
// ---------------------------------------------------------------------------

#[test]
fn apply_context_stamps_effective_merged_context() {
    use tracevault_cli::commands::stream::apply_context;

    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let wt_dir = tmp.path().join("wt");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);
    std::fs::create_dir_all(repo_dir.join(".tracevault")).unwrap();
    add_worktree(&repo_dir, &wt_dir);

    // Set global context from primary.
    run_set(
        &repo_dir,
        Some("global-flow".to_string()),
        vec!["global-label".to_string()],
        vec!["env=prod".to_string()],
        false,
        false,
    )
    .unwrap();

    // Set per-worktree context from linked worktree.
    run_set(
        &wt_dir,
        Some("wt-flow".to_string()),
        vec!["wt-label".to_string()],
        vec!["env=staging".to_string()],
        false,
        false,
    )
    .unwrap();

    // Simulate what the hook does: load effective context and call apply_context.
    let effective = Context::effective(&wt_dir, None);
    let (flow_id, labels, params) = apply_context(effective);

    // flow: worktree wins
    assert_eq!(
        flow_id,
        Some("wt-flow".to_string()),
        "hook must stamp effective flow"
    );

    // labels: union
    let labels = labels.expect("labels should be Some");
    assert!(
        labels.contains(&"global-label".to_string()),
        "global label must be in hook output"
    );
    assert!(
        labels.contains(&"wt-label".to_string()),
        "wt label must be in hook output"
    );

    // params: worktree override
    let params = params.expect("params should be Some");
    assert_eq!(
        params.get("env").map(String::as_str),
        Some("staging"),
        "worktree param must override global"
    );
}
