use crate::config::{TracevaultConfig, UserContext};
use crate::context::{resolve_context_paths, Context, WorktreeScope};
use std::io;
use std::path::{Path, PathBuf};

/// Walk up from `start` to find the nearest ancestor that contains a
/// `.tracevault/` directory. Returns that ancestor. Errors if none found.
///
/// Kept as a public utility for integration tests and callers that need the
/// classic "must have .tracevault/" guarantee (e.g. legacy hooks, tests).
/// Worktree-aware commands use `resolve_context_paths` directly instead.
pub fn find_project_root(start: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    for ancestor in start.ancestors() {
        if ancestor.join(".tracevault").is_dir() {
            return Ok(ancestor.to_path_buf());
        }
    }
    Err(format!(
        "No .tracevault/ directory found in '{}' or any parent directory. \
         Run `tracevault init` first.",
        start.display()
    )
    .into())
}

/// Parse a `--param` value of the form `key=value`.
/// Rejects values with no `=`.
fn parse_param(raw: &str) -> Result<(String, String), Box<dyn std::error::Error>> {
    let pos = raw
        .find('=')
        .ok_or_else(|| format!("invalid --param '{raw}': expected key=value"))?;
    let key = raw[..pos].to_string();
    let value = raw[pos + 1..].to_string();
    Ok((key, value))
}

