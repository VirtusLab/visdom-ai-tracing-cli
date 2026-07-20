//! Detached/workspace-mode repo resolution: turn a filesystem path into a
//! `RepoBinding` (via the path's git remote + the server), and pick the
//! effective binding for an event/command from the precedence chain
//! (`--path` flag → subagent worktree override → session active → bound
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

/// Pick the org slug from the credential's memberships when none is
/// configured locally. The slugs are de-duplicated first (a credential can
/// have more than one membership row for the same org, e.g. multiple roles),
/// then: exactly one distinct org → that slug; zero or many → an error telling
/// the user to set `TRACEVAULT_ORG_SLUG`. The empty case is not only "no
/// memberships": `/me/orgs` is also empty for an org-scoped API key (which has
/// no user), so the message names that case explicitly. Sorting makes the
/// multi-org message deterministic regardless of server ordering.
pub fn org_slug_from_slugs(slugs: &[String]) -> Result<String, String> {
    let mut unique: Vec<&str> = slugs.iter().map(String::as_str).collect();
    unique.sort_unstable();
    unique.dedup();
    match unique.as_slice() {
        [] => Err(
            "could not derive an org from this credential: it has no org membership \
             (org-scoped API keys are not supported here); set TRACEVAULT_ORG_SLUG"
                .to_string(),
        ),
        [one] => Ok((*one).to_string()),
        many => Err(format!(
            "credential belongs to multiple orgs; set TRACEVAULT_ORG_SLUG to one of: {}",
            many.join(", ")
        )),
    }
}

/// `git -C <path> remote get-url origin`, trimmed. `None` if git fails or there
/// is no origin remote. Shared by every caller that needs a checkout's origin
/// remote URL (`init`, `sync`, `status`, and this module's own
/// `resolve_path_to_binding`) — there is exactly one implementation.
pub(crate) fn git_remote_url(path: &Path) -> Option<String> {
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

/// Render a human line for a codebase resolved via `resolve_remote` /
/// `get_remote_detail`: the registered name if the server has one, else the
/// normalized URL, plus the server-side clone status. Shared by `init`
/// (after registering) and `repo status` (live tiers) so the format has
/// exactly one implementation.
pub(crate) fn codebase_line(
    name: Option<&str>,
    normalized_url: &str,
    clone_status: &str,
) -> String {
    format!(
        "codebase: {} ({})",
        name.unwrap_or(normalized_url),
        clone_status
    )
}

/// Choose the `repo status` codebase line from the available sources in
/// priority order: live remote detail, live resolve-by-URL, cached name.
/// The two live lines are pre-formatted (via `codebase_line`, so they carry
/// clone status); the cached tier is name-only (no live status).
pub(crate) fn pick_status_line(
    detail_line: Option<String>,
    resolved_line: Option<String>,
    cached_name: Option<&str>,
) -> Option<String> {
    detail_line
        .or(resolved_line)
        .or_else(|| cached_name.map(|n| format!("codebase: {n}")))
}

/// Resolve a filesystem path to a registered-repo binding: read its origin
/// remote URL and ask the server. `Ok(None)` when the path has no remote or
/// the server has no matching codebase (pre-registered-only).
pub async fn resolve_path_to_binding(
    path: &Path,
    org_slug: &str,
    client: &ApiClient,
) -> Result<Option<RepoBinding>, Box<dyn std::error::Error>> {
    let Some(git_url) = git_remote_url(path) else {
        return Ok(None);
    };
    // Resolve the CODEBASE by normalized URL (deduped), then bind to one of its
    // repos. Any linked repo works for ingest — every repo-keyed server read
    // resolves codebase-wide — so the lowest-id linked repo (deterministic) is
    // fine.
    let Some(remote) = client.resolve_remote(org_slug, &git_url).await? else {
        return Ok(None);
    };
    let repos = client.get_remote_repos(org_slug, remote.remote_id).await?;
    let Some(first) = repos.into_iter().min_by_key(|r| r.id) else {
        return Err(format!(
            "codebase {} is registered but has no tracked repo; run `tracevault init` in a checkout",
            remote.name.as_deref().unwrap_or(&remote.normalized_url)
        )
        .into());
    };
    Ok(Some(RepoBinding {
        org_slug: org_slug.to_string(),
        repo_id: first.id.to_string(),
        git_url: Some(git_url),
        remote_id: Some(remote.remote_id),
        codebase_name: remote.name,
        updated_at: chrono::Utc::now().to_rfc3339(),
    }))
}

/// Inputs for the effective-binding precedence chain. `repo_flag` and `bound`
/// are resolved by the caller (the `--path` override and the bound
/// `config.toml`, respectively); `session`/`worktree_path` come from the
/// per-session state.
pub struct ResolveInputs<'a> {
    pub repo_flag: Option<RepoBinding>,
    pub session: &'a SessionState,
    pub worktree_path: Option<&'a str>,
    pub bound: Option<RepoBinding>,
    /// Lowest-precedence, session-independent default (from `repo switch --user`
    /// / a no-session switch). Applies when nothing more specific resolves.
    pub user_default: Option<RepoBinding>,
}

