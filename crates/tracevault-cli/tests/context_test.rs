use std::collections::BTreeMap;
use tempfile::TempDir;
use tracevault_cli::commands::context::{
    find_project_root, run_clear, run_set, run_show, run_update,
};
use tracevault_cli::context::Context;

/// Create a tempdir with a pre-existing `.tracevault/` directory
/// (simulating a `tracevault init`-ed repo).
fn tmp_project() -> TempDir {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join(".tracevault")).unwrap();
    tmp
}

/// Load the context from a test project dir (reads the global context.json).
fn load_ctx(tmp: &TempDir) -> Context {
    Context::load_from(&tmp.path().join(".tracevault").join("context.json"))
}

// ---------------------------------------------------------------------------
// find_project_root
// ---------------------------------------------------------------------------

#[test]
fn find_project_root_returns_dir_with_tracevault() {
    let tmp = tmp_project();
    let root = find_project_root(tmp.path()).unwrap();
    assert_eq!(root, tmp.path());
}

#[test]
fn find_project_root_walks_up_from_subdir() {
    let tmp = tmp_project();
    let sub = tmp.path().join("deep/nested/dir");
    std::fs::create_dir_all(&sub).unwrap();
    let root = find_project_root(&sub).unwrap();
    assert_eq!(root, tmp.path());
}

#[test]
fn find_project_root_errors_without_tracevault() {
    let tmp = TempDir::new().unwrap(); // no .tracevault/
    let err = find_project_root(tmp.path()).unwrap_err();
    assert!(err.to_string().contains("tracevault init"));
}

// ---------------------------------------------------------------------------
// set — replaces everything
// ---------------------------------------------------------------------------

#[test]
fn set_creates_context_with_all_fields() {
    let tmp = tmp_project();
    run_set(
        tmp.path(),
        Some("flow-xyz".to_string()),
        vec!["alpha".to_string(), "beta".to_string()],
        vec!["k1=v1".to_string(), "k2=v2".to_string()],
        false,
        false,
    )
    .unwrap();

    let ctx = load_ctx(&tmp);
    assert_eq!(ctx.flow_id, Some("flow-xyz".to_string()));
    assert_eq!(ctx.labels, vec!["alpha", "beta"]);
    let mut expected = BTreeMap::new();
    expected.insert("k1".to_string(), Some("v1".to_string()));
    expected.insert("k2".to_string(), Some("v2".to_string()));
    assert_eq!(ctx.params, expected);
}

#[test]
fn set_replaces_previous_context_entirely() {
    let tmp = tmp_project();

    // First set with labels + params
    run_set(
        tmp.path(),
        Some("flow-A".to_string()),
        vec!["old-label".to_string()],
        vec!["old_key=old_val".to_string()],
        false,
        false,
    )
    .unwrap();

    // Second set with only a new flow — labels + params should be CLEARED
    run_set(
        tmp.path(),
        Some("flow-B".to_string()),
        vec![],
        vec![],
        false,
        false,
    )
    .unwrap();

    let ctx = load_ctx(&tmp);
    assert_eq!(ctx.flow_id, Some("flow-B".to_string()));
    assert!(ctx.labels.is_empty(), "labels should have been cleared");
    assert!(ctx.params.is_empty(), "params should have been cleared");
}

#[test]
fn set_no_flow_leaves_flow_none() {
    let tmp = tmp_project();
    run_set(
        tmp.path(),
        None,
        vec!["lbl".to_string()],
        vec![],
        false,
        false,
    )
    .unwrap();
    let ctx = load_ctx(&tmp);
    assert!(ctx.flow_id.is_none());
    assert_eq!(ctx.labels, vec!["lbl"]);
}

#[test]
fn set_comma_split_labels() {
    let tmp = tmp_project();
    run_set(
        tmp.path(),
        None,
        vec!["a,b,c".to_string()],
        vec![],
        false,
        false,
    )
    .unwrap();
    let ctx = load_ctx(&tmp);
    assert_eq!(ctx.labels, vec!["a", "b", "c"]);
}

#[test]
fn set_malformed_param_errors() {
    let tmp = tmp_project();
    let err = run_set(
        tmp.path(),
        None,
        vec![],
        vec!["noequals".to_string()],
        false,
        false,
    )
    .unwrap_err();
    assert!(err.to_string().contains("expected key=value"));
    assert!(err.to_string().contains("noequals"));
}

// ---------------------------------------------------------------------------
// update — merges changes
// ---------------------------------------------------------------------------

#[test]
fn update_changes_flow_keeps_labels() {
    let tmp = tmp_project();

    run_set(
        tmp.path(),
        Some("flow-1".to_string()),
        vec!["keep-me".to_string()],
        vec![],
        false,
        false,
    )
    .unwrap();

    run_update(
        tmp.path(),
        Some("flow-2".to_string()),
        vec![],
        vec![],
        vec![],
        vec![],
        false,
        false,
    )
    .unwrap();

    let ctx = load_ctx(&tmp);
    assert_eq!(ctx.flow_id, Some("flow-2".to_string()));
    assert_eq!(ctx.labels, vec!["keep-me"]);
}

#[test]
fn update_unions_labels() {
    let tmp = tmp_project();

    run_set(
        tmp.path(),
        None,
        vec!["a".to_string()],
        vec![],
        false,
        false,
    )
    .unwrap();
    run_update(
        tmp.path(),
        None,
        vec!["b".to_string(), "a".to_string()], // "a" is a duplicate
        vec![],
        vec![],
        vec![],
        false,
        false,
    )
    .unwrap();

    let ctx = load_ctx(&tmp);
    // "a" should appear once, "b" added
    assert_eq!(ctx.labels, vec!["a", "b"]);
}

