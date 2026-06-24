/// Multi-worktree capture end-to-end test (PR-A proof, Phase-1, Task 6).
///
/// Proves the headline Phase-1 guarantee: TWO Claude Code instances in TWO
/// separate sibling worktrees of one repo both capture correctly under the
/// primary `.tracevault/`, with no silent drops and correct per-worktree
/// session attribution.
///
/// # What is exercised
///
/// - `paths::resolve_project_root` — git-aware resolver (git-common-dir path)
/// - `commands::stream::resolve_session_paths` — maps (cwd, session_id) →
///   (project_root, session_dir) using the resolver above
/// - Origin-marker write logic (simulated; production path in `run_stream`)
/// - `find_sessions_by_origin`-equivalent logic (replicated locally — the
///   production function is private, so we assert at the layer above it)
///
/// # Assertions (mirroring the task spec)
///
/// 1. From each sibling worktree, `resolve_project_root` → primary root;
///    `resolve_session_paths` → session_dir under `<primary>/.tracevault/sessions/<sid>/`.
/// 2. Both session dirs live under the SAME primary `.tracevault/sessions/`,
///    with DISTINCT session IDs → no collision.
/// 3. Each session's `origin` marker = its own worktree toplevel (canonicalized).
/// 4. verify-start disambiguation: from worktree A, find_sessions_by_origin
///    returns session A only; from B, only B.  (The private production function
///    is replicated here — see reachability note below.)
/// 5. A non-git / uninitialized dir degrades gracefully (Fallback, no panic).
///
/// # Reachability note
///
/// `commands::verification_phase::find_sessions_by_origin` is private and
/// `open_verification_phase` is public but performs a live network request,
/// making it unsuitable for an offline unit test.  We therefore:
///   a) Use the production `paths::worktree_toplevel` helper directly (now
///      shared), and replicate only the trivial `find_sessions_by_origin`
///      one-liner inline.
///   b) Assert that the selection logic picks the correct session — which is
///      the real assertion we need; the production code paths are exercised by
///      `verification_phase` module's own `#[cfg(test)]` tests.
use std::fs;
use std::path::Path;
use std::process::Command;

use tracevault_cli::commands::stream::resolve_session_paths;
use tracevault_cli::paths::{resolve_project_root, worktree_toplevel, ProjectRootSource};

// ---------------------------------------------------------------------------
// Shared fixtures (mirrors crate::test_helpers which is pub(crate))
// ---------------------------------------------------------------------------

