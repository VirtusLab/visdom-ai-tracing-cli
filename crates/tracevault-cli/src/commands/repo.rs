//! `tracevault repo` — workspace/detached-mode repo binding commands.

use std::path::Path;

use crate::api_client::{resolve_credentials, ApiClient};
use crate::resolution::{
    binding_from_config, effective_binding, org_slug_for, resolve_path_to_binding, BindingSource,
    ResolveInputs,
};
use crate::session_state::{self, RepoBinding, SessionState};

/// Sub-actions for `tracevault repo` (workspace/detached mode).
#[derive(clap::Subcommand)]
pub enum RepoCmd {
    /// Bind tracing to a registered repo for the current session and print its
    /// policies. Give a checkout <path> OR --name; exactly one is required.
    #[command(group(clap::ArgGroup::new("target").required(true).multiple(false)))]
    Switch {
        /// Path to a checkout of a registered repo (resolves via its git origin remote).
        #[arg(group = "target")]
        path: Option<String>,
        /// Bind by the repo's registered name instead of a checkout path (no checkout needed).
        #[arg(long, group = "target")]
        name: Option<String>,
        /// Session to target; defaults to $TRACEVAULT_SESSION_ID.
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Show the repo the current session is bound to.
    Status {
        /// Session to target; defaults to $TRACEVAULT_SESSION_ID.
        #[arg(long)]
        session_id: Option<String>,
        /// One-off: resolve this checkout path instead of the session binding.
        #[arg(long)]
        path: Option<String>,
    },
    /// Clear the current session's binding.
    Reset {
        /// Session to target; defaults to $TRACEVAULT_SESSION_ID.
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

pub async fn run(
    cmd: RepoCmd,
    project_root: &Path,
    cwd: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        RepoCmd::Status { session_id, path } => {
            status(session_id.as_deref(), path.as_deref(), project_root, cwd).await
        }
        RepoCmd::Switch {
            path,
            name,
            session_id,
        } => {
            switch(
                path.as_deref(),
                name.as_deref(),
                session_id.as_deref(),
                project_root,
            )
            .await
        }
        RepoCmd::Reset { session_id } => reset(session_id.as_deref()),
    }
}

/// Resolve a path to a binding for `repo switch`, turning "unregistered" into
/// a clear error (switch should fail loudly, unlike the best-effort `--path`
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

/// Resolve a registered repo by its NAME (no git checkout) to a binding.
/// Exact, case-sensitive match on `repos.name`; on no match, errors listing
/// the available names so a typo self-corrects.
async fn resolve_name_to_binding(
    name: &str,
    org_slug: &str,
    client: &ApiClient,
) -> Result<RepoBinding, Box<dyn std::error::Error>> {
    let repos = client.list_repos(org_slug).await?;
    match repos.iter().find(|r| r.name == name) {
        Some(repo) => Ok(RepoBinding {
            org_slug: org_slug.to_string(),
            repo_id: repo.id.to_string(),
            git_url: repo.github_url.clone(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        }),
        None => {
            let mut names: Vec<&str> = repos.iter().map(|r| r.name.as_str()).collect();
            names.sort_unstable();
            Err(format!(
                "no repo named '{name}' in org {org_slug} (available: {})",
                names.join(", ")
            )
            .into())
        }
    }
}

/// Apply a switch to session state (session-level active binding). Pure.
fn apply_switch(state: &mut SessionState, binding: RepoBinding) {
    state.active = Some(binding);
}

/// Clear the session's workspace binding (session-level reset). Pure.
fn apply_reset(state: &mut SessionState) {
    *state = SessionState::default();
}

fn reset(session_id: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let id = resolve_session_id(session_id)?;
    let mut state = session_state::load(&id);
    apply_reset(&mut state);
    session_state::save(&id, &state)?;
    // NOTE: subagent worktree-scoped reset-to-parent is handled in sub-plan C.
    println!("cleared workspace repo binding for session {id}");
    Ok(())
}

/// Which target `repo switch` should resolve to a binding: a checkout path
/// or a registered repo name. Exactly one of `path`/`name` must be given;
/// see `switch_target`.
enum SwitchTarget<'a> {
    Path(&'a str),
    Name(&'a str),
}

/// Pure exactly-one-of guard for `repo switch`'s `<path>` / `--name`
/// arguments. Kept separate from `switch` so it can be unit-tested without
/// spinning up a server or session state (also a safety net if the clap
/// `ArgGroup` on the `Switch` variant is ever loosened).
fn switch_target<'a>(
    path: Option<&'a str>,
    name: Option<&'a str>,
) -> Result<SwitchTarget<'a>, Box<dyn std::error::Error>> {
    match (path, name) {
        (Some(p), None) => Ok(SwitchTarget::Path(p)),
        (None, Some(n)) => Ok(SwitchTarget::Name(n)),
        _ => Err("provide exactly one of <path> or --name".into()),
    }
}

async fn switch(
    path: Option<&str>,
    name: Option<&str>,
    session_id: Option<&str>,
    project_root: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let id = resolve_session_id(session_id)?;
    let org_slug = org_slug_for(project_root).ok_or(
        "no org configured: set TRACEVAULT_ORG_SLUG, log in, or run inside a bound repo checkout",
    )?;

    let (server_url, token) = resolve_credentials(project_root);
    let server_url = server_url.ok_or("not logged in / no server_url; run `tracevault login`")?;
    let client = ApiClient::new(&server_url, token.as_deref());

    let binding = match switch_target(path, name)? {
        SwitchTarget::Path(p) => resolve_switch_binding(Path::new(p), &org_slug, &client).await?,
        SwitchTarget::Name(n) => resolve_name_to_binding(n, &org_slug, &client).await?,
    };

    // Validate the repo id BEFORE persisting anything: a malformed repo_id
    // must not leave a persisted session binding behind.
    let repo_uuid = binding.repo_id.parse::<uuid::Uuid>().map_err(|e| {
        format!(
            "server returned an invalid repo id {:?}: {e}",
            binding.repo_id
        )
    })?;

    // NOTE: subagent worktree-override writes are handled in sub-plan C.
    let mut state = session_state::load(&id);
    apply_switch(&mut state, binding.clone());
    session_state::save(&id, &state)?;

    println!(
        "bound session {id} to repo {} (org {})",
        binding.repo_id, binding.org_slug
    );

    // The binding was already saved above — that's the primary effect of
    // `switch`. A failure to fetch policies afterward shouldn't make the
    // whole command report as an error.
    match client
        .get_agent_instructions(&binding.org_slug, &repo_uuid)
        .await
    {
        Ok(policies) => println!("{}", policies.content),
        Err(e) => eprintln!("warning: bound, but could not fetch policies: {e}"),
    }
    Ok(())
}

/// Resolve a `--path <path>` override into a live `RepoBinding`, if possible.
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
            "--path {path}: no org slug configured (set TRACEVAULT_ORG_SLUG, log in, or bind \
             the repo); showing binding without the --path override"
        );
        return None;
    };

    let (server_url, token) = resolve_credentials(project_root);
    let Some(server_url) = server_url else {
        eprintln!(
            "--path {path}: no server URL configured (run `tracevault login`); showing binding \
             without the --path override"
        );
        return None;
    };

    let client = ApiClient::new(&server_url, token.as_deref());
    match resolve_path_to_binding(Path::new(path), &org_slug, &client).await {
        Ok(Some(binding)) => Some(binding),
        Ok(None) => {
            eprintln!(
                "--path {path}: no registered repo found for this path's git remote; showing \
                 binding without the --path override"
            );
            None
        }
        Err(e) => {
            eprintln!(
                "--path {path}: failed to resolve ({e}); showing binding without the --path \
                 override"
            );
            None
        }
    }
}