/// Split a label argument on commas, trimming whitespace.
fn split_labels(labels: &[String]) -> Vec<String> {
    labels
        .iter()
        .flat_map(|l| l.split(','))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Resolve the file backing the project's user-level context source
/// (`user_context` in `config.toml`), regardless of whether it is enabled.
///
/// Used by `--user` on `context set/update/clear` to edit that file directly.
fn user_context_path(cwd: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    user_context_path_in(cwd, &crate::config::tv_config_root())
}

/// Resolve the file backing the user-level context for `--user` edits.
/// Precedence: a repo `config.toml` that *configured* `user_context` wins;
/// otherwise the user-level `config.toml`; otherwise the default path. A
/// malformed repo OR user-level config is an error (never silently misroute a
/// `--user` write). Works with NO checkout.
fn user_context_path_in(
    cwd: &Path,
    config_root: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let repo_uc = match find_project_root(cwd) {
        Ok(root) => TracevaultConfig::try_load(&root)
            .map_err(|e| {
                format!("cannot resolve --user path: malformed .tracevault/config.toml: {e}")
            })?
            .and_then(|c| c.user_context),
        Err(_) => None, // no checkout → fall through to the user-level config
    };
    if let Some(uc) = repo_uc {
        // A bare disable expresses no opinion about WHERE the user-context file
        // lives — fall through to the user-level config so `--user` edits target
        // the file the hook actually reads elsewhere. A repo that set an explicit
        // path (even while disabled) still points the edit at that path.
        let bare_disable = matches!(uc, UserContext::Toggle(false))
            || matches!(
                uc,
                UserContext::Full {
                    enable: false,
                    path: None
                }
            );
        if !bare_disable {
            return Ok(uc.path());
        }
    }
    let user_cfg = crate::config::try_load_user_config_in(config_root)
        .map_err(|e| format!("cannot resolve --user path: malformed user config.toml: {e}"))?;
    Ok(user_cfg
        .and_then(|c| c.user_context)
        .map(|uc| uc.path_in(config_root))
        .unwrap_or_else(|| crate::config::default_user_context_path_in(config_root)))
}

/// `tracevault context set` — build a fresh Context from flags, save it.
/// Omitted dimensions are empty. Clears anything not explicitly provided.
///
/// Default scope:
/// - Linked worktree → writes `worktrees/<key>/context.json`.
/// - Primary checkout → writes the global `context.json`.
///
/// `global = true` forces the global file from any worktree.
/// `user = true` writes the resolved user-context file instead (wins over `global`).
pub fn run_set(
    cwd: &Path,
    flow: Option<String>,
    labels: Vec<String>,
    params: Vec<String>,
    global: bool,
    user: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut parsed_params = std::collections::BTreeMap::new();
    for raw in &params {
        let (k, v) = parse_param(raw)?;
        parsed_params.insert(k, Some(v));
    }

    let ctx = Context {
        flow_id: flow,
        labels: split_labels(&labels),
        params: parsed_params,
    };

    if user {
        let path = user_context_path(cwd)?;
        ctx.save_to(&path)
            .map_err(|e: io::Error| format!("failed to save context: {e}"))?;
        println!("Context set.");
        return Ok(());
    }

    let paths = resolve_context_paths(cwd);

    // Require an initialised .tracevault/ before mutating.
    if !paths.tracevault_dir.exists() {
        return Err("no .tracevault/ found — run 'tracevault init' first".into());
    }

    if global || matches!(paths.scope, WorktreeScope::Primary) {
        ctx.save_global(&paths)
    } else {
        ctx.save_worktree(&paths)
    }
    .map_err(|e: io::Error| format!("failed to save context: {e}"))?;

    println!("Context set.");
    Ok(())
}

/// `tracevault context update` — load existing, apply mutations, save.
///
/// Default scope:
/// - Linked worktree → reads/writes `worktrees/<key>/context.json`.
/// - Primary checkout → reads/writes the global `context.json`.
///
/// `global = true` forces the global file from any worktree.
/// `user = true` reads/writes the resolved user-context file instead (wins over `global`).
#[allow(clippy::too_many_arguments)]
pub fn run_update(
    cwd: &Path,
    flow: Option<String>,
    labels: Vec<String>,
    params: Vec<String>,
    remove_labels: Vec<String>,
    remove_params: Vec<String>,
    global: bool,
    user: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if user {
        let path = user_context_path(cwd)?;
        let mut ctx = Context::load_from(&path);
        apply_update_mutations(
            &mut ctx,
            flow,
            &labels,
            &params,
            &remove_labels,
            &remove_params,
        )?;
        ctx.save_to(&path)
            .map_err(|e: io::Error| format!("failed to save context: {e}"))?;
        println!("Context updated.");
        return Ok(());
    }

    let paths = resolve_context_paths(cwd);

    // Require an initialised .tracevault/ before mutating.
    if !paths.tracevault_dir.exists() {
        return Err("no .tracevault/ found — run 'tracevault init' first".into());
    }

    let use_global = global || matches!(paths.scope, WorktreeScope::Primary);

    let mut ctx = if use_global {
        Context::load_global(&paths)
    } else {
        Context::load_worktree(&paths).unwrap_or_default()
    };

    apply_update_mutations(
        &mut ctx,
        flow,
        &labels,
        &params,
        &remove_labels,
        &remove_params,
    )?;

    if use_global {
        ctx.save_global(&paths)
    } else {
        ctx.save_worktree(&paths)
    }
    .map_err(|e: io::Error| format!("failed to save context: {e}"))?;

    println!("Context updated.");
    Ok(())
}

/// Apply the `update` flow's mutations (set flow, union labels, upsert params,
/// remove labels/params) to an already-loaded `ctx`. Shared by the
/// repo/worktree/global path and the `--user` path so both branches stay in sync.
fn apply_update_mutations(
    ctx: &mut Context,
    flow: Option<String>,
    labels: &[String],
    params: &[String],
    remove_labels: &[String],
    remove_params: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    // Set flow if provided
    if let Some(f) = flow {
        ctx.flow_id = Some(f);
    }

    // Union-add labels (dedup guarded inline by the contains check below; save also normalises)
    let new_labels = split_labels(labels);
    for l in new_labels {
        if !ctx.labels.contains(&l) {
            ctx.labels.push(l);
        }
    }

    // Insert/overwrite params
    for raw in params {
        let (k, v) = parse_param(raw)?;
        ctx.params.insert(k, Some(v));
    }

    // Remove labels
    let remove_set: std::collections::HashSet<String> =
        split_labels(remove_labels).into_iter().collect();
    ctx.labels.retain(|l| !remove_set.contains(l));

    // Remove params: insert a `None` tombstone so the removal propagates through
    // the merge and drops the key even when a lower-precedence layer sets it.
    for k in remove_params {
        ctx.params.insert(k.clone(), None);
    }

    Ok(())
}

/// `tracevault context show` — print four labelled sections:
///   1. User — the cross-repo user-level context (its contents are shown only
///      when `user_context` is enabled in `config.toml`; the header shows the
///      resolved file path when enabled and `disabled` otherwise).
///   2. Global — the repo-wide `context.json`.
///   3. This worktree — the per-worktree context (Linked only; "(none)" when missing).
///   4. Effective — the merged result that the hook stamps on every event, with
///      each label/param annotated with the layer it came from.
///
/// Prints a hint if no `.tracevault/` directory is found (i.e. `tracevault init` has
/// not been run), using `find_project_root` for the diagnostic.
pub fn run_show(cwd: &Path) -> Result<(), Box<dyn std::error::Error>> {
    run_show_in(cwd, &crate::config::tv_config_root())
}

/// [`run_show`], but resolving the user-level fallback against an injectable
/// `config_root` instead of the process-global [`crate::config::tv_config_root`].
/// Lets tests exercise the detached (no-checkout) path without touching the
/// real `~/.config`.
pub fn run_show_in(cwd: &Path, config_root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // Resolve the user-level context source: a repo config that configured it
    // wins; otherwise fall back to the user-level config.toml (Task 2/3).
    // Tolerate no project root/config existing at all, same as `user_context_path`.
    let repo_uc = find_project_root(cwd)
        .ok()
        .and_then(|root| TracevaultConfig::load(&root))
        .and_then(|c| c.user_context);
    let user_path =
        crate::config::resolve_user_context_in(repo_uc, config_root).resolve_in(config_root);
    let user_enabled = user_path.is_some();

    let (paths, user, global, worktree, effective) =
        Context::effective_with_parts(cwd, user_path.as_deref());

    // If the tracevault directory doesn't exist yet, hint that init is needed.
    // find_project_root gives the classic walk-up path; if it also fails, the
    // user has not run `tracevault init` yet.
    if !paths.tracevault_dir.exists() && find_project_root(cwd).is_err() {
        eprintln!("hint: No .tracevault/ directory found. Run `tracevault init` first.");
    }

    let is_linked = matches!(paths.scope, WorktreeScope::Linked { .. });

    print!(
        "{}",
        show_report(
            user.as_ref(),
            user_enabled,
            user_path.as_deref(),
            &global,
            worktree.as_ref(),
            is_linked,
            &effective,
        )
    );

    Ok(())
}

/// Layer names used in provenance annotations, precedence low→high.
const LAYER_USER: &str = "user";
const LAYER_REPO: &str = "repo";
const LAYER_WORKTREE: &str = "worktree";

/// Highest-precedence layer (`worktree` > `repo` > `user`) whose stored
/// `params` map has `key` set to `Some(_)`. `None` tombstones are skipped:
/// per `Context::merge_layers`, a key only survives into the effective
/// params at all if the highest layer that *touches* it sets `Some`, so the
/// highest `Some` always identifies the correct source.
fn param_source(
    user: Option<&Context>,
    global: &Context,
    worktree: Option<&Context>,
    key: &str,
) -> &'static str {
    if let Some(w) = worktree {
        if matches!(w.params.get(key), Some(Some(_))) {
            return LAYER_WORKTREE;
        }
    }
    if matches!(global.params.get(key), Some(Some(_))) {
        return LAYER_REPO;
    }
    if let Some(u) = user {
        if matches!(u.params.get(key), Some(Some(_))) {
            return LAYER_USER;
        }
    }
    "?"
}

