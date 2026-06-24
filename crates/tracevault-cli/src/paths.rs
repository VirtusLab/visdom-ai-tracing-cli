/// Shared project-root resolution for the entire CLI.
///
/// This module provides a single [`resolve_project_root`] function that all CLI
/// paths (stream, check, verify-start, context, …) use to locate the primary
/// `.tracevault/` directory.  The resolution strategy is:
///
/// 1. **Git-aware (primary):** run `git rev-parse --git-common-dir` from
///    `start`; the primary worktree root is `parent(canonicalized git-common-dir)`.
///    This works correctly from a primary checkout, a nested worktree, AND a
///    sibling linked worktree (where the primary `.tracevault/` is not an
///    ancestor of the working directory).
///
/// 2. **Fallback (non-git):** ancestor-walk from `start` to the nearest
///    `.tracevault/` directory (matching the existing `stream.rs` behaviour).
///    Returns `start` itself if nothing is found.
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The resolved project root and how it was found.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectRoot {
    /// The directory that directly contains (or should contain) `.tracevault/`.
    pub root: PathBuf,
    /// How the root was discovered.
    pub source: ProjectRootSource,
}

/// How [`ProjectRoot`] was determined.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectRootSource {
    /// Resolved via `git rev-parse --git-common-dir` (works from any worktree).
    Git,
    /// Git is not available or `start` is not in a git repo; root was found by
    /// walking ancestors for the nearest `.tracevault/` directory.
    AncestorWalk,
    /// Neither git nor an ancestor `.tracevault/` was found; `start` itself is
    /// returned as the root (matching the existing `unwrap_or(start)` fallback).
    Fallback,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Resolve the primary project root starting from `start`.
///
/// See module-level documentation for the resolution strategy.
///
/// # Panics
/// Never panics.
pub fn resolve_project_root(start: &Path) -> ProjectRoot {
    // --- Strategy 1: git rev-parse --git-common-dir ---
    if let Some(primary_root) = git_common_dir_root(start) {
        return ProjectRoot {
            root: primary_root,
            source: ProjectRootSource::Git,
        };
    }

    // --- Strategy 2: ancestor walk for .tracevault/ ---
    for ancestor in start.ancestors() {
        if ancestor.join(".tracevault").is_dir() {
            return ProjectRoot {
                root: ancestor.to_path_buf(),
                source: ProjectRootSource::AncestorWalk,
            };
        }
    }

    // --- Strategy 3: last resort — return start itself ---
    ProjectRoot {
        root: start.to_path_buf(),
        source: ProjectRootSource::Fallback,
    }
}

/// Return the canonicalized git worktree toplevel for `from`.
///
/// Runs `git rev-parse --show-toplevel` in `from` and canonicalizes the result
/// so the value is comparable across the write side (the `stream` hook's origin
/// marker) and the read side (`verify-start` disambiguation) regardless of
/// symlinked paths (e.g. `/tmp` → `/private/tmp`). Falls back to the
/// canonicalized `from` when git is unavailable or `from` is not a repo.
///
/// # Panics
/// Never panics.
pub fn worktree_toplevel(from: &Path) -> String {
    Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(from)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| {
            let p = PathBuf::from(s.trim());
            p.canonicalize().unwrap_or(p).to_string_lossy().into_owned()
        })
        .unwrap_or_else(|| {
            from.canonicalize()
                .unwrap_or_else(|_| from.to_path_buf())
                .to_string_lossy()
                .into_owned()
        })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Run `git rev-parse --git-common-dir` from `start_dir` and return
/// `parent(canonicalized git-common-dir)`, i.e. the primary worktree root.
///
/// Returns `None` if git is not available, `start_dir` is not inside a git
/// repo, or the git-common-dir has no parent (shouldn't happen in practice).
fn git_common_dir_root(start_dir: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args([
            "-C",
            &start_dir.to_string_lossy(),
            "rev-parse",
            "--git-common-dir",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = std::str::from_utf8(&output.stdout).ok()?;
    let raw = stdout.lines().next()?.trim();

    // git may return a relative path; canonicalize against start_dir.
    let p = PathBuf::from(raw);
    let git_common_dir = if p.is_absolute() {
        p.canonicalize().ok()?
    } else {
        start_dir.join(&p).canonicalize().ok()?
    };

    // primary_root = parent of git-common-dir (the dir containing `.git/`)
    git_common_dir.parent().map(PathBuf::from)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{add_worktree, init_git_repo};

    // -----------------------------------------------------------------------
    // Git-aware resolution tests
    // -----------------------------------------------------------------------

    #[test]
    fn primary_checkout_resolves_to_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);

        let result = resolve_project_root(&repo);

        assert_eq!(result.source, ProjectRootSource::Git);
        assert_eq!(
            result.root.canonicalize().unwrap(),
            repo.canonicalize().unwrap(),
            "primary checkout must resolve to repo root"
        );
    }

    #[test]
    fn nested_worktree_resolves_to_primary_root() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        // Nested worktree lives INSIDE the primary repo directory tree.
        let wt = repo.join("nested-wt");

        std::fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);
        add_worktree(&repo, &wt);

        let result = resolve_project_root(&wt);

        assert_eq!(result.source, ProjectRootSource::Git);
        assert_eq!(
            result.root.canonicalize().unwrap(),
            repo.canonicalize().unwrap(),
            "nested worktree must resolve to primary repo root"
        );
    }

    #[test]
    fn sibling_worktree_resolves_to_primary_root() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        // Sibling worktree is OUTSIDE the primary repo directory tree —
        // the primary `.tracevault/` is NOT an ancestor.
        let wt = tmp.path().join("sibling-wt");

        std::fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);
        add_worktree(&repo, &wt);

        let result = resolve_project_root(&wt);

        assert_eq!(result.source, ProjectRootSource::Git);
        assert_eq!(
            result.root.canonicalize().unwrap(),
            repo.canonicalize().unwrap(),
            "sibling worktree must resolve to primary repo root (not the worktree dir)"
        );
    }

    // -----------------------------------------------------------------------
    // Fallback: non-git directory with .tracevault/ ancestor
    // -----------------------------------------------------------------------

    #[test]
    fn non_git_with_tracevault_ancestor_uses_ancestor_walk() {
        let tmp = tempfile::tempdir().unwrap();
        // Create .tracevault/ at the tmp root.
        std::fs::create_dir_all(tmp.path().join(".tracevault")).unwrap();

        // Start from a subdirectory.
        let subdir = tmp.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&subdir).unwrap();

        let result = resolve_project_root(&subdir);

        assert_eq!(result.source, ProjectRootSource::AncestorWalk);
        assert_eq!(
            result.root.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap(),
            "ancestor walk must find the directory that contains .tracevault/"
        );
    }

    #[test]
    fn non_git_no_tracevault_falls_back_to_start() {
        let tmp = tempfile::tempdir().unwrap();
        // No git repo, no .tracevault/ anywhere under /tmp (usually).
        let result = resolve_project_root(tmp.path());

        assert_eq!(result.source, ProjectRootSource::Fallback);
        assert_eq!(
            result.root,
            tmp.path(),
            "with no git and no .tracevault/, root must be start itself"
        );
    }
}
