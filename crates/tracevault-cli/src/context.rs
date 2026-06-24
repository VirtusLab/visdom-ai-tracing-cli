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

    /// Compute the effective context by merging global and per-worktree contexts:
    ///
    /// - `flow_id`: worktree wins if set, otherwise falls back to global.
    /// - `labels`: global ∪ worktree (dedup, global-first order, stable).
    /// - `params`: global then worktree overwrites keys (worktree takes precedence).
    pub fn merge_effective(global: &Context, worktree: Option<&Context>) -> Context {
        let wt = match worktree {
            Some(w) => w,
            None => return global.clone(),
        };

        // flow: worktree wins if set
        let flow_id = wt.flow_id.clone().or_else(|| global.flow_id.clone());

        // labels: global ∪ worktree, dedup (global-first, stable order).
        // dedup_labels trims, drops blanks, and deduplicates — no separate contains-loop needed.
        let combined_labels: Vec<String> = global
            .labels
            .iter()
            .chain(wt.labels.iter())
            .cloned()
            .collect();
        let labels = dedup_labels(&combined_labels);

        // params: global base, worktree overrides
        let mut params = global.params.clone();
        for (k, v) in &wt.params {
            params.insert(k.clone(), v.clone());
        }

        Context {
            flow_id,
            labels,
            params,
        }
    }

    /// Resolve paths from `start_dir`, load global + per-worktree, and return the
    /// merged effective context. This is what the hook stamps.
    pub fn effective(start_dir: &Path) -> Context {
        let paths = resolve_context_paths(start_dir);
        let global = Self::load_global(&paths);
        let worktree = Self::load_worktree(&paths);
        Self::merge_effective(&global, worktree.as_ref())
    }

    /// Like `effective`, but also returns the resolved paths, global, and per-worktree
    /// contexts separately (for `show` to display all three).
    pub fn effective_with_parts(
        start_dir: &Path,
    ) -> (ContextPaths, Context, Option<Context>, Context) {
        let paths = resolve_context_paths(start_dir);
        let global = Self::load_global(&paths);
        let worktree = Self::load_worktree(&paths);
        let effective = Self::merge_effective(&global, worktree.as_ref());
        (paths, global, worktree, effective)
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
        params.insert("key1".to_string(), "value1".to_string());
        params.insert("key2".to_string(), "value2".to_string());

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

    // ---------------------------------------------------------------------------
    // merge_effective unit tests
    // ---------------------------------------------------------------------------

    fn ctx(flow: Option<&str>, labels: &[&str], params: &[(&str, &str)]) -> Context {
        Context {
            flow_id: flow.map(str::to_string),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn merge_no_worktree_returns_global() {
        let global = ctx(Some("g"), &["a"], &[("k", "1")]);
        let result = Context::merge_effective(&global, None);
        assert_eq!(result, global);
    }

    #[test]
    fn merge_worktree_flow_wins() {
        let global = ctx(Some("g"), &[], &[]);
        let wt = ctx(Some("w"), &[], &[]);
        let result = Context::merge_effective(&global, Some(&wt));
        assert_eq!(result.flow_id, Some("w".to_string()));
    }

    #[test]
    fn merge_worktree_flow_none_falls_back_to_global() {
        let global = ctx(Some("g"), &[], &[]);
        let wt = ctx(None, &[], &[]);
        let result = Context::merge_effective(&global, Some(&wt));
        assert_eq!(result.flow_id, Some("g".to_string()));
    }

    #[test]
    fn merge_labels_union_dedup_global_first() {
        let global = ctx(None, &["a"], &[]);
        let wt = ctx(None, &["b"], &[]);
        let result = Context::merge_effective(&global, Some(&wt));
        assert_eq!(result.labels, vec!["a", "b"]);
    }

    #[test]
    fn merge_labels_no_duplicates() {
        let global = ctx(None, &["a", "b"], &[]);
        let wt = ctx(None, &["b", "c"], &[]);
        let result = Context::merge_effective(&global, Some(&wt));
        assert_eq!(result.labels, vec!["a", "b", "c"]);
    }

    #[test]
    fn merge_params_worktree_overwrites_keys() {
        let global = ctx(None, &[], &[("k", "1"), ("m", "old")]);
        let wt = ctx(None, &[], &[("k", "2"), ("m", "3")]);
        let result = Context::merge_effective(&global, Some(&wt));
        assert_eq!(result.params["k"], "2");
        assert_eq!(result.params["m"], "3");
    }

    #[test]
    fn merge_params_global_keys_not_in_worktree_preserved() {
        let global = ctx(None, &[], &[("g_only", "yes")]);
        let wt = ctx(None, &[], &[("wt_only", "yes")]);
        let result = Context::merge_effective(&global, Some(&wt));
        assert_eq!(result.params["g_only"], "yes");
        assert_eq!(result.params["wt_only"], "yes");
    }

    #[test]
    fn merge_full_example_from_spec() {
        // global{flow:g, labels:[a], params:{k:1}}
        // worktree{flow:w, labels:[b], params:{k:2,m:3}}
        // → effective{flow:w, labels:[a,b], params:{k:2,m:3}}
        let global = ctx(Some("g"), &["a"], &[("k", "1")]);
        let wt = ctx(Some("w"), &["b"], &[("k", "2"), ("m", "3")]);
        let result = Context::merge_effective(&global, Some(&wt));
        assert_eq!(result.flow_id, Some("w".to_string()));
        assert_eq!(result.labels, vec!["a", "b"]);
        assert_eq!(result.params["k"], "2");
        assert_eq!(result.params["m"], "3");
    }
}