#[test]
fn update_overwrites_param_key() {
    let tmp = tmp_project();

    run_set(
        tmp.path(),
        None,
        vec![],
        vec!["key=old".to_string()],
        false,
        false,
    )
    .unwrap();

    run_update(
        tmp.path(),
        None,
        vec![],
        vec!["key=new".to_string()],
        vec![],
        vec![],
        false,
        false,
    )
    .unwrap();

    let ctx = load_ctx(&tmp);
    assert_eq!(ctx.params["key"], Some("new".to_string()));
}

#[test]
fn update_remove_label_works() {
    let tmp = tmp_project();

    run_set(
        tmp.path(),
        None,
        vec!["keep".to_string(), "drop".to_string()],
        vec![],
        false,
        false,
    )
    .unwrap();

    run_update(
        tmp.path(),
        None,
        vec![],
        vec![],
        vec!["drop".to_string()],
        vec![],
        false,
        false,
    )
    .unwrap();

    let ctx = load_ctx(&tmp);
    assert_eq!(ctx.labels, vec!["keep"]);
}

#[test]
fn update_remove_param_works() {
    let tmp = tmp_project();

    run_set(
        tmp.path(),
        None,
        vec![],
        vec!["keep=yes".to_string(), "drop=yes".to_string()],
        false,
        false,
    )
    .unwrap();

    run_update(
        tmp.path(),
        None,
        vec![],
        vec![],
        vec![],
        vec!["drop".to_string()],
        false,
        false,
    )
    .unwrap();

    let ctx = load_ctx(&tmp);
    // `keep` retains its value; `--remove-param drop` now records a `None`
    // tombstone (rather than deleting the key) so the removal propagates
    // through the layered merge and drops any inherited lower-layer value.
    assert_eq!(ctx.params.get("keep"), Some(&Some("yes".to_string())));
    assert_eq!(ctx.params.get("drop"), Some(&None));
}

#[test]
fn update_no_flow_arg_leaves_flow_unchanged() {
    let tmp = tmp_project();

    run_set(
        tmp.path(),
        Some("stable-flow".to_string()),
        vec![],
        vec![],
        false,
        false,
    )
    .unwrap();
    run_update(
        tmp.path(),
        None,
        vec![],
        vec![],
        vec![],
        vec![],
        false,
        false,
    )
    .unwrap();

    let ctx = load_ctx(&tmp);
    assert_eq!(ctx.flow_id, Some("stable-flow".to_string()));
}

#[test]
fn update_malformed_param_errors() {
    let tmp = tmp_project();
    let err = run_update(
        tmp.path(),
        None,
        vec![],
        vec!["bad".to_string()],
        vec![],
        vec![],
        false,
        false,
    )
    .unwrap_err();
    assert!(err.to_string().contains("expected key=value"));
    assert!(err.to_string().contains("bad"));
}

// ---------------------------------------------------------------------------
// clear
// ---------------------------------------------------------------------------

#[test]
fn clear_empties_all_fields() {
    let tmp = tmp_project();

    run_set(
        tmp.path(),
        Some("flow-X".to_string()),
        vec!["lbl".to_string()],
        vec!["k=v".to_string()],
        false,
        false,
    )
    .unwrap();

    run_clear(tmp.path(), false, false).unwrap();

    let ctx = load_ctx(&tmp);
    assert_eq!(ctx, Context::default());
}

// ---------------------------------------------------------------------------
// set / update / clear — require initialised .tracevault/
// ---------------------------------------------------------------------------

#[test]
fn set_errors_without_tracevault_dir() {
    let tmp = tempfile::TempDir::new().unwrap(); // no .tracevault/
    let err = run_set(tmp.path(), None, vec![], vec![], false, false).unwrap_err();
    assert!(
        err.to_string().contains("tracevault init"),
        "error must mention 'tracevault init', got: {err}"
    );
}

#[test]
fn update_errors_without_tracevault_dir() {
    let tmp = tempfile::TempDir::new().unwrap(); // no .tracevault/
    let err = run_update(
        tmp.path(),
        None,
        vec![],
        vec![],
        vec![],
        vec![],
        false,
        false,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("tracevault init"),
        "error must mention 'tracevault init', got: {err}"
    );
}

#[test]
fn clear_errors_without_tracevault_dir() {
    let tmp = tempfile::TempDir::new().unwrap(); // no .tracevault/
    let err = run_clear(tmp.path(), false, false).unwrap_err();
    assert!(
        err.to_string().contains("tracevault init"),
        "error must mention 'tracevault init', got: {err}"
    );
}

// ---------------------------------------------------------------------------
// show — smoke test (just check it doesn't error)
// ---------------------------------------------------------------------------

#[test]
fn show_does_not_error_on_empty_context() {
    let tmp = tmp_project();
    run_show(tmp.path()).unwrap();
}

#[test]
fn show_does_not_error_with_populated_context() {
    let tmp = tmp_project();
    run_set(
        tmp.path(),
        Some("show-flow".to_string()),
        vec!["tag1".to_string()],
        vec!["env=prod".to_string()],
        false,
        false,
    )
    .unwrap();
    run_show(tmp.path()).unwrap();
}