/// First (lowest-precedence) layer whose `labels` contains `label` — labels
/// are a union, so the *introducing* layer (not the overriding one) is the
/// meaningful provenance.
fn label_source(
    user: Option<&Context>,
    global: &Context,
    worktree: Option<&Context>,
    label: &str,
) -> &'static str {
    if let Some(u) = user {
        if u.labels.iter().any(|l| l == label) {
            return LAYER_USER;
        }
    }
    if global.labels.iter().any(|l| l == label) {
        return LAYER_REPO;
    }
    if let Some(w) = worktree {
        if w.labels.iter().any(|l| l == label) {
            return LAYER_WORKTREE;
        }
    }
    "?"
}

/// Highest-precedence layer whose `flow_id` is `Some(_)` — same "highest
/// wins" rule `merge_layers` uses for `flow_id`.
fn flow_source(
    user: Option<&Context>,
    global: &Context,
    worktree: Option<&Context>,
) -> &'static str {
    if let Some(w) = worktree {
        if w.flow_id.is_some() {
            return LAYER_WORKTREE;
        }
    }
    if global.flow_id.is_some() {
        return LAYER_REPO;
    }
    if let Some(u) = user {
        if u.flow_id.is_some() {
            return LAYER_USER;
        }
    }
    "?"
}

