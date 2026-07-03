use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Context struct
// ---------------------------------------------------------------------------

/// Active context written by `tracevault context set` and read by the CC hook.
///
/// Stored as pretty-printed JSON at `.tracevault/context.json`.
/// BTreeMap is used for `params` so keys are always in sorted order on disk,
/// giving stable, human-readable diffs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Context {
    pub flow_id: Option<String>,
    /// Deduplicated, non-empty labels. Duplicates are removed on save (first
    /// occurrence wins); blank/whitespace-only entries are dropped.
    pub labels: Vec<String>,
    /// Stored params. `Some(v)` sets a value; `None` is a delete tombstone that
    /// removes an inherited key from a lower-precedence layer during the merge.
    /// On disk, `Some("v")` serializes as `"v"` and `None` as `null`.
    pub params: BTreeMap<String, Option<String>>,
}

/// Fully resolved context stamped onto every event. Unlike [`Context`] its
/// params are string-only — tombstones have already been applied and dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct EffectiveContext {
    pub flow_id: Option<String>,
    pub labels: Vec<String>,
    pub params: BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------
// Worktree-aware resolution
// ---------------------------------------------------------------------------

/// Whether the current directory is the primary checkout or a linked worktree.
#[derive(Debug, Clone, PartialEq)]
pub enum WorktreeScope {
    /// We are in the primary (main) worktree. The global context IS the only context.
    Primary,
    /// We are in a linked worktree. `key` is the basename of git-dir
    /// (e.g. "foo" for `.git/worktrees/foo`).
    Linked { key: String },
}

/// Resolved paths for context storage, rooted in the primary worktree's `.tracevault/`.
#[derive(Debug, Clone)]
pub struct ContextPaths {
    /// `<primary_root>/.tracevault/`
    pub tracevault_dir: PathBuf,
    /// Primary or Linked.
    pub scope: WorktreeScope,
}

impl ContextPaths {
    /// `<tracevault_dir>/context.json` — the global (repo-wide) context file.
    pub fn global_path(&self) -> PathBuf {
        self.tracevault_dir.join("context.json")
    }

    /// `<tracevault_dir>/worktrees/<key>/context.json` when in a linked worktree;
    /// `None` when in the primary worktree.
    pub fn worktree_path(&self) -> Option<PathBuf> {
        match &self.scope {
            WorktreeScope::Primary => None,
            WorktreeScope::Linked { key } => Some(
                self.tracevault_dir
                    .join("worktrees")
                    .join(key)
                    .join("context.json"),
            ),
        }
    }
}

/// Run `git rev-parse --git-common-dir --git-dir` from `start_dir`.
///
/// Returns `(git_common_dir, git_dir)` both canonicalized to absolute paths
/// (git may return relative paths; we resolve them against `start_dir`).
///
/// Returns `None` if git fails (not a git repo, git not installed, etc.).
fn git_rev_parse(start_dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let output = Command::new("git")
        .args([
            "-C",
            &start_dir.to_string_lossy(),
            "rev-parse",
            "--git-common-dir",
            "--git-dir",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = std::str::from_utf8(&output.stdout).ok()?;
    let mut lines = stdout.lines();
    let raw_common = lines.next()?.trim();
    let raw_dir = lines.next()?.trim();

    // git may return relative paths; canonicalize against start_dir.
    let canonicalize = |raw: &str| -> Option<PathBuf> {
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            // Already absolute — canonicalize to resolve symlinks.
            p.canonicalize().ok()
        } else {
            // Relative to start_dir.
            start_dir.join(&p).canonicalize().ok()
        }
    };

    let git_common_dir = canonicalize(raw_common)?;
    let git_dir = canonicalize(raw_dir)?;

    Some((git_common_dir, git_dir))
}

/// Resolve worktree-aware context paths from `start_dir`.
///
/// Resolution order:
/// 1. Run `git rev-parse --git-common-dir --git-dir` to locate the primary worktree.
///    - `tracevault_dir = parent(git_common_dir)/.tracevault`
///    - If `git_dir == git_common_dir` → Primary scope.
///    - Else → Linked scope with `key = basename(git_dir)`.
/// 2. Fallback (git not available or not a repo): delegate to
///    [`crate::paths::resolve_project_root`] which walks up from `start_dir` to
///    the nearest `.tracevault/` (or returns `start_dir` as last resort).
///    Treat as Primary scope in both cases.
pub fn resolve_context_paths(start_dir: &Path) -> ContextPaths {
    // --- Git-aware path ---
    if let Some((git_common_dir, git_dir)) = git_rev_parse(start_dir) {
        // primary_root = parent of git_common_dir (e.g. parent of `.git` = repo root)
        if let Some(primary_root) = git_common_dir.parent() {
            let tracevault_dir = primary_root.join(".tracevault");

            let scope = if git_dir == git_common_dir {
                WorktreeScope::Primary
            } else {
                // Linked worktree: key = basename of git_dir (e.g. "foo" from .git/worktrees/foo)
                let key = git_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "default".to_string());
                WorktreeScope::Linked { key }
            };

            return ContextPaths {
                tracevault_dir,
                scope,
            };
        }
    }

    // --- Fallback: delegate to the shared resolver (ancestor-walk or start_dir) ---
    let fallback = crate::paths::resolve_project_root(start_dir);
    ContextPaths {
        tracevault_dir: fallback.root.join(".tracevault"),
        scope: WorktreeScope::Primary,
    }
}

