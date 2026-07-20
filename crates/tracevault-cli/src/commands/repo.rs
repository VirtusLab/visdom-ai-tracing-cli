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
        /// Write a session-independent user-level default instead of a session
        /// binding. Implied when no session id is available.
        #[arg(long)]
        user: bool,
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
    /// Clear the current session's binding, or (with --user) the user-level default.
    Reset {
        /// Clear the user-level default instead of the session binding.
        #[arg(long)]
        user: bool,
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
            user,
            session_id,
        } => {
            switch(
                path.as_deref(),
                name.as_deref(),
                user,
                session_id.as_deref(),
                project_root,
            )
            .await
        }
        RepoCmd::Reset { user, session_id } => reset(user, session_id.as_deref()),
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

/// Apply a switch to session state (session-level active binding). Pure.
fn apply_switch(state: &mut SessionState, binding: RepoBinding) {
    state.active = Some(binding);
}

/// Clear the session's workspace binding (session-level reset). Pure.
fn apply_reset(state: &mut SessionState) {
    *state = SessionState::default();
}

fn reset(user: bool, session_id: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    if user {
        crate::user_default::clear()?;
        println!("cleared user-level default repo binding");
        return Ok(());
    }
    let id = resolve_session_id(session_id)?;
    let mut state = session_state::load(&id);
    apply_reset(&mut state);
    session_state::save(&id, &state)?;
    // NOTE: subagent worktree-scoped reset-to-parent is handled in sub-plan C.
    println!("cleared workspace repo binding for session {id}");
    Ok(())
}

/// Which target `repo switch` should resolve to a binding: a checkout path.
/// `--name` binding is deprecated (see `switch_target`) and no longer
/// produces a variant here.
#[derive(Debug, PartialEq, Eq)]
enum SwitchTarget<'a> {
    Path(&'a str),
}

/// Pure guard for `repo switch`'s `<path>` / `--name` arguments. Kept
/// separate from `switch` so it can be unit-tested without spinning up a
/// server or session state (also a safety net if the clap `ArgGroup` on the
/// `Switch` variant is ever loosened). `--name` binding is hard-deprecated:
/// a name-only invocation errors rather than resolving by name, directing
/// callers to bind by checkout path so the codebase is resolved from its git
/// remote.
fn switch_target<'a>(
    path: Option<&'a str>,
    name: Option<&'a str>,
) -> Result<SwitchTarget<'a>, Box<dyn std::error::Error>> {
    match (path, name) {
        (Some(p), _) if !p.trim().is_empty() => Ok(SwitchTarget::Path(p)),
        (Some(_), _) => Err("repo path must not be empty".into()),
        (None, Some(_)) => Err(
            "`--name` binding is deprecated and no longer supported; bind by checkout path \
             (`tracevault repo switch <path>`) so the codebase is resolved from its git remote"
                .into(),
        ),
        (None, None) => {
            Err("provide a checkout path to bind (`tracevault repo switch <path>`)".into())
        }
    }
}

/// Where a `repo switch` should persist its binding: a specific session, or the
/// session-independent user-level default.
#[derive(Debug)]
enum SwitchDest {
    Session(String),
    UserDefault,
}

/// Pure choice of write target for `repo switch`: `--user` (or the absence of
/// any session id) selects the user-level default; otherwise the resolved
/// session id selects a session binding. Kept pure so it's unit-testable.
fn switch_destination(user: bool, session_id: Option<String>) -> SwitchDest {
    if user {
        return SwitchDest::UserDefault;
    }
    match session_id {
        Some(id) => SwitchDest::Session(id),
        None => SwitchDest::UserDefault,
    }
}