/// Pure formatter for `tracevault context show`. Extracted out of `run_show`
/// so the provenance logic is unit-testable without capturing stdout.
///
/// `user_enabled`/`user_path` carry the `user_context` config state needed
/// for the `User` header (a `Context`/`EffectiveContext` alone can't tell
/// "disabled" apart from "enabled but file missing"). `is_linked` likewise
/// carries `ContextPaths::scope`: a bare `worktree: Option<&Context>` can't
/// distinguish "primary checkout, no per-worktree section at all" from
/// "linked worktree with no per-worktree file yet ('(none)')".
#[allow(clippy::too_many_arguments)]
fn show_report(
    user: Option<&Context>,
    user_enabled: bool,
    user_path: Option<&Path>,
    global: &Context,
    worktree: Option<&Context>,
    is_linked: bool,
    eff: &crate::context::EffectiveContext,
) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    let file_desc = match user_path {
        Some(p) if user_enabled => p.display().to_string(),
        _ => "disabled".to_string(),
    };
    let _ = writeln!(
        out,
        "=== User (enabled: {user_enabled}, file: {file_desc}) ==="
    );
    match user {
        Some(u) => {
            let _ = writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(u).unwrap_or_default()
            );
        }
        None => {
            let _ = writeln!(out, "(none)");
        }
    }

    let _ = writeln!(out, "\n=== Global ===");
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(global).unwrap_or_default()
    );

    if is_linked {
        let _ = writeln!(out, "\n=== This worktree ===");
        match worktree {
            Some(wt) => {
                let _ = writeln!(
                    out,
                    "{}",
                    serde_json::to_string_pretty(wt).unwrap_or_default()
                );
            }
            None => {
                let _ = writeln!(out, "(none)");
            }
        }
    }

    let _ = writeln!(out, "\n=== Effective ===");
    match &eff.flow_id {
        Some(f) => {
            let source = flow_source(user, global, worktree);
            let _ = writeln!(out, "flow_id = {f}   [from {source}]");
        }
        None => {
            let _ = writeln!(out, "flow_id = (none)");
        }
    }

    let _ = writeln!(out, "labels:");
    if eff.labels.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        for label in &eff.labels {
            let source = label_source(user, global, worktree, label);
            let _ = writeln!(out, "  {label}   [from {source}]");
        }
    }

    let _ = writeln!(out, "params:");
    if eff.params.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        for (k, v) in &eff.params {
            let source = param_source(user, global, worktree, k);
            let _ = writeln!(out, "  {k} = {v}   [from {source}]");
        }
    }

    out
}