// ---------------------------------------------------------------------------
// Context I/O
// ---------------------------------------------------------------------------

impl Context {
    /// Load the context from an explicit file path.
    ///
    /// - Missing file → `Context::default()` (silent).
    /// - Malformed JSON → `Context::default()` + a warning printed to stderr.
    ///
    /// Use `ContextPaths::global_path()` or `ContextPaths::worktree_path()` to
    /// obtain the correct path for the current worktree context.
    pub fn load_from(path: &Path) -> Context {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Context::default(),
            Err(e) => {
                eprintln!(
                    "tracevault: warning: could not read context file {}: {e}",
                    path.display()
                );
                return Context::default();
            }
        };
        match serde_json::from_str::<Context>(&content) {
            Ok(ctx) => ctx,
            Err(e) => {
                eprintln!(
                    "tracevault: warning: malformed context file {} (using empty context): {e}",
                    path.display()
                );
                Context::default()
            }
        }
    }

    /// Save the context to an explicit file path.
    ///
    /// Normalises labels first:
    /// 1. Trim whitespace from each label.
    /// 2. Drop blank labels.
    /// 3. Deduplicate: first occurrence wins.
    ///
    /// Creates the parent directory (including `worktrees/<key>/`) if it does not exist.
    ///
    /// Use `save_global` or `save_worktree` for scope-aware saves.
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let normalised = Self {
            flow_id: self.flow_id.clone(),
            labels: dedup_labels(&self.labels),
            params: self.params.clone(),
        };

        let json = serde_json::to_string_pretty(&normalised)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        std::fs::write(path, json)
    }

    // ---------------------------------------------------------------------------
    // Scope-aware load/save
    // ---------------------------------------------------------------------------

    /// Load the global context from the given resolved paths.
    pub fn load_global(paths: &ContextPaths) -> Context {
        Self::load_from(&paths.global_path())
    }

    /// Load the per-worktree context (if in a linked worktree and the file exists).
    ///
    /// Returns `None` when:
    /// - In a primary checkout (no per-worktree path).
    /// - In a linked worktree but the per-worktree file does not yet exist.
    ///
    /// Returns `Some(ctx)` only when the file exists and was successfully (or partially) loaded.
    pub fn load_worktree(paths: &ContextPaths) -> Option<Context> {
        let wt_path = paths.worktree_path()?;
        if !wt_path.exists() {
            return None;
        }
        Some(Self::load_from(&wt_path))
    }

    /// Save to the global context path.
    pub fn save_global(&self, paths: &ContextPaths) -> io::Result<()> {
        self.save_to(&paths.global_path())
    }

    /// Save to the per-worktree context path. Errors if called from a Primary scope.
    pub fn save_worktree(&self, paths: &ContextPaths) -> io::Result<()> {
        let wt_path = paths.worktree_path().ok_or_else(|| {
            io::Error::other("cannot save per-worktree context from primary checkout")
        })?;
        self.save_to(&wt_path)
    }

    // ---------------------------------------------------------------------------
    // Effective merge
    // ---------------------------------------------------------------------------

    /// Fold layers low→high into the effective context.
    /// - flow_id: highest layer that sets a non-null value wins.
    /// - labels: union across layers, deduped, low→high stable order.
    /// - params: JSON merge-patch per layer — `Some(v)` sets/overrides,
    ///   `None` deletes an inherited key. Effective params are string-only.
    pub fn merge_layers(layers: &[&Context]) -> EffectiveContext {
        let mut flow_id: Option<String> = None;
        let mut all_labels: Vec<String> = Vec::new();
        let mut params: BTreeMap<String, String> = BTreeMap::new();

        for layer in layers {
            if layer.flow_id.is_some() {
                flow_id = layer.flow_id.clone();
            }
            all_labels.extend(layer.labels.iter().cloned());
            for (k, v) in &layer.params {
                match v {
                    Some(val) => {
                        params.insert(k.clone(), val.clone());
                    }
                    None => {
                        params.remove(k);
                    }
                }
            }
        }

        EffectiveContext {
            flow_id,
            labels: dedup_labels(&all_labels),
            params,
        }
    }

    /// Load user (if given) + global + per-worktree and return the merged
    /// effective context that the hook stamps.
    pub fn effective(start_dir: &Path, user_layer: Option<&Path>) -> EffectiveContext {
        let (_, _, _, _, eff) = Self::effective_with_parts(start_dir, user_layer);
        eff
    }

    /// Like `effective`, returning each layer separately (for `show`).
    /// Tuple: (paths, user, global, worktree, effective).
    #[allow(clippy::type_complexity)]
    pub fn effective_with_parts(
        start_dir: &Path,
        user_layer: Option<&Path>,
    ) -> (
        ContextPaths,
        Option<Context>,
        Context,
        Option<Context>,
        EffectiveContext,
    ) {
        let paths = resolve_context_paths(start_dir);
        let user = user_layer.map(Self::load_from);
        let global = Self::load_global(&paths);
        let worktree = Self::load_worktree(&paths);

        let mut layers: Vec<&Context> = Vec::new();
        if let Some(u) = &user {
            layers.push(u);
        }
        layers.push(&global);
        if let Some(w) = &worktree {
            layers.push(w);
        }
        let effective = Self::merge_layers(&layers);
        (paths, user, global, worktree, effective)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Trim whitespace, drop blanks, remove duplicates (first occurrence wins).
fn dedup_labels(labels: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    labels
        .iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .filter(|l| seen.insert(l.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn default_context_is_empty() {
        let ctx = Context::default();
        assert!(ctx.flow_id.is_none());
        assert!(ctx.labels.is_empty());
        assert!(ctx.params.is_empty());
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let ctx_path = dir.path().join(".tracevault").join("context.json");
        let mut params = BTreeMap::new();
        params.insert("key1".to_string(), Some("value1".to_string()));
        params.insert("key2".to_string(), Some("value2".to_string()));

        let ctx = Context {
            flow_id: Some("flow-abc-123".to_string()),
            labels: vec!["backend".to_string(), "urgent".to_string()],
            params,
        };

        ctx.save_to(&ctx_path).unwrap();
        let loaded = Context::load_from(&ctx_path);
        assert_eq!(ctx, loaded);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join(".tracevault").join("context.json");
        let ctx = Context::load_from(&missing);
        assert_eq!(ctx, Context::default());
    }

    #[test]
    fn label_dedup_on_save() {
        let dir = tempfile::tempdir().unwrap();
        let ctx_path = dir.path().join(".tracevault").join("context.json");
        let ctx = Context {
            flow_id: None,
            labels: vec![
                "alpha".to_string(),
                "beta".to_string(),
                "alpha".to_string(), // duplicate — should be dropped
                "  ".to_string(),    // whitespace-only — should be dropped
                "gamma".to_string(),
                "beta".to_string(), // duplicate — should be dropped
            ],
            params: BTreeMap::new(),
        };

        ctx.save_to(&ctx_path).unwrap();
        let loaded = Context::load_from(&ctx_path);
        assert_eq!(
            loaded.labels,
            vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
        );
    }

    #[test]
    fn load_malformed_json_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let tv_dir = dir.path().join(".tracevault");
        std::fs::create_dir_all(&tv_dir).unwrap();
        let ctx_path = tv_dir.join("context.json");
        std::fs::write(&ctx_path, b"not valid json {{{{").unwrap();

        let ctx = Context::load_from(&ctx_path);
        assert_eq!(ctx, Context::default());
    }

    #[test]
    fn save_creates_tracevault_dir_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let ctx_path = dir.path().join(".tracevault").join("context.json");
        // Ensure .tracevault does NOT exist yet
        assert!(!dir.path().join(".tracevault").exists());

        let ctx = Context::default();
        ctx.save_to(&ctx_path).unwrap();

        assert!(ctx_path.exists());
    }

    #[test]
    fn params_tombstone_round_trips_as_null() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".tracevault").join("context.json");
        let mut params = BTreeMap::new();
        params.insert("keep".to_string(), Some("v".to_string()));
        params.insert("drop".to_string(), None); // tombstone
        let ctx = Context {
            flow_id: None,
            labels: vec![],
            params,
        };
        ctx.save_to(&path).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("\"drop\": null"),
            "tombstone must serialize as null"
        );
        assert!(raw.contains("\"keep\": \"v\""));
        assert_eq!(Context::load_from(&path), ctx);
    }

    #[test]
    fn legacy_string_params_load_as_some() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".tracevault").join("context.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"flow_id":null,"labels":[],"params":{"k":"v"}}"#).unwrap();
        let ctx = Context::load_from(&path);
        assert_eq!(ctx.params.get("k"), Some(&Some("v".to_string())));
    }

    // ---------------------------------------------------------------------------
    // merge_layers (ordered fold) unit tests
    // ---------------------------------------------------------------------------

    fn ctx(flow: Option<&str>, labels: &[&str], params: &[(&str, &str)]) -> Context {
        Context {
            flow_id: flow.map(str::to_string),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), Some(v.to_string())))
                .collect(),
        }
    }

    #[test]
    fn fold_precedence_and_labels_union() {
        let user = ctx(Some("u"), &["base"], &[("env", "prod"), ("owner", "u")]);
        let repo = ctx(Some("r"), &["repo"], &[("owner", "team")]); // overrides owner
        let wt = ctx(None, &["wt"], &[]);
        let eff = Context::merge_layers(&[&user, &repo, &wt]);
        assert_eq!(eff.flow_id, Some("r".to_string())); // highest non-null (wt is None)
        assert_eq!(eff.labels, vec!["base", "repo", "wt"]); // union, low→high order
        assert_eq!(eff.params["env"], "prod"); // only user set it
        assert_eq!(eff.params["owner"], "team"); // repo overrides user
    }

    #[test]
    fn fold_tombstone_removes_lower_layer_param() {
        let user = ctx(Some("u"), &[], &[("secret", "x")]);
        let mut repo = ctx(None, &[], &[]);
        repo.params.insert("secret".to_string(), None); // tombstone
        let eff = Context::merge_layers(&[&user, &repo]);
        assert!(
            !eff.params.contains_key("secret"),
            "tombstone drops inherited key"
        );
    }

    #[test]
    fn fold_single_layer_matches_input() {
        let only = ctx(Some("g"), &["a"], &[("k", "1")]);
        let eff = Context::merge_layers(&[&only]);
        assert_eq!(eff.flow_id, Some("g".to_string()));
        assert_eq!(eff.labels, vec!["a"]);
        assert_eq!(eff.params["k"], "1");
    }
}