fn init_git_repo(dir: &Path) {
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

fn add_worktree(repo_dir: &Path, wt_dir: &Path) {
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

/// Return session dirs whose `origin` marker matches `worktree_top` (mirrors
/// production `find_sessions_by_origin`; that function is private in
/// `commands::verification_phase`).
fn sessions_matching_origin(sessions_dir: &Path, worktree_top: &str) -> Vec<std::path::PathBuf> {
    let Ok(entries) = fs::read_dir(sessions_dir) else {
        return vec![];
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter(|e| {
            fs::read_to_string(e.path().join("origin"))
                .map(|s| s.trim() == worktree_top)
                .unwrap_or(false)
        })
        .map(|e| e.path())
        .collect()
}

// ---------------------------------------------------------------------------
// 1 + 2: resolve_project_root + resolve_session_paths from sibling worktrees
// ---------------------------------------------------------------------------

/// Assertion 1 & 2: from EACH sibling worktree, resolve_project_root returns
/// the primary root; resolve_session_paths places session dirs under that same
/// primary `.tracevault/sessions/`.  Session IDs are distinct → no collision.
#[test]
fn sibling_worktrees_resolve_to_primary_and_no_collision() {
    let tmp = tempfile::tempdir().unwrap();
    let primary = tmp.path().join("repo");
    let wt_a = tmp.path().join("wt-a");
    let wt_b = tmp.path().join("wt-b");

    fs::create_dir_all(&primary).unwrap();
    init_git_repo(&primary);
    // .tracevault/ exists ONLY in the primary — the key condition that the old
    // ancestor-walk broke on from a sibling worktree.
    fs::create_dir_all(primary.join(".tracevault")).unwrap();

    add_worktree(&primary, &wt_a);
    add_worktree(&primary, &wt_b);

    let sid_a = "sid-worktree-a-001";
    let sid_b = "sid-worktree-b-002";

    // --- Assertion 1a: resolve_project_root from wt_a → primary ---
    let root_a = resolve_project_root(&wt_a);
    assert_eq!(
        root_a.source,
        ProjectRootSource::Git,
        "wt_a: must resolve via git (not ancestor-walk)"
    );
    assert_eq!(
        root_a.root.canonicalize().unwrap(),
        primary.canonicalize().unwrap(),
        "wt_a: resolve_project_root must return primary root, not wt_a"
    );

    // --- Assertion 1b: resolve_project_root from wt_b → primary ---
    let root_b = resolve_project_root(&wt_b);
    assert_eq!(
        root_b.source,
        ProjectRootSource::Git,
        "wt_b: must resolve via git"
    );
    assert_eq!(
        root_b.root.canonicalize().unwrap(),
        primary.canonicalize().unwrap(),
        "wt_b: resolve_project_root must return primary root, not wt_b"
    );

    // --- Assertion 1c: resolve_session_paths → session_dir under primary ---
    let (proj_root_a, session_dir_a) = resolve_session_paths(&wt_a, sid_a);
    let (proj_root_b, session_dir_b) = resolve_session_paths(&wt_b, sid_b);

    assert_eq!(
        proj_root_a.root.canonicalize().unwrap(),
        primary.canonicalize().unwrap(),
        "wt_a: project_root from resolve_session_paths must be primary"
    );
    assert_eq!(
        proj_root_b.root.canonicalize().unwrap(),
        primary.canonicalize().unwrap(),
        "wt_b: project_root from resolve_session_paths must be primary"
    );

    // Session dirs must be under the primary .tracevault/sessions/
    let expected_sessions = primary.join(".tracevault").join("sessions");
    assert!(
        session_dir_a.starts_with(&primary),
        "wt_a: session_dir must be under primary"
    );
    assert!(
        session_dir_a.starts_with(&expected_sessions),
        "wt_a: session_dir must be under .tracevault/sessions/"
    );
    assert!(
        !session_dir_a.starts_with(&wt_a),
        "wt_a: session_dir must NOT be inside the sibling worktree dir"
    );

    assert!(
        session_dir_b.starts_with(&primary),
        "wt_b: session_dir must be under primary"
    );
    assert!(
        session_dir_b.starts_with(&expected_sessions),
        "wt_b: session_dir must be under .tracevault/sessions/"
    );
    assert!(
        !session_dir_b.starts_with(&wt_b),
        "wt_b: session_dir must NOT be inside the sibling worktree dir"
    );

    // --- Assertion 2: distinct session IDs → different dirs, no collision ---
    assert_ne!(
        session_dir_a, session_dir_b,
        "session dirs for distinct IDs must not collide"
    );
    assert!(
        session_dir_a.ends_with(
            std::path::Path::new(".tracevault")
                .join("sessions")
                .join(sid_a)
        ),
        "wt_a: session_dir must end with .tracevault/sessions/{sid_a}"
    );
    assert!(
        session_dir_b.ends_with(
            std::path::Path::new(".tracevault")
                .join("sessions")
                .join(sid_b)
        ),
        "wt_b: session_dir must end with .tracevault/sessions/{sid_b}"
    );
}

// ---------------------------------------------------------------------------
// 3: Origin markers contain each session's own worktree toplevel
// ---------------------------------------------------------------------------

/// Assertion 3: each session's `origin` marker = that session's worktree
/// toplevel (A's marker = A's toplevel; B's marker = B's toplevel).
#[test]
fn origin_markers_contain_correct_worktree_toplevel() {
    let tmp = tempfile::tempdir().unwrap();
    let primary = tmp.path().join("repo");
    let wt_a = tmp.path().join("wt-a");
    let wt_b = tmp.path().join("wt-b");

    fs::create_dir_all(&primary).unwrap();
    init_git_repo(&primary);
    add_worktree(&primary, &wt_a);
    add_worktree(&primary, &wt_b);

    let sid_a = "sid-origin-a";
    let sid_b = "sid-origin-b";

    // Simulate the origin-marker write logic from run_stream (the production
    // path; we can't call run_stream without a live server).
    let (_, session_dir_a) = resolve_session_paths(&wt_a, sid_a);
    let (_, session_dir_b) = resolve_session_paths(&wt_b, sid_b);
    fs::create_dir_all(&session_dir_a).unwrap();
    fs::create_dir_all(&session_dir_b).unwrap();

    let origin_a = worktree_toplevel(&wt_a);
    let origin_b = worktree_toplevel(&wt_b);
    fs::write(session_dir_a.join("origin"), &origin_a).unwrap();
    fs::write(session_dir_b.join("origin"), &origin_b).unwrap();

    // Each marker must equal the respective worktree's canonical toplevel.
    let canonical_a = wt_a.canonicalize().unwrap().to_string_lossy().into_owned();
    let canonical_b = wt_b.canonicalize().unwrap().to_string_lossy().into_owned();

    let marker_a = fs::read_to_string(session_dir_a.join("origin")).unwrap();
    let marker_b = fs::read_to_string(session_dir_b.join("origin")).unwrap();

    assert_eq!(
        marker_a.trim(),
        canonical_a.as_str(),
        "session A's origin marker must be wt_a's canonical toplevel"
    );
    assert_eq!(
        marker_b.trim(),
        canonical_b.as_str(),
        "session B's origin marker must be wt_b's canonical toplevel"
    );

    // Cross-check: A's marker must NOT equal B's toplevel and vice-versa.
    assert_ne!(
        marker_a.trim(),
        canonical_b.as_str(),
        "session A's origin marker must differ from wt_b's toplevel"
    );
    assert_ne!(
        marker_b.trim(),
        canonical_a.as_str(),
        "session B's origin marker must differ from wt_a's toplevel"
    );
}

// ---------------------------------------------------------------------------
// 4: verify-start disambiguation selects the correct session per worktree
// ---------------------------------------------------------------------------

/// Assertion 4: the disambiguation logic (replicated from production
/// `find_sessions_by_origin`) selects session A from wt_a and session B from
/// wt_b, with no cross-contamination.
#[test]
fn verify_start_disambiguation_selects_correct_worktree_session() {
    let tmp = tempfile::tempdir().unwrap();
    let primary = tmp.path().join("repo");
    let wt_a = tmp.path().join("wt-a");
    let wt_b = tmp.path().join("wt-b");

    fs::create_dir_all(&primary).unwrap();
    init_git_repo(&primary);
    add_worktree(&primary, &wt_a);
    add_worktree(&primary, &wt_b);

    let sid_a = "sid-disambig-a";
    let sid_b = "sid-disambig-b";

    // Create both session dirs under the primary .tracevault/sessions/
    let (_, session_dir_a) = resolve_session_paths(&wt_a, sid_a);
    let (_, session_dir_b) = resolve_session_paths(&wt_b, sid_b);
    fs::create_dir_all(&session_dir_a).unwrap();
    fs::create_dir_all(&session_dir_b).unwrap();

    // Stamp origin markers (as run_stream does)
    let origin_a = worktree_toplevel(&wt_a);
    let origin_b = worktree_toplevel(&wt_b);
    fs::write(session_dir_a.join("origin"), &origin_a).unwrap();
    fs::write(session_dir_b.join("origin"), &origin_b).unwrap();

    let sessions_dir = primary.join(".tracevault").join("sessions");

    // From wt_a: disambiguation must select sid_a only
    let wt_a_top = worktree_toplevel(&wt_a);
    let matched_from_a = sessions_matching_origin(&sessions_dir, &wt_a_top);
    assert_eq!(
        matched_from_a.len(),
        1,
        "disambiguation from wt_a must match exactly 1 session"
    );
    let selected_id_a = matched_from_a[0]
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap();
    assert_eq!(
        selected_id_a, sid_a,
        "disambiguation from wt_a must select sid_a, not sid_b"
    );

    // From wt_b: disambiguation must select sid_b only
    let wt_b_top = worktree_toplevel(&wt_b);
    let matched_from_b = sessions_matching_origin(&sessions_dir, &wt_b_top);
    assert_eq!(
        matched_from_b.len(),
        1,
        "disambiguation from wt_b must match exactly 1 session"
    );
    let selected_id_b = matched_from_b[0]
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap();
    assert_eq!(
        selected_id_b, sid_b,
        "disambiguation from wt_b must select sid_b, not sid_a"
    );
}

// ---------------------------------------------------------------------------
// 5: non-git / uninitialized dirs degrade gracefully
// ---------------------------------------------------------------------------

/// Assertion 5a: non-git dir with no `.tracevault/` → Fallback, no panic.
/// `resolve_session_paths` returns session_dir relative to start — the caller
/// (run_stream) will fail to load config and exit gracefully without blocking
/// the tool.
#[test]
fn non_git_dir_degrades_gracefully_no_panic() {
    let tmp = tempfile::tempdir().unwrap();
    // No git, no .tracevault/ — must not panic.
    let result = resolve_project_root(tmp.path());
    assert_eq!(
        result.source,
        ProjectRootSource::Fallback,
        "non-git dir must use Fallback resolution"
    );
    // resolve_session_paths must also not panic.
    let (proj_root, session_dir) = resolve_session_paths(tmp.path(), "sess-non-git");
    assert_eq!(
        proj_root.root,
        tmp.path(),
        "non-git: project_root must be start dir"
    );
    assert!(
        session_dir.starts_with(tmp.path()),
        "non-git: session_dir must be under start dir"
    );
}

/// Assertion 5b: a dir that has a `.tracevault/` ancestor but is not a git
/// repo → AncestorWalk source, no panic.
#[test]
fn non_git_with_tracevault_ancestor_resolves_no_panic() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join(".tracevault")).unwrap();
    let subdir = tmp.path().join("nested").join("dir");
    fs::create_dir_all(&subdir).unwrap();

    let result = resolve_project_root(&subdir);
    assert_eq!(
        result.source,
        ProjectRootSource::AncestorWalk,
        "non-git with .tracevault/ ancestor must use AncestorWalk"
    );
    assert_eq!(
        result.root.canonicalize().unwrap(),
        tmp.path().canonicalize().unwrap(),
        "AncestorWalk root must be the dir containing .tracevault/"
    );
    // resolve_session_paths must not panic.
    let (_root, _session_dir) = resolve_session_paths(&subdir, "sess-ancestor");
}

// ---------------------------------------------------------------------------
// Combined: full two-worktree scenario end-to-end
// ---------------------------------------------------------------------------

/// Full e2e scenario: primary repo + two sibling worktrees + distinct sessions.
/// Exercises all five assertions in a single scenario to confirm no
/// interaction or cross-contamination between sessions.
#[test]
fn full_two_worktree_capture_scenario() {
    let tmp = tempfile::tempdir().unwrap();
    let primary = tmp.path().join("primary-repo");
    let wt_a = tmp.path().join("worktree-alpha");
    let wt_b = tmp.path().join("worktree-beta");

    fs::create_dir_all(&primary).unwrap();
    init_git_repo(&primary);
    // .tracevault/ ONLY in primary — the scenario that broke with the old
    // ancestor-walk resolver.
    fs::create_dir_all(primary.join(".tracevault")).unwrap();
    add_worktree(&primary, &wt_a);
    add_worktree(&primary, &wt_b);

    let sid_a = "e2e-alpha-session-001";
    let sid_b = "e2e-beta-session-002";

    // --- Step 1 & 2: resolve paths from each worktree ---
    let (root_a, sess_a) = resolve_session_paths(&wt_a, sid_a);
    let (root_b, sess_b) = resolve_session_paths(&wt_b, sid_b);

    let primary_canonical = primary.canonicalize().unwrap();

    assert_eq!(
        root_a.root.canonicalize().unwrap(),
        primary_canonical,
        "alpha: project_root must be primary"
    );
    assert_eq!(
        root_b.root.canonicalize().unwrap(),
        primary_canonical,
        "beta: project_root must be primary"
    );
    // Both session dirs are under the PRIMARY .tracevault/sessions/
    let sessions_dir = primary.join(".tracevault").join("sessions");
    assert!(sess_a.starts_with(&primary), "alpha sess under primary");
    assert!(sess_b.starts_with(&primary), "beta sess under primary");
    assert_ne!(sess_a, sess_b, "session dirs must be distinct");

    // --- Step 3: Write origin markers ---
    fs::create_dir_all(&sess_a).unwrap();
    fs::create_dir_all(&sess_b).unwrap();
    let top_a = worktree_toplevel(&wt_a);
    let top_b = worktree_toplevel(&wt_b);
    fs::write(sess_a.join("origin"), &top_a).unwrap();
    fs::write(sess_b.join("origin"), &top_b).unwrap();

    // Marker A == wt_a canonical; marker B == wt_b canonical
    let canonical_a = wt_a.canonicalize().unwrap().to_string_lossy().into_owned();
    let canonical_b = wt_b.canonicalize().unwrap().to_string_lossy().into_owned();
    assert_eq!(
        fs::read_to_string(sess_a.join("origin")).unwrap().trim(),
        canonical_a.as_str()
    );
    assert_eq!(
        fs::read_to_string(sess_b.join("origin")).unwrap().trim(),
        canonical_b.as_str()
    );

    // --- Step 4: Disambiguation ---
    let matched_a = sessions_matching_origin(&sessions_dir, &canonical_a);
    let matched_b = sessions_matching_origin(&sessions_dir, &canonical_b);

    assert_eq!(matched_a.len(), 1, "alpha: exactly one session matches");
    assert_eq!(matched_b.len(), 1, "beta: exactly one session matches");

    let sel_a = matched_a[0].file_name().and_then(|n| n.to_str()).unwrap();
    let sel_b = matched_b[0].file_name().and_then(|n| n.to_str()).unwrap();

    assert_eq!(sel_a, sid_a, "alpha worktree selects alpha session");
    assert_eq!(sel_b, sid_b, "beta worktree selects beta session");

    // --- Step 5: Third session from a non-git dir doesn't panic ---
    let non_git = tempfile::tempdir().unwrap();
    let fallback = resolve_project_root(non_git.path());
    assert_eq!(fallback.source, ProjectRootSource::Fallback);
    let (_, _) = resolve_session_paths(non_git.path(), "sess-non-git-e2e");
}