/// `tracevault context clear` — remove/reset the context for the chosen scope.
///
/// Default scope:
/// - Linked worktree → DELETE `worktrees/<key>/context.json` if it exists.
///   A missing file means "no per-worktree context" (`load_worktree` returns
///   `None`, `show` prints "(none)"), so deleting preserves the absent-vs-empty
///   distinction that the rest of the design relies on.
/// - Primary checkout → write an empty (default) `context.json`.
///   The global file is the canonical one; there is no absent-vs-empty
///   distinction to preserve, and `show`/`effective` always read it.
///
/// `global = true` forces clearing the global file from any worktree.
/// `user = true` clears the resolved user-context file instead (wins over `global`);
/// like the global file, the user file is canonical, so it is overwritten with an
/// empty context rather than deleted.
pub fn run_clear(cwd: &Path, global: bool, user: bool) -> Result<(), Box<dyn std::error::Error>> {
    if user {
        let path = user_context_path(cwd)?;
        Context::default()
            .save_to(&path)
            .map_err(|e: io::Error| format!("failed to clear context: {e}"))?;
        println!("Context cleared.");
        return Ok(());
    }

    let paths = resolve_context_paths(cwd);

    // Require an initialised .tracevault/ before mutating.
    if !paths.tracevault_dir.exists() {
        return Err("no .tracevault/ found — run 'tracevault init' first".into());
    }

    let use_global = global || matches!(paths.scope, WorktreeScope::Primary);

    if use_global {
        // Global scope: overwrite with an empty context so `show`/`effective`
        // continue to find a well-formed file.
        Context::default()
            .save_global(&paths)
            .map_err(|e: io::Error| format!("failed to clear context: {e}"))?;
    } else {
        // Per-worktree scope: delete the file so that `load_worktree` returns
        // `None` and `show` prints "(none)" — preserving the absent-vs-empty
        // distinction.
        if let Some(wt_path) = paths.worktree_path() {
            match std::fs::remove_file(&wt_path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {} // already absent — fine
                Err(e) => return Err(format!("failed to clear context: {e}").into()),
            }
        }
    }

    println!("Context cleared.");
    Ok(())
}

