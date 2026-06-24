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

/// `tracevault context set` — build a fresh Context from flags, save it.
/// Omitted dimensions are empty. Clears anything not explicitly provided.
///
/// Default scope:
/// - Linked worktree → writes `worktrees/<key>/context.json`.
/// - Primary checkout → writes the global `context.json`.
///
/// `global = true` forces the global file from any worktree.
pub fn run_set(
    cwd: &Path,
    flow: Option<String>,
    labels: Vec<String>,
    params: Vec<String>,
    global: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut parsed_params = std::collections::BTreeMap::new();
    for raw in &params {
        let (k, v) = parse_param(raw)?;
        parsed_params.insert(k, v);
    }

    let ctx = Context {
        flow_id: flow,
        labels: split_labels(&labels),
        params: parsed_params,
    };

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
pub fn run_update(
    cwd: &Path,
    flow: Option<String>,
    labels: Vec<String>,
    params: Vec<String>,
    remove_labels: Vec<String>,
    remove_params: Vec<String>,
    global: bool,
) -> Result<(), Box<dyn std::error::Error>> {
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

    // Set flow if provided
    if let Some(f) = flow {
        ctx.flow_id = Some(f);
    }

    // Union-add labels (dedup guarded inline by the contains check below; save also normalises)
    let new_labels = split_labels(&labels);
    for l in new_labels {
        if !ctx.labels.contains(&l) {
            ctx.labels.push(l);
        }
    }

    // Insert/overwrite params
    for raw in &params {
        let (k, v) = parse_param(raw)?;
        ctx.params.insert(k, v);
    }

    // Remove labels
    let remove_set: std::collections::HashSet<String> =
        split_labels(&remove_labels).into_iter().collect();
    ctx.labels.retain(|l| !remove_set.contains(l));

    // Remove params
    for k in &remove_params {
        ctx.params.remove(k);
    }

    if use_global {
        ctx.save_global(&paths)
    } else {
        ctx.save_worktree(&paths)
    }
    .map_err(|e: io::Error| format!("failed to save context: {e}"))?;

    println!("Context updated.");
    Ok(())
}

/// `tracevault context show` — print three labelled sections:
///   1. Global — the repo-wide `context.json`.
///   2. This worktree — the per-worktree context (Linked only; "(none)" when missing).
///   3. Effective — the merged result that the hook stamps on every event.
///
/// Also prints the resolved file paths.
///
/// Prints a hint if no `.tracevault/` directory is found (i.e. `tracevault init` has
/// not been run), using `find_project_root` for the diagnostic.
pub fn run_show(cwd: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let (paths, global, worktree, effective) = Context::effective_with_parts(cwd);

    // If the tracevault directory doesn't exist yet, hint that init is needed.
    // find_project_root gives the classic walk-up path; if it also fails, the
    // user has not run `tracevault init` yet.
    if !paths.tracevault_dir.exists() && find_project_root(cwd).is_err() {
        eprintln!("hint: No .tracevault/ directory found. Run `tracevault init` first.");
    }

    println!("=== Global ===");
    println!("File: {}", paths.global_path().display());
    println!("{}", serde_json::to_string_pretty(&global)?);

    match &paths.scope {
        WorktreeScope::Linked { key } => {
            let wt_path = paths
                .worktree_path()
                .expect("Linked scope always has a worktree_path");
            println!("\n=== This worktree ({key}) ===");
            println!("File: {}", wt_path.display());
            match &worktree {
                Some(wt) => println!("{}", serde_json::to_string_pretty(wt)?),
                None => println!("(none)"),
            }
        }
        WorktreeScope::Primary => {
            // No separate per-worktree section for the primary checkout.
        }
    }

    println!("\n=== Effective ===");
    println!("{}", serde_json::to_string_pretty(&effective)?);

    Ok(())
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
pub fn run_clear(cwd: &Path, global: bool) -> Result<(), Box<dyn std::error::Error>> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
