//! Detached/workspace-mode repo resolution: turn a filesystem path into a
//! `RepoBinding` (via the path's git remote + the server), and pick the
//! effective binding for an event/command from the precedence chain
//! (`--repo` flag → subagent worktree override → session active → bound
//! `.tracevault/config.toml`). See design §2/§3/§4.

use std::path::Path;

use crate::api_client::ApiClient;
use crate::session_state::{RepoBinding, SessionState};

/// Pure precedence for the org slug: first non-`None` of env, credentials,
/// bound config. Callers pass `None` for an env value that was empty.
pub fn org_slug_precedence(
    env: Option<String>,
    creds: Option<String>,
    bound: Option<String>,
) -> Option<String> {
    env.or(creds).or(bound)
}

/// Resolve the org slug for `project_root`: `TRACEVAULT_ORG_SLUG` (non-empty)
/// → `credentials.json` `org_slug` → bound `config.toml` `org_slug`.
pub fn org_slug_for(project_root: &Path) -> Option<String> {
    let env = std::env::var("TRACEVAULT_ORG_SLUG")
        .ok()
        .filter(|s| !s.is_empty());
    let creds = crate::credentials::Credentials::load().and_then(|c| c.org_slug);
    let bound = crate::config::TracevaultConfig::load(project_root).and_then(|c| c.org_slug);
    org_slug_precedence(env, creds, bound)
}

/// `git -C <path> remote get-url origin`, trimmed. `None` if git fails or there
/// is no origin remote.
fn git_remote_url(path: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if url.is_empty() {
        None
    } else {
        Some(url)
    }
}

/// Resolve a filesystem path to a registered-repo binding: read its origin
/// remote URL and ask the server. `Ok(None)` when the path has no remote or
/// the server has no matching repo (pre-registered-only).
pub async fn resolve_path_to_binding(
    path: &Path,
    org_slug: &str,
    client: &ApiClient,
) -> Result<Option<RepoBinding>, Box<dyn std::error::Error>> {
    let Some(git_url) = git_remote_url(path) else {
        return Ok(None);
    };
    match client.resolve_repo(org_slug, &git_url).await? {
        Some(repo_id) => Ok(Some(RepoBinding {
            org_slug: org_slug.to_string(),
            repo_id: repo_id.to_string(),
            git_url: Some(git_url),
            updated_at: chrono::Utc::now().to_rfc3339(),
        })),
        None => Ok(None),
    }
}

/// Inputs for the effective-binding precedence chain. `repo_flag` and `bound`
/// are resolved by the caller (the `--repo` override and the bound
/// `config.toml`, respectively); `session`/`worktree_path` come from the
/// per-session state.
pub struct ResolveInputs<'a> {
    pub repo_flag: Option<RepoBinding>,
    pub session: &'a SessionState,
    pub worktree_path: Option<&'a str>,
    pub bound: Option<RepoBinding>,
}

/// Which precedence tier produced the [`effective_binding`] result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingSource {
    /// A `--repo <path>` flag override.
    RepoFlag,
    /// A subagent's per-worktree override.
    Subagent,
    /// The session's session-level active binding.
    SessionActive,
    /// A pinned `.tracevault/config.toml` (bound mode).
    Bound,
}

/// The binding that applies, and which tier produced it: `--repo` flag →
/// subagent worktree override → session active → bound config → none.
pub fn effective_binding(inputs: ResolveInputs) -> Option<(RepoBinding, BindingSource)> {
    if let Some(b) = inputs.repo_flag {
        return Some((b, BindingSource::RepoFlag));
    }
    if let Some(wt) = inputs.worktree_path {
        if let Some(b) = inputs.session.subagents.get(wt) {
            return Some((b.clone(), BindingSource::Subagent));
        }
    }
    if let Some(b) = &inputs.session.active {
        return Some((b.clone(), BindingSource::SessionActive));
    }
    inputs.bound.map(|b| (b, BindingSource::Bound))
}

/// A RepoBinding from a pinned `.tracevault/config.toml` (bound mode), if it has
/// both org_slug and repo_id. Pure — caller supplies the already-loaded config.
pub fn binding_from_config(config: &crate::config::TracevaultConfig) -> Option<RepoBinding> {
    Some(RepoBinding {
        org_slug: config.org_slug.clone()?,
        repo_id: config.repo_id.clone()?,
        git_url: None,
        updated_at: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_state::{RepoBinding, SessionState};
    use std::collections::HashMap;

    fn binding(id: &str) -> RepoBinding {
        RepoBinding {
            org_slug: "org".into(),
            repo_id: id.into(),
            git_url: None,
            updated_at: "t".into(),
        }
    }

    #[test]
    fn effective_binding_precedence() {
        let session = SessionState {
            active: Some(binding("session")),
            subagents: HashMap::from([("/wt/a".to_string(), binding("subagent"))]),
        };

        // 1. repo_flag wins over everything.
        let (b, source) = effective_binding(ResolveInputs {
            repo_flag: Some(binding("flag")),
            session: &session,
            worktree_path: Some("/wt/a"),
            bound: Some(binding("bound")),
        })
        .unwrap();
        assert_eq!(b.repo_id, "flag");
        assert_eq!(source, BindingSource::RepoFlag);

        // 2. subagent override (for the worktree) wins over session active + bound.
        let (b, source) = effective_binding(ResolveInputs {
            repo_flag: None,
            session: &session,
            worktree_path: Some("/wt/a"),
            bound: Some(binding("bound")),
        })
        .unwrap();
        assert_eq!(b.repo_id, "subagent");
        assert_eq!(source, BindingSource::Subagent);

        // 3. session active wins over bound when no subagent override matches.
        let (b, source) = effective_binding(ResolveInputs {
            repo_flag: None,
            session: &session,
            worktree_path: Some("/wt/other"),
            bound: Some(binding("bound")),
        })
        .unwrap();
        assert_eq!(b.repo_id, "session");
        assert_eq!(source, BindingSource::SessionActive);

        // 4. bound is last resort.
        let empty = SessionState::default();
        let (b, source) = effective_binding(ResolveInputs {
            repo_flag: None,
            session: &empty,
            worktree_path: None,
            bound: Some(binding("bound")),
        })
        .unwrap();
        assert_eq!(b.repo_id, "bound");
        assert_eq!(source, BindingSource::Bound);

        // 5. nothing → None.
        let got = effective_binding(ResolveInputs {
            repo_flag: None,
            session: &empty,
            worktree_path: None,
            bound: None,
        });
        assert!(got.is_none());
    }

    #[test]
    fn org_slug_precedence_order() {
        assert_eq!(
            org_slug_precedence(
                Some("envorg".into()),
                Some("creds".into()),
                Some("bound".into())
            ),
            Some("envorg".into())
        );
        assert_eq!(
            org_slug_precedence(None, Some("creds".into()), Some("bound".into())),
            Some("creds".into())
        );
        assert_eq!(
            org_slug_precedence(None, None, Some("bound".into())),
            Some("bound".into())
        );
        assert_eq!(org_slug_precedence(None, None, None), None);
        // empty env string is treated as unset by the caller (org_slug_for),
        // so org_slug_precedence only ever sees None or non-empty.
    }
}