/// Which precedence tier produced the [`effective_binding`] result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingSource {
    /// A `--path <path>` flag override (on `repo status`).
    RepoFlag,
    /// A subagent's per-worktree override.
    Subagent,
    /// The session's session-level active binding.
    SessionActive,
    /// A pinned `.tracevault/config.toml` (bound mode).
    Bound,
    /// A session-independent user-level default (`repo switch --user`).
    UserDefault,
}

impl std::fmt::Display for BindingSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            BindingSource::RepoFlag => "--path override",
            BindingSource::Subagent => "subagent worktree override",
            BindingSource::SessionActive => "session (repo switch)",
            BindingSource::Bound => "bound .tracevault/config.toml",
            BindingSource::UserDefault => "user default (repo switch --user)",
        };
        f.write_str(label)
    }
}

/// The binding that applies, and which tier produced it: `--path` flag →
/// subagent worktree override → session active → bound config → user
/// default → none.
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
    if let Some(b) = inputs.bound {
        return Some((b, BindingSource::Bound));
    }
    inputs.user_default.map(|b| (b, BindingSource::UserDefault))
}

/// A RepoBinding from a pinned `.tracevault/config.toml` (bound mode), if it has
/// both org_slug and repo_id. Pure — caller supplies the already-loaded config.
/// Carries through the `remote_id`/`codebase_name` `init` persisted (best-effort,
/// display-only) so bound-mode `status` can print the codebase without an extra
/// network round-trip; `git_url` stays `None` here — bound mode has no live
/// origin URL to hand back (that's the older `git_url` fallback's job, covering
/// bindings that predate `codebase_name`).
pub fn binding_from_config(config: &crate::config::TracevaultConfig) -> Option<RepoBinding> {
    Some(RepoBinding {
        org_slug: config.org_slug.clone()?,
        repo_id: config.repo_id.clone()?,
        git_url: None,
        remote_id: config.remote_id.as_deref().and_then(|s| s.parse().ok()),
        codebase_name: config.codebase_name.clone(),
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
            remote_id: None,
            codebase_name: None,
            updated_at: "t".into(),
        }
    }

    #[test]
    fn effective_binding_precedence() {
        let session = SessionState {
            active: Some(binding("session")),
            subagents: HashMap::from([("/wt/a".to_string(), binding("subagent"))]),
            ..Default::default()
        };

        // 1. repo_flag wins over everything.
        let (b, source) = effective_binding(ResolveInputs {
            repo_flag: Some(binding("flag")),
            session: &session,
            worktree_path: Some("/wt/a"),
            bound: Some(binding("bound")),
            user_default: Some(binding("userdef")),
        })
        .unwrap();
        assert_eq!(b.repo_id, "flag");
        assert_eq!(source, BindingSource::RepoFlag);

        // 2. subagent override wins over session active + bound + user default.
        let (b, source) = effective_binding(ResolveInputs {
            repo_flag: None,
            session: &session,
            worktree_path: Some("/wt/a"),
            bound: Some(binding("bound")),
            user_default: Some(binding("userdef")),
        })
        .unwrap();
        assert_eq!(b.repo_id, "subagent");
        assert_eq!(source, BindingSource::Subagent);

        // 3. session active wins over bound + user default.
        let (b, source) = effective_binding(ResolveInputs {
            repo_flag: None,
            session: &session,
            worktree_path: Some("/wt/other"),
            bound: Some(binding("bound")),
            user_default: Some(binding("userdef")),
        })
        .unwrap();
        assert_eq!(b.repo_id, "session");
        assert_eq!(source, BindingSource::SessionActive);

        // 4. bound wins over user default.
        let empty = SessionState::default();
        let (b, source) = effective_binding(ResolveInputs {
            repo_flag: None,
            session: &empty,
            worktree_path: None,
            bound: Some(binding("bound")),
            user_default: Some(binding("userdef")),
        })
        .unwrap();
        assert_eq!(b.repo_id, "bound");
        assert_eq!(source, BindingSource::Bound);

        // 5. user default is used when nothing more specific resolves.
        let (b, source) = effective_binding(ResolveInputs {
            repo_flag: None,
            session: &empty,
            worktree_path: None,
            bound: None,
            user_default: Some(binding("userdef")),
        })
        .unwrap();
        assert_eq!(b.repo_id, "userdef");
        assert_eq!(source, BindingSource::UserDefault);

        // 6. nothing at all → None.
        let got = effective_binding(ResolveInputs {
            repo_flag: None,
            session: &empty,
            worktree_path: None,
            bound: None,
            user_default: None,
        });
        assert!(got.is_none());
    }

    #[test]
    fn binding_source_display_labels() {
        assert_eq!(BindingSource::RepoFlag.to_string(), "--path override");
        assert_eq!(
            BindingSource::Subagent.to_string(),
            "subagent worktree override"
        );
        assert_eq!(
            BindingSource::SessionActive.to_string(),
            "session (repo switch)"
        );
        assert_eq!(
            BindingSource::Bound.to_string(),
            "bound .tracevault/config.toml"
        );
        assert_eq!(
            BindingSource::UserDefault.to_string(),
            "user default (repo switch --user)"
        );
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

    #[test]
    fn org_slug_from_slugs_single() {
        assert_eq!(
            org_slug_from_slugs(&["acme".to_string()]),
            Ok("acme".to_string())
        );
    }

    #[test]
    fn org_slug_from_slugs_none_errors() {
        let err = org_slug_from_slugs(&[]).unwrap_err();
        assert_eq!(
            err,
            "could not derive an org from this credential: it has no org membership \
             (org-scoped API keys are not supported here); set TRACEVAULT_ORG_SLUG"
        );
    }

    #[test]
    fn org_slug_from_slugs_multiple_lists_them() {
        let err = org_slug_from_slugs(&["acme".to_string(), "globex".to_string()]).unwrap_err();
        assert_eq!(
            err,
            "credential belongs to multiple orgs; set TRACEVAULT_ORG_SLUG to one of: acme, globex"
        );
    }

    #[test]
    fn org_slug_from_slugs_single_org_duplicated_is_not_multi() {
        // One org with two membership rows must derive, not be rejected as multi-org.
        assert_eq!(
            org_slug_from_slugs(&["acme".to_string(), "acme".to_string()]),
            Ok("acme".to_string())
        );
    }

    #[test]
    fn binding_from_config_carries_remote_id_and_codebase_name() {
        let uuid = uuid::Uuid::new_v4();
        let config = crate::config::TracevaultConfig {
            org_slug: Some("acme".into()),
            repo_id: Some("repo-1".into()),
            remote_id: Some(uuid.to_string()),
            codebase_name: Some("acme/foo".into()),
            ..Default::default()
        };
        let binding = binding_from_config(&config).expect("org_slug + repo_id present");
        assert_eq!(binding.codebase_name, Some("acme/foo".to_string()));
        assert_eq!(binding.remote_id, Some(uuid));
    }

    #[test]
    fn codebase_line_formats_name_and_status() {
        let line = codebase_line(Some("acme/foo"), "github.com/acme/foo", "ready");
        assert_eq!(line, "codebase: acme/foo (ready)");
        let line = codebase_line(None, "github.com/acme/foo", "pending");
        assert_eq!(line, "codebase: github.com/acme/foo (pending)");
    }

    #[test]
    fn pick_status_line_detail_wins_over_resolved_and_cached() {
        assert_eq!(
            pick_status_line(
                Some("codebase: a (ready)".to_string()),
                Some("codebase: b (pending)".to_string()),
                Some("c"),
            ),
            Some("codebase: a (ready)".to_string())
        );
    }

    #[test]
    fn pick_status_line_resolved_wins_over_cached() {
        assert_eq!(
            pick_status_line(None, Some("codebase: b (pending)".to_string()), Some("c"),),
            Some("codebase: b (pending)".to_string())
        );
    }

    #[test]
    fn pick_status_line_cached_only_is_name_only() {
        assert_eq!(
            pick_status_line(None, None, Some("c")),
            Some("codebase: c".to_string())
        );
    }

    #[test]
    fn pick_status_line_none_when_nothing_available() {
        assert_eq!(pick_status_line(None, None, None), None);
    }

    #[test]
    fn org_slug_from_slugs_multiple_sorted_and_deduped() {
        // Message must not depend on server ordering, and repeats collapse.
        let err = org_slug_from_slugs(&[
            "globex".to_string(),
            "acme".to_string(),
            "globex".to_string(),
        ])
        .unwrap_err();
        assert_eq!(
            err,
            "credential belongs to multiple orgs; set TRACEVAULT_ORG_SLUG to one of: acme, globex"
        );
    }
}