/// `tracevault context source` — enable/disable or point the project's
/// user-level context source (`user_context` in `config.toml`).
///
/// The mode flags (`--enable`, `--disable`, `--path <p>`, `--default`) are
/// mutually exclusive — enforced at the CLI layer — so exactly one mode is set:
/// `--path` points at an explicit file; `--enable`/`--default` both mean
/// "enabled at the default path"; `--disable` turns it off. If none are given,
/// errors rather than silently no-op'ing.
pub fn run_source(
    cwd: &Path,
    enable: bool,
    disable: bool,
    path: Option<String>,
    default: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = find_project_root(cwd)?;
    // Editing `user_context`, so distinguish a missing config from a malformed
    // one (same as `run_stream` / `user_context_path`) rather than reporting a
    // malformed file as "not found".
    let mut config = match TracevaultConfig::try_load(&root) {
        Ok(Some(cfg)) => cfg,
        Ok(None) => {
            return Err("no .tracevault/config.toml found — run 'tracevault init' first".into())
        }
        Err(e) => return Err(format!("malformed .tracevault/config.toml: {e}").into()),
    };

    let new_user_context = if disable {
        UserContext::Toggle(false)
    } else if let Some(p) = path {
        UserContext::Path(p)
    } else if default || enable {
        UserContext::Toggle(true)
    } else {
        return Err("specify one of --enable, --disable, --path <file>, --default".into());
    };
    config.user_context = Some(new_user_context);

    std::fs::write(TracevaultConfig::config_path(&root), config.to_toml())?;
    println!("Updated user_context.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    #[test]
    fn show_report_includes_user_section_and_repo_provenance() {
        let user = Context {
            flow_id: None,
            labels: vec![],
            params: BTreeMap::from([("owner".to_string(), Some("alice".to_string()))]),
        };
        let global = Context {
            flow_id: None,
            labels: vec![],
            params: BTreeMap::from([("owner".to_string(), Some("bob".to_string()))]),
        };
        let eff = Context::merge_layers(&[&user, &global]);

        let report = show_report(
            Some(&user),
            true,
            Some(Path::new("/tmp/user.json")),
            &global,
            None,
            false,
            &eff,
        );

        assert!(
            report.contains("=== User"),
            "missing User section: {report}"
        );
        assert!(
            report.contains("owner = bob   [from repo]"),
            "missing repo provenance for overridden param: {report}"
        );
    }

    /// Create a temp project with a `.tracevault/config.toml` for `run_source` tests.
    fn temp_project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let tv_dir = dir.path().join(".tracevault");
        fs::create_dir_all(&tv_dir).unwrap();
        fs::write(
            tv_dir.join("config.toml"),
            TracevaultConfig::default().to_toml(),
        )
        .unwrap();
        dir
    }

    #[test]
    fn source_enable_disable_and_path() {
        let dir = temp_project();

        run_source(dir.path(), true, false, None, false).unwrap();
        let cfg = TracevaultConfig::load(dir.path()).unwrap();
        assert!(cfg.user_context.unwrap().resolve().is_some());

        run_source(dir.path(), false, true, None, false).unwrap();
        let cfg = TracevaultConfig::load(dir.path()).unwrap();
        assert!(cfg.user_context.unwrap().resolve().is_none());

        run_source(dir.path(), false, false, Some("/p".to_string()), false).unwrap();
        let cfg = TracevaultConfig::load(dir.path()).unwrap();
        assert_eq!(
            cfg.user_context.unwrap().resolve(),
            Some(std::path::PathBuf::from("/p"))
        );
    }

    #[test]
    fn source_default_flag_enables() {
        let dir = temp_project();
        run_source(dir.path(), false, false, None, true).unwrap();
        let cfg = TracevaultConfig::load(dir.path()).unwrap();
        assert!(cfg.user_context.unwrap().resolve().is_some());
    }

    #[test]
    fn source_no_flags_errors() {
        let dir = temp_project();
        let err = run_source(dir.path(), false, false, None, false).unwrap_err();
        assert!(err.to_string().contains("--enable"));
    }

    #[test]
    fn parse_param_valid() {
        assert_eq!(
            parse_param("key=value").unwrap(),
            ("key".to_string(), "value".to_string())
        );
    }

    #[test]
    fn parse_param_value_contains_equals() {
        // Only split on the FIRST =
        assert_eq!(
            parse_param("key=a=b").unwrap(),
            ("key".to_string(), "a=b".to_string())
        );
    }

    #[test]
    fn parse_param_no_equals_errors() {
        let err = parse_param("noequals").unwrap_err();
        assert!(err.to_string().contains("expected key=value"));
        assert!(err.to_string().contains("noequals"));
    }

    #[test]
    fn split_labels_comma_separated() {
        let result = split_labels(&["a,b,c".to_string(), "d".to_string()]);
        assert_eq!(result, vec!["a", "b", "c", "d"]);
    }

    /// Create a temp project whose `config.toml` points `user_context` at an
    /// explicit path outside the project (`uc_path`).
    fn temp_project_with_user_context(uc_path: &Path) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let tv_dir = dir.path().join(".tracevault");
        fs::create_dir_all(&tv_dir).unwrap();
        let config = TracevaultConfig {
            user_context: Some(UserContext::Path(uc_path.display().to_string())),
            ..TracevaultConfig::default()
        };
        fs::write(tv_dir.join("config.toml"), config.to_toml()).unwrap();
        dir
    }

    #[test]
    fn set_user_writes_resolved_user_context_file() {
        let uc_dir = tempfile::tempdir().unwrap();
        let uc_path = uc_dir.path().join("uc.json");
        let project = temp_project_with_user_context(&uc_path);

        run_set(
            project.path(),
            None,
            vec!["team".to_string()],
            vec!["env=prod".to_string()],
            false,
            true,
        )
        .unwrap();

        let ctx = Context::load_from(&uc_path);
        assert_eq!(ctx.labels, vec!["team".to_string()]);
        assert_eq!(ctx.params.get("env"), Some(&Some("prod".to_string())));
    }

    #[test]
    fn update_user_merges_into_resolved_user_context_file() {
        let uc_dir = tempfile::tempdir().unwrap();
        let uc_path = uc_dir.path().join("uc.json");
        let project = temp_project_with_user_context(&uc_path);

        run_set(
            project.path(),
            None,
            vec!["team".to_string()],
            vec!["env=prod".to_string()],
            false,
            true,
        )
        .unwrap();

        run_update(
            project.path(),
            None,
            vec!["extra".to_string()],
            vec![],
            vec![],
            vec!["env".to_string()],
            false,
            true,
        )
        .unwrap();

        let ctx = Context::load_from(&uc_path);
        assert_eq!(ctx.labels, vec!["team".to_string(), "extra".to_string()]);
        assert_eq!(ctx.params.get("env"), Some(&None));
    }

    #[test]
    fn user_context_path_no_checkout_uses_global_config() {
        let cfg_root = tempfile::tempdir().unwrap();
        std::fs::write(
            crate::config::user_config_path_in(cfg_root.path()),
            "user_context = true\n",
        )
        .unwrap();
        let cwd = tempfile::tempdir().unwrap(); // no .tracevault/ anywhere above
        let p = user_context_path_in(cwd.path(), cfg_root.path()).unwrap();
        assert_eq!(
            p,
            crate::config::default_user_context_path_in(cfg_root.path())
        );
    }

    #[test]
    fn user_context_path_no_checkout_defaults_when_global_absent() {
        let cfg_root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let p = user_context_path_in(cwd.path(), cfg_root.path()).unwrap();
        assert_eq!(
            p,
            crate::config::default_user_context_path_in(cfg_root.path())
        );
    }

    #[test]
    fn run_show_in_detached_uses_user_level_config_without_error() {
        let cfg_root = tempfile::tempdir().unwrap();
        std::fs::write(
            crate::config::user_config_path_in(cfg_root.path()),
            "user_context = true\n",
        )
        .unwrap();
        // Seed the referenced default context file so the enabled layer has content.
        let ctx = crate::config::default_user_context_path_in(cfg_root.path());
        crate::context::Context::default().save_to(&ctx).unwrap();
        let cwd = tempfile::tempdir().unwrap(); // no .tracevault/ anywhere above
                                                // Must resolve against the injected root (NOT ~/.config) and not error.
        assert!(run_show_in(cwd.path(), cfg_root.path()).is_ok());
    }

    #[test]
    fn user_context_path_explicit_off_repo_falls_through_to_user_level() {
        // A checkout whose repo config explicitly disables the user layer.
        let repo = tempfile::tempdir().unwrap();
        let tv = repo.path().join(".tracevault");
        std::fs::create_dir_all(&tv).unwrap();
        std::fs::write(
            tv.join("config.toml"),
            "agent = \"claude-code\"\nuser_context = false\n",
        )
        .unwrap();
        // A user-level config that points the user context at a custom path.
        let cfg_root = tempfile::tempdir().unwrap();
        let custom = cfg_root.path().join("mine.json");
        std::fs::write(
            crate::config::user_config_path_in(cfg_root.path()),
            format!("user_context = \"{}\"\n", custom.display()),
        )
        .unwrap();
        let p = user_context_path_in(repo.path(), cfg_root.path()).unwrap();
        assert_eq!(
            p, custom,
            "explicit-off repo must not hijack the --user edit to the default file"
        );
    }

    #[test]
    fn clear_user_resets_resolved_user_context_file() {
        let uc_dir = tempfile::tempdir().unwrap();
        let uc_path = uc_dir.path().join("uc.json");
        let project = temp_project_with_user_context(&uc_path);

        run_set(
            project.path(),
            None,
            vec!["team".to_string()],
            vec![],
            false,
            true,
        )
        .unwrap();

        run_clear(project.path(), false, true).unwrap();

        let ctx = Context::load_from(&uc_path);
        assert_eq!(ctx, Context::default());
    }
}