async fn switch(
    path: Option<&str>,
    name: Option<&str>,
    user: bool,
    session_id: Option<&str>,
    project_root: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    // Validate the target argument before any network work, so a bad/empty
    // path or name surfaces as an argument error rather than an unrelated org
    // or network error from the derivation step below.
    let target = switch_target(path, name)?;

    // Build the client first: deriving an org from the credential needs it.
    let (server_url, token) = resolve_credentials(project_root);
    let server_url = server_url
        .ok_or("no server URL configured: set TRACEVAULT_SERVER_URL or run `tracevault login`")?;
    let client = ApiClient::new(&server_url, token.as_deref());

    // Locally-configured org wins (env / credentials / bound config). Only when
    // none is set do we ask the server which org this credential belongs to —
    // this is what lets a service-account key bind a repo before checkout.
    let org_slug = match org_slug_for(project_root) {
        Some(s) => s,
        None => {
            let orgs = client.list_my_orgs().await.map_err(|e| {
                format!(
                    "no org configured and could not derive it from your credential ({e}); \
                     set TRACEVAULT_ORG_SLUG, log in, or run inside a bound repo checkout"
                )
            })?;
            let slugs: Vec<String> = orgs.into_iter().map(|o| o.org_name).collect();
            crate::resolution::org_slug_from_slugs(&slugs)?
        }
    };

    let binding = match target {
        SwitchTarget::Path(p) => resolve_switch_binding(Path::new(p), &org_slug, &client).await?,
    };

    // Validate the repo id BEFORE persisting anything: a malformed repo_id must
    // not leave a persisted binding behind.
    let repo_uuid = binding.repo_id.parse::<uuid::Uuid>().map_err(|e| {
        format!(
            "server returned an invalid repo id {:?}: {e}",
            binding.repo_id
        )
    })?;

    // A session id is optional now: with one (and no --user) we bind that
    // session; without one (or with --user) we set the user-level default,
    // which any new session inherits. This is what lets a container bind its
    // repo before Claude — and its session — exists.
    let session = resolve_session_id(session_id).ok();
    match switch_destination(user, session) {
        SwitchDest::Session(id) => {
            let mut state = session_state::load(&id);
            apply_switch(&mut state, binding.clone());
            session_state::save(&id, &state)?;
            println!(
                "bound session {id} to repo {} (org {})",
                binding.repo_id, binding.org_slug
            );
        }
        SwitchDest::UserDefault => {
            crate::user_default::save(&binding)?;
            println!(
                "set user-level default repo {} (org {}); applies to new sessions without their own binding (the current session, if any, is unchanged — omit --user to bind this session)",
                binding.repo_id, binding.org_slug
            );
        }
    }

    // Best-effort policy fetch (applies to either destination). A failure here
    // must not make the whole command report as an error.
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
            "not bound to any repo (workspace mode; run `tracevault repo switch <path>` or `tracevault repo switch --name <project>`)".into()
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

    let user_default = crate::user_default::load();

    let effective = effective_binding(ResolveInputs {
        repo_flag,
        session: &session,
        worktree_path: Some(&worktree),
        bound,
        user_default: user_default.clone(),
    });
    println!(
        "{}",
        format_status(effective.as_ref().map(|(b, s)| (b, *s)))
    );

    // Surface which codebase the effective binding belongs to. Best-effort:
    // a resolved `codebase_name` prints directly; otherwise (older bindings
    // without one) fall back to a live `resolve_remote` lookup by the
    // binding's git_url. A failure here must not fail `status`.
    if let Some((binding, _)) = &effective {
        if let Some(cb) = &binding.codebase_name {
            println!("codebase: {cb}");
        } else if let Some(git_url) = &binding.git_url {
            let (server_url, token) = resolve_credentials(project_root);
            if let Some(server_url) = server_url {
                let client = ApiClient::new(&server_url, token.as_deref());
                if let Ok(Some(remote)) = client.resolve_remote(&binding.org_slug, git_url).await {
                    println!(
                        "codebase: {} ({})",
                        remote.name.as_deref().unwrap_or(&remote.normalized_url),
                        remote.clone_status
                    );
                }
            }
        }
    }

    // Surface a configured user default even when a more specific tier won, so
    // it's discoverable (avoids double-printing when it IS the winning tier).
    let default_is_effective = matches!(
        effective.as_ref().map(|(_, s)| *s),
        Some(BindingSource::UserDefault)
    );
    if let Some(ud) = &user_default {
        if !default_is_effective {
            println!("user default: repo {} (org {})", ud.repo_id, ud.org_slug);
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

    #[test]
    fn switch_destination_user_flag_forces_user_default() {
        assert!(matches!(
            switch_destination(true, Some("sess-1".to_string())),
            SwitchDest::UserDefault
        ));
        assert!(matches!(
            switch_destination(true, None),
            SwitchDest::UserDefault
        ));
    }

    #[test]
    fn switch_destination_session_when_present_and_no_user_flag() {
        assert!(matches!(
            switch_destination(false, Some("sess-1".to_string())),
            SwitchDest::Session(id) if id == "sess-1"
        ));
    }

    #[test]
    fn switch_destination_user_default_when_no_session() {
        assert!(matches!(
            switch_destination(false, None),
            SwitchDest::UserDefault
        ));
    }

    #[test]
    fn switch_target_rejects_neither() {
        assert!(switch_target(None, None).is_err());
    }

    // The clap `ArgGroup` on `Switch` already prevents both being given
    // together, but as a safety net at this level path wins: a non-empty
    // path resolves the target regardless of a (deprecated) --name.
    #[test]
    fn switch_target_path_wins_when_both_given() {
        assert!(matches!(
            switch_target(Some("/p"), Some("x")),
            Ok(SwitchTarget::Path("/p"))
        ));
    }

    #[test]
    fn switch_target_accepts_path_only() {
        assert!(matches!(
            switch_target(Some("/p"), None),
            Ok(SwitchTarget::Path("/p"))
        ));
    }

    // --name now warns and does NOT resolve by name: switch_target maps a name-only
    // invocation to a deprecation error directing to path binding (no list_repos call).
    #[test]
    fn switch_target_name_is_deprecated() {
        let err = switch_target(None, Some("some-repo"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("deprecated"), "got: {err}");
        assert!(err.contains("path"), "should direct to path binding: {err}");
    }

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
            "not bound to any repo (workspace mode; run `tracevault repo switch <path>` or `tracevault repo switch --name <project>`)"
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

    #[test]
    fn format_status_user_default() {
        let b = binding("r5");
        assert_eq!(
            format_status(Some((&b, BindingSource::UserDefault))),
            "bound to repo r5 (org org) via user default (repo switch --user)"
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

    /// Format a raw HTTP/1.1 200 response with a correct `Content-Length` for
    /// a JSON `body`.
    fn http_200(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    }

    /// Generalizes `spawn_once` to a listener that answers each of `responses`
    /// in order, one per accepted connection (the resolve-by-remote flow makes
    /// two requests: `resolve_remote` then `get_remote_repos`).
    fn spawn_n(responses: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for response in responses {
                let response: &'static str = Box::leak(response.into_boxed_str());
                if let Ok((stream, _)) = listener.accept() {
                    let mut reader = BufReader::new(stream);
                    let mut request_line = String::new();
                    let _ = reader.read_line(&mut request_line);
                    let mut stream = reader.into_inner();
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn resolve_switch_binding_ok_for_registered_repo() {
        // Path resolution now goes through the codebase (remote) resolver:
        // resolve_remote finds the codebase, then get_remote_repos returns its
        // linked repos — the switch binds to the first one.
        let tmp = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(tmp.path());
        add_origin_remote(tmp.path(), "git@github.com:org/repo.git");

        let remote_id = "44000761-8d22-4256-bd2c-27a0ba278c6f";
        let repo_id = "11111111-1111-4111-8111-111111111111";
        let remote = format!(
            r#"{{"remote_id":"{remote_id}","name":"repo","normalized_url":"github.com/org/repo","clone_status":"ready"}}"#
        );
        let detail = format!(
            r#"{{"id":"{remote_id}","name":"repo","normalized_url":"github.com/org/repo","clone_url":"https://github.com/org/repo.git","clone_status":"ready","clone_error":null,"last_fetched_at":null,"repo_count":1,"created_at":"2026-01-01T00:00:00Z","repos":[{{"id":"{repo_id}","name":"repo"}}]}}"#
        );
        let base = spawn_n(vec![http_200(&remote), http_200(&detail)]);
        let client = ApiClient::new(&base, Some("tok"));

        let got = resolve_switch_binding(tmp.path(), "org", &client)
            .await
            .expect("expected Ok binding");
        assert_eq!(got.repo_id, repo_id);
        assert_eq!(got.org_slug, "org");
        assert_eq!(got.remote_id, Some(remote_id.parse().unwrap()));
        assert_eq!(got.codebase_name.as_deref(), Some("repo"));
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

    #[test]
    fn switch_target_rejects_empty_name() {
        // `--name` is deprecated outright, so there's no separate empty-name
        // branch anymore — an empty name still hits the deprecation error.
        assert!(switch_target(None, Some("")).is_err());
        assert!(switch_target(None, Some("   ")).is_err());
        let err = switch_target(None, Some(" ")).unwrap_err().to_string();
        assert!(err.contains("deprecated"), "got: {err}");
    }

    #[test]
    fn switch_target_rejects_empty_path() {
        assert!(switch_target(Some(""), None).is_err());
        assert!(switch_target(Some("   "), None).is_err());
        let err = switch_target(Some(" "), None).unwrap_err().to_string();
        assert!(err.contains("must not be empty"), "got: {err}");
    }

    // repo switch resolves by NORMALIZED url: origin spelling differs from what
    // was registered, but resolve_remote matches, and we bind to a linked repo.
    #[tokio::test]
    async fn resolve_path_binds_via_remote_across_url_spelling() {
        let tmp = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(tmp.path());
        add_origin_remote(tmp.path(), "git@github.com:o/x.git"); // scp spelling
        let remote = r#"{"remote_id":"44000761-8d22-4256-bd2c-27a0ba278c6f","name":"x","normalized_url":"github.com/o/x","clone_status":"ready"}"#;
        let detail = r#"{"id":"44000761-8d22-4256-bd2c-27a0ba278c6f","name":"x","normalized_url":"github.com/o/x","clone_url":"https://github.com/o/x.git","clone_status":"ready","clone_error":null,"last_fetched_at":null,"repo_count":1,"created_at":"2026-01-01T00:00:00Z","repos":[{"id":"11111111-1111-4111-8111-111111111111","name":"x"}]}"#;
        let base = spawn_n(vec![http_200(remote), http_200(detail)]);
        let client = ApiClient::new(&base, Some("tok"));
        let b = resolve_path_to_binding(tmp.path(), "org", &client)
            .await
            .unwrap()
            .expect("Some binding");
        assert_eq!(b.repo_id, "11111111-1111-4111-8111-111111111111");
        assert_eq!(
            b.remote_id,
            Some("44000761-8d22-4256-bd2c-27a0ba278c6f".parse().unwrap())
        );
        assert_eq!(b.codebase_name.as_deref(), Some("x"));
    }

    // A tracked codebase with no linked repos (bare remote) → explicit error.
    #[tokio::test]
    async fn resolve_path_bare_remote_errors() {
        let tmp = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(tmp.path());
        add_origin_remote(tmp.path(), "git@github.com:o/x.git");
        let remote = r#"{"remote_id":"44000761-8d22-4256-bd2c-27a0ba278c6f","name":"x","normalized_url":"github.com/o/x","clone_status":"pending"}"#;
        let detail = r#"{"id":"44000761-8d22-4256-bd2c-27a0ba278c6f","name":"x","normalized_url":"github.com/o/x","clone_url":"https://github.com/o/x.git","clone_status":"pending","clone_error":null,"last_fetched_at":null,"repo_count":0,"created_at":"2026-01-01T00:00:00Z","repos":[]}"#;
        let base = spawn_n(vec![http_200(remote), http_200(detail)]);
        let client = ApiClient::new(&base, Some("tok"));
        let err = resolve_path_to_binding(tmp.path(), "org", &client)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no tracked repo"), "got: {err}");
    }
}
