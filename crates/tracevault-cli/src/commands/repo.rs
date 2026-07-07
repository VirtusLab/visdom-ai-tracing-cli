//! `tracevault repo` — workspace/detached-mode repo binding commands.

use std::path::Path;

use crate::api_client::{resolve_credentials, ApiClient};
use crate::resolution::{effective_binding, org_slug_for, resolve_path_to_binding, ResolveInputs};
use crate::session_state::{self, RepoBinding, SessionState};

/// Sub-actions for `tracevault repo` (workspace/detached mode).
#[derive(clap::Subcommand)]
pub enum RepoCmd {
    /// Bind tracing to the repo at <path> for the current session and print
    /// its policies.
    Switch {
        /// Path to a checkout of a registered repo.
        path: String,
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Show the repo the current session is bound to.
    Status {
        #[arg(long)]
        session_id: Option<String>,
        /// One-off override: resolve this path instead of the session binding.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Clear the current session's binding.
    Reset {
        #[arg(long)]
        session_id: Option<String>,
    },
}

/// Resolve the current session id: the explicit flag wins, else
/// `$TRACEVAULT_SESSION_ID` (set by the SessionStart hook of `init --global`).
pub fn resolve_session_id(explicit: Option<&str>) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(s) = explicit {
        return Ok(s.to_string());
    }
    std::env::var("TRACEVAULT_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "no session id available: pass --session-id or set TRACEVAULT_SESSION_ID \
             (installed by `tracevault init --global`)"
                .into()
        })
}

/// The binding implied by a pinned `.tracevault/config.toml` (bound mode), if any.
pub fn bound_binding(project_root: &Path) -> Option<RepoBinding> {
    let cfg = crate::config::TracevaultConfig::load(project_root)?;
    let org_slug = cfg.org_slug?;
    let repo_id = cfg.repo_id?;
    Some(RepoBinding {
        org_slug,
        repo_id,
        git_url: None,
        updated_at: String::new(),
    })
}

pub async fn run(
    cmd: RepoCmd,
    project_root: &Path,
    cwd: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        RepoCmd::Status { session_id, repo } => {
            status(session_id.as_deref(), repo.as_deref(), project_root, cwd).await
        }
        RepoCmd::Switch { .. } => {
            Err("`repo switch` not yet implemented (sub-plan B task 2)".into())
        }
        RepoCmd::Reset { .. } => Err("`repo reset` not yet implemented (sub-plan B task 3)".into()),
    }
}

/// Resolve a `--repo <path>` override into a live `RepoBinding`, if possible.
/// Best-effort: prints a clear message and returns `None` on any failure
/// (missing org slug / credentials, or a server error) rather than failing
/// the whole `status` inspector.
async fn resolve_repo_flag(
    repo_flag_path: Option<&str>,
    project_root: &Path,
) -> Option<RepoBinding> {
    let path = repo_flag_path?;

    let Some(org_slug) = org_slug_for(project_root) else {
        eprintln!(
            "--repo {path}: no org slug configured (set TRACEVAULT_ORG_SLUG, log in, or bind \
             the repo); showing binding without the --repo override"
        );
        return None;
    };

    let (server_url, token) = resolve_credentials(project_root);
    let Some(server_url) = server_url else {
        eprintln!(
            "--repo {path}: no server URL configured (run `tracevault login`); showing binding \
             without the --repo override"
        );
        return None;
    };

    let client = ApiClient::new(&server_url, token.as_deref());
    match resolve_path_to_binding(Path::new(path), &org_slug, &client).await {
        Ok(Some(binding)) => Some(binding),
        Ok(None) => {
            eprintln!(
                "--repo {path}: no registered repo found for this path's git remote; showing \
                 binding without the --repo override"
            );
            None
        }
        Err(e) => {
            eprintln!(
                "--repo {path}: failed to resolve ({e}); showing binding without the --repo \
                 override"
            );
            None
        }
    }
}

async fn status(
    session_id: Option<&str>,
    repo_flag_path: Option<&str>,
    project_root: &Path,
    cwd: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    // Session state is best-effort: if a session id resolves, load it; else
    // fall back to an empty SessionState rather than failing the inspector.
    let session = match resolve_session_id(session_id) {
        Ok(id) => session_state::load(&id),
        Err(_) => SessionState::default(),
    };
    let worktree = crate::paths::worktree_toplevel(cwd);
    let bound = bound_binding(project_root);

    // A --repo override on status resolves live (needs org_slug + server).
    let repo_flag = resolve_repo_flag(repo_flag_path, project_root).await;

    match effective_binding(ResolveInputs {
        repo_flag,
        session: &session,
        worktree_path: Some(&worktree),
        bound,
    }) {
        Some(b) => println!("bound to repo {} (org {})", b.repo_id, b.org_slug),
        None => {
            println!("not bound to any repo (workspace mode; run `tracevault repo switch <path>`)")
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_session_id_prefers_explicit() {
        assert_eq!(resolve_session_id(Some("sess-x")).unwrap(), "sess-x");
    }

    #[test]
    fn resolve_session_id_errors_when_absent() {
        // No explicit id and (in this test process) no env var set → error.
        // Guard against a leaked env var from another test by checking the
        // message only when the var is genuinely absent.
        if std::env::var("TRACEVAULT_SESSION_ID").is_err() {
            assert!(resolve_session_id(None).is_err());
        }
    }
}