/// Pure formatter for `repo status`'s output: which repo is bound, and via
/// which precedence tier. Kept separate from I/O so it can be unit-tested.
fn format_status(binding: Option<(&RepoBinding, BindingSource)>) -> String {
    match binding {
        Some((b, source)) => format!(
            "bound to repo {} (org {}) via {source}",
            b.repo_id, b.org_slug
        ),
        None => {
            "not bound to any repo (workspace mode; run `tracevault repo switch <path>|--name <project>`)".into()
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
    // warn and fall back to an empty SessionState rather than silently
    // reporting "not bound" without the user knowing why.
    let session = match resolve_session_id(session_id) {
        Ok(id) => session_state::load(&id),
        Err(_) => {
            eprintln!(
                "warning: no session id (pass --session-id or set TRACEVAULT_SESSION_ID); \
                 showing binding without session context"
            );
            SessionState::default()
        }
    };
    let worktree = crate::paths::worktree_toplevel(cwd);
    let bound = crate::config::TracevaultConfig::load(project_root)
        .as_ref()
        .and_then(binding_from_config);

    // A --path override on status resolves live (needs org_slug + server).
    let repo_flag = resolve_repo_flag(repo_flag_path, project_root).await;

    let effective = effective_binding(ResolveInputs {
        repo_flag,
        session: &session,
        worktree_path: Some(&worktree),
        bound,
    });
    println!(
        "{}",
        format_status(effective.as_ref().map(|(b, s)| (b, *s)))
    );
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

    #[test]
    fn switch_target_rejects_neither() {
        assert!(switch_target(None, None).is_err());
    }

    #[test]
    fn switch_target_rejects_both() {
        assert!(switch_target(Some("/p"), Some("x")).is_err());
    }

    #[test]
    fn switch_target_accepts_path_only() {
        assert!(matches!(
            switch_target(Some("/p"), None),
            Ok(SwitchTarget::Path("/p"))
        ));
    }

    #[test]
    fn switch_target_accepts_name_only() {
        assert!(matches!(
            switch_target(None, Some("proj")),
            Ok(SwitchTarget::Name("proj"))
        ));
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
    fn apply_reset_clears_active_and_subagents() {
        let mut state = SessionState {
            active: Some(binding("r")),
            subagents: std::collections::HashMap::from([("/wt/a".to_string(), binding("r2"))]),
            ..Default::default()
        };
        apply_reset(&mut state);
        assert_eq!(state, SessionState::default());
    }

    #[test]
    fn apply_switch_overwrites_existing_active() {
        let mut state = SessionState {
            active: Some(binding("old-repo")),
            subagents: Default::default(),
            ..Default::default()
        };
        apply_switch(&mut state, binding("new-repo"));
        assert_eq!(state.active.unwrap().repo_id, "new-repo");
    }

    #[test]
    fn format_status_none() {
        assert_eq!(
            format_status(None),
            "not bound to any repo (workspace mode; run `tracevault repo switch <path>|--name <project>`)"
        );
    }

    #[test]
    fn format_status_repo_flag() {
        let b = binding("r1");
        assert_eq!(
            format_status(Some((&b, BindingSource::RepoFlag))),
            "bound to repo r1 (org org) via --path override"
        );
    }

    #[test]
    fn format_status_subagent() {
        let b = binding("r2");
        assert_eq!(
            format_status(Some((&b, BindingSource::Subagent))),
            "bound to repo r2 (org org) via subagent worktree override"
        );
    }

    #[test]
    fn format_status_session_active() {
        let b = binding("r3");
        assert_eq!(
            format_status(Some((&b, BindingSource::SessionActive))),
            "bound to repo r3 (org org) via session (repo switch)"
        );
    }

    #[test]
    fn format_status_bound() {
        let b = binding("r4");
        assert_eq!(
            format_status(Some((&b, BindingSource::Bound))),
            "bound to repo r4 (org org) via bound .tracevault/config.toml"
        );
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

    #[tokio::test]
    async fn resolve_name_to_binding_finds_by_name() {
        let body = r#"[{"id":"11111111-1111-4111-8111-111111111111","name":"visdom-orchestrator","github_url":null,"clone_status":null},{"id":"22222222-2222-4222-8222-222222222222","name":"visdom-web","github_url":"https://github.com/o/web.git","clone_status":null}]"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let base = spawn_once(Box::leak(resp.into_boxed_str()));
        let client = ApiClient::new(&base, Some("tok"));
        let b = resolve_name_to_binding("visdom-orchestrator", "acme", &client)
            .await
            .expect("expected Ok binding");
        assert_eq!(b.repo_id, "11111111-1111-4111-8111-111111111111");
        assert_eq!(b.org_slug, "acme");
    }

    #[tokio::test]
    async fn resolve_name_to_binding_errors_when_absent_listing_names() {
        let body = r#"[{"id":"22222222-2222-4222-8222-222222222222","name":"visdom-web","github_url":null,"clone_status":null}]"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let base = spawn_once(Box::leak(resp.into_boxed_str()));
        let client = ApiClient::new(&base, Some("tok"));
        let err = resolve_name_to_binding("visdom-orch", "acme", &client)
            .await
            .expect_err("expected Err for missing name")
            .to_string();
        assert!(err.contains("no repo named"), "got: {err}");
        assert!(
            err.contains("visdom-web"),
            "error should list available names, got: {err}"
        );
    }
}
