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
        RepoCmd::Switch { path, session_id } => {
            switch(&path, session_id.as_deref(), project_root).await
        }
        RepoCmd::Reset { .. } => Err("`repo reset` not yet implemented (sub-plan B task 3)".into()),
    }
}

/// Resolve a path to a binding for `repo switch`, turning "unregistered" into
/// a clear error (switch should fail loudly, unlike the best-effort `--repo`
/// status override).
async fn resolve_switch_binding(
    path: &Path,
    org_slug: &str,
    client: &ApiClient,
) -> Result<RepoBinding, Box<dyn std::error::Error>> {
    resolve_path_to_binding(path, org_slug, client)
        .await?
        .ok_or_else(|| {
            format!(
                "repo at {} is not registered with TraceVault (org {org_slug}); nothing to bind",
                path.display()
            )
            .into()
        })
}

/// Apply a switch to session state (session-level active binding). Pure.
fn apply_switch(state: &mut SessionState, binding: RepoBinding) {
    state.active = Some(binding);
}

async fn switch(
    path: &str,
    session_id: Option<&str>,
    project_root: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let id = resolve_session_id(session_id)?;
    let org_slug = org_slug_for(project_root)
        .ok_or("no org configured: set TRACEVAULT_ORG_SLUG, log in, or run inside a bound repo")?;

    let (server_url, token) = resolve_credentials(project_root);
    let server_url = server_url.ok_or("not logged in / no server_url; run `tracevault login`")?;
    let client = ApiClient::new(&server_url, token.as_deref());

    let binding = resolve_switch_binding(Path::new(path), &org_slug, &client).await?;

    // NOTE: subagent worktree-override writes are handled in sub-plan C.
    let mut state = session_state::load(&id);
    apply_switch(&mut state, binding.clone());
    session_state::save(&id, &state)?;

    let repo_uuid: uuid::Uuid = binding.repo_id.parse().map_err(|e| {
        format!(
            "invalid repo id {:?} returned by server: {e}",
            binding.repo_id
        )
    })?;
    let policies = client
        .get_agent_instructions(&binding.org_slug, &repo_uuid)
        .await?;
    println!("{}", policies.content);
    Ok(())
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
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

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

    fn binding(id: &str) -> RepoBinding {
        RepoBinding {
            org_slug: "org".into(),
            repo_id: id.into(),
            git_url: None,
            updated_at: "t".into(),
        }
    }

    #[test]
    fn apply_switch_sets_session_active() {
        let mut state = SessionState::default();
        apply_switch(&mut state, binding("new-repo"));
        assert_eq!(state.active.unwrap().repo_id, "new-repo");
    }

    #[test]
    fn apply_switch_overwrites_existing_active() {
        let mut state = SessionState {
            active: Some(binding("old-repo")),
            subagents: Default::default(),
        };
        apply_switch(&mut state, binding("new-repo"));
        assert_eq!(state.active.unwrap().repo_id, "new-repo");
    }

    fn add_origin_remote(dir: &std::path::Path, url: &str) {
        let ok = std::process::Command::new("git")
            .args(["-C", &dir.to_string_lossy(), "remote", "add", "origin", url])
            .status()
            .expect("git remote add failed")
            .success();
        assert!(ok, "git remote add must succeed");
    }

    /// Spawn a one-shot raw-HTTP server that returns `response` to the first
    /// request it accepts (mirrors `tests/resolve_repo_test.rs`).
    fn spawn_once(response: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                let _ = reader.read_line(&mut request_line);
                let mut stream = reader.into_inner();
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn resolve_switch_binding_ok_for_registered_repo() {
        let tmp = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(tmp.path());
        add_origin_remote(tmp.path(), "git@github.com:org/repo.git");

        let repo_id = "44000761-8d22-4256-bd2c-27a0ba278c6f";
        let body = format!(r#"{{"repo_id":"{repo_id}"}}"#);
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let base = spawn_once(Box::leak(resp.into_boxed_str()));
        let client = ApiClient::new(&base, Some("tok"));

        let got = resolve_switch_binding(tmp.path(), "org", &client)
            .await
            .expect("expected Ok binding");
        assert_eq!(got.repo_id, repo_id);
        assert_eq!(got.org_slug, "org");
    }

    #[tokio::test]
    async fn resolve_switch_binding_errors_for_unregistered_repo() {
        let tmp = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(tmp.path());
        add_origin_remote(tmp.path(), "git@github.com:org/unregistered.git");

        let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let base = spawn_once(resp);
        let client = ApiClient::new(&base, Some("tok"));

        let err = resolve_switch_binding(tmp.path(), "org", &client)
            .await
            .expect_err("expected Err for unregistered repo");
        assert!(
            err.to_string().contains("not registered"),
            "unexpected error message: {err}"
        );
    }
}
