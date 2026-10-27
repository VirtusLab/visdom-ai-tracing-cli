//! `tracevault repo` — workspace/detached-mode repo binding commands.

use std::path::Path;

use crate::api_client::{resolve_client, ApiClient};
use crate::credentials::resolve_credentials;
use crate::resolution::{
    binding_from_config, checkout_config_repo_id, effective_binding, resolve_path_to_binding,
    resolve_path_to_codebase, BindingSource, RepoChoice, ResolveInputs, ResolvedCodebase,
};
use crate::session_state::{self, RepoBinding, SessionState};

/// Sub-actions for `tracevault repo` (workspace/detached mode).
#[derive(clap::Subcommand)]
pub enum RepoCmd {
    /// Bind tracing to a registered repo for the current session and print its
    /// policies. Give a checkout <path> — `--name` is deprecated and always
    /// errors, telling you to bind by path instead.
    #[command(group(clap::ArgGroup::new("target").required(true).multiple(false)))]
    Switch {
        /// Path to a checkout of a registered repo (resolves via its git origin remote).
        #[arg(group = "target")]
        path: Option<String>,
        /// Deprecated: name-only binding is no longer supported and always
        /// errors; bind by checkout path instead (`tracevault repo switch <path>`).
        #[arg(long, group = "target")]
        name: Option<String>,
        /// Which of the codebase's repo records to bind, when <path>'s
        /// codebase has several. Without it, switch keeps the repo already in
        /// use (the session's current binding, else a checkout's
        /// `.tracevault/config.toml`) and errors if none of them is.
        #[arg(long, requires = "path")]
        repo_id: Option<uuid::Uuid>,
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
            repo_id,
            user,
            session_id,
        } => {
            switch(
                path.as_deref(),
                name.as_deref(),
                repo_id,
                user,
                session_id.as_deref(),
                project_root,
            )
            .await
        }
        RepoCmd::Reset { user, session_id } => reset(user, session_id.as_deref()),
    }
}

/// Resolve a path to its codebase for `repo switch`, turning "unregistered"
/// into a clear error (switch should fail loudly, unlike the best-effort
/// `--path` status override).
async fn resolve_switch_codebase(
    path: &Path,
    client: &ApiClient,
) -> Result<ResolvedCodebase, Box<dyn std::error::Error>> {
    resolve_path_to_codebase(path, client)
        .await?
        .ok_or_else(|| {
            format!(
                "repo at {} is not registered with TraceVault; nothing to bind",
                path.display()
            )
            .into()
        })
}

/// A repo id `repo switch` should keep if the codebase links it, and where it
/// comes from (for the message saying which one was kept).
struct InUse {
    repo_id: uuid::Uuid,
    source: String,
}

/// Which of the codebase's repos `repo switch` binds. Pure.
///
/// An explicit `--repo-id` wins, if it is one of the codebase's repos. Else
/// the only repo; else the first `in_use` repo the codebase links — the
/// destination's current binding comes first, so a session is never moved to
/// another repo of the same codebase mid-flight (that splits it across two
/// session rows on the server, VIS-1018). Else an error listing the repos.
/// `Ok` carries the repo id and, when several repos were possible, which
/// `in_use` source decided.
fn pick_switch_repo(
    codebase: &ResolvedCodebase,
    explicit: Option<uuid::Uuid>,
    in_use: &[InUse],
) -> Result<(uuid::Uuid, Option<String>), Box<dyn std::error::Error>> {
    if let Some(id) = explicit {
        if codebase.repos.iter().any(|r| r.id == id) {
            return Ok((id, None));
        }
        return Err(format!(
            "repo {id} is not a repo of codebase {}; its repos are: {}",
            codebase.display_name(),
            codebase.repo_list()
        )
        .into());
    }
    let preferred: Vec<uuid::Uuid> = in_use.iter().map(|u| u.repo_id).collect();
    match codebase.choose(&preferred) {
        RepoChoice::Only(id) => Ok((id, None)),
        RepoChoice::Preferred(id, i) => Ok((id, Some(in_use[i].source.clone()))),
        RepoChoice::Ambiguous => Err(format!(
            "codebase {} has {} repo records: {}. None of them is this binding's current \
             repo or a checkout's .tracevault/config.toml repo, so switch will not guess; \
             choose one with `tracevault repo switch <path> --repo-id <id>`",
            codebase.display_name(),
            codebase.repos.len(),
            codebase.repo_list()
        )
        .into()),
    }
}

/// Apply a switch to session state (session-level active binding). Pure.
fn apply_switch(state: &mut SessionState, binding: RepoBinding) {
    state.active = Some(binding);
}

/// Clear the session's workspace binding (session-level reset), and keep it
/// cleared: the hooks would otherwise bind the session again from its
/// checkout's remote on the next event. Pure.
fn apply_reset(state: &mut SessionState) {
    *state = SessionState {
        auto_bind_off: true,
        ..SessionState::default()
    };
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
    println!(
        "cleared workspace repo binding for session {id}; it stays unbound (no automatic \
         binding from the checkout's remote) until `tracevault repo switch`"
    );
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

/// The repos `repo switch` should keep, most preferred first: what the
/// destination currently resolves to (for a session, exactly what its hooks
/// stream to: session binding → this checkout's config.toml → user default;
/// for `--user`, the current user default), then the target checkout's own
/// config.toml, then this checkout's. Ids that are not UUIDs are skipped.
fn in_use_repos(dest: &SwitchDest, target: &Path, project_root: &Path) -> Vec<InUse> {
    let bound = crate::config::TracevaultConfig::load(project_root)
        .as_ref()
        .and_then(binding_from_config);
    let user_default = crate::user_default::load();
    let current = match dest {
        SwitchDest::Session(id) => effective_binding(ResolveInputs {
            repo_flag: None,
            session: &session_state::load(id),
            worktree_path: None,
            bound,
            user_default,
        })
        .map(|(b, source)| (b, source.to_string())),
        SwitchDest::UserDefault => {
            user_default.map(|b| (b, BindingSource::UserDefault.to_string()))
        }
    };
    let mut in_use: Vec<InUse> = current
        .and_then(|(b, source)| {
            Some(InUse {
                repo_id: b.repo_id.parse().ok()?,
                source: format!("the current binding ({source})"),
            })
        })
        .into_iter()
        .collect();
    for checkout in [target, project_root] {
        if let Some(repo_id) = checkout_config_repo_id(checkout) {
            in_use.push(InUse {
                repo_id,
                source: format!("{}'s .tracevault/config.toml", checkout.display()),
            });
        }
    }
    in_use
}

async fn switch(
    path: Option<&str>,
    name: Option<&str>,
    repo_id: Option<uuid::Uuid>,
    user: bool,
    session_id: Option<&str>,
    project_root: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    // Validate the target argument before any network work, so a bad/empty
    // path or name surfaces as an argument error rather than an unrelated
    // network error from the resolution step below.
    let target = switch_target(path, name)?;

    let client = resolve_client(project_root)?;

    let SwitchTarget::Path(p) = target;
    let target_path = Path::new(p);
    let codebase = resolve_switch_codebase(target_path, &client).await?;

    // A session id is optional now: with one (and no --user) we bind that
    // session; without one (or with --user) we set the user-level default,
    // which any new session inherits. This is what lets a container bind its
    // repo before Claude — and its session — exists.
    let session = resolve_session_id(session_id).ok();
    let dest = switch_destination(user, session);

    // Resolve the repo BEFORE persisting anything: an ambiguous codebase must
    // not leave a persisted binding behind.
    let in_use = in_use_repos(&dest, target_path, project_root);
    let (repo_uuid, kept) = pick_switch_repo(&codebase, repo_id, &in_use)?;
    if let Some(source) = kept {
        let name = codebase
            .repos
            .iter()
            .find(|r| r.id == repo_uuid)
            .map_or("", |r| r.name.as_str());
        eprintln!(
            "codebase {} has {} repo records; keeping repo {name}, {source}",
            codebase.display_name(),
            codebase.repos.len()
        );
    }
    let binding = codebase.binding(repo_uuid);

    match dest {
        SwitchDest::Session(id) => {
            let mut state = session_state::load(&id);
            apply_switch(&mut state, binding.clone());
            session_state::save(&id, &state)?;
            println!("bound session {id} to repo {}", binding.repo_id);
        }
        SwitchDest::UserDefault => {
            crate::user_default::save(&binding)?;
            println!(
                "set user-level default repo {}; applies to new sessions without their own binding (the current session, if any, is unchanged — omit --user to bind this session)",
                binding.repo_id
            );
        }
    }

    // Best-effort policy fetch (applies to either destination). A failure here
    // must not make the whole command report as an error.
    match client.get_agent_instructions(&repo_uuid).await {
        Ok(policies) => println!("{}", policies.content),
        Err(e) => eprintln!("warning: bound, but could not fetch policies: {e}"),
    }
    Ok(())
}

/// Resolve a `--path <path>` override into a live `RepoBinding`, if possible.
/// Best-effort: prints a clear message and returns `None` on any failure
/// (missing credentials, or a server error) rather than failing the whole
/// `status` inspector.
async fn resolve_repo_flag(
    repo_flag_path: Option<&str>,
    project_root: &Path,
) -> Option<RepoBinding> {
    let path = repo_flag_path?;

    // Best-effort, per this function's contract: a credential/URL mismatch
    // prints the reason and drops the `--path` override rather than failing the
    // whole `status` inspector.
    let (server_url, credential) = match resolve_credentials(project_root) {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("--path {path}: {e}");
            return None;
        }
    };
    let Some(server_url) = server_url else {
        eprintln!(
            "--path {path}: no server URL configured (run `tracevault login`); showing binding \
             without the --path override"
        );
        return None;
    };

    let client = ApiClient::with_credential(&server_url, credential);
    match resolve_path_to_binding(Path::new(path), &client).await {
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
        Some((b, source)) => format!("bound to repo {} via {source}", b.repo_id),
        None => {
            "not bound to any repo (workspace mode; run `tracevault repo switch <path>`)".into()
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

    // A --path override on status resolves live (needs the server).
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

    // Surface which codebase the effective binding belongs to, with live clone
    // status when reachable. Best-effort — a failure must not fail `status`:
    //   1. `remote_id` (persisted at init / set by switch) → fetch the remote
    //      detail for name + clone status (works without a checkout);
    //   2. else a `git_url` → live `resolve_remote` for name + clone status;
    //   3. else fall back to the cached `codebase_name` (name only, offline).
    if let Some((binding, _)) = &effective {
        // Best-effort codebase line: a credential/URL mismatch degrades to the
        // offline fallback (cached name only) with the reason printed, rather
        // than failing `repo status` — whose job is to explain local state.
        let (server_url, credential) = match resolve_credentials(project_root) {
            Ok(resolved) => resolved,
            Err(e) => {
                eprintln!("Warning: {e}");
                (None, None)
            }
        };
        let client = server_url
            .as_ref()
            .map(|su| ApiClient::with_credential(su, credential));
        let detail_line = if let (Some(client), Some(remote_id)) = (&client, binding.remote_id) {
            client.get_remote_detail(remote_id).await.ok().map(|d| {
                crate::resolution::codebase_line(
                    d.name.as_deref(),
                    &d.normalized_url,
                    &d.clone_status,
                )
            })
        } else {
            None
        };
        let resolved_line = if detail_line.is_none() {
            if let (Some(client), Some(git_url)) = (&client, binding.git_url.as_deref()) {
                client
                    .resolve_remote(git_url)
                    .await
                    .ok()
                    .flatten()
                    .map(|r| {
                        crate::resolution::codebase_line(
                            r.name.as_deref(),
                            &r.normalized_url,
                            &r.clone_status,
                        )
                    })
            } else {
                None
            }
        } else {
            None
        };
        if let Some(line) = crate::resolution::pick_status_line(
            detail_line,
            resolved_line,
            binding.codebase_name.as_deref(),
        ) {
            println!("{line}");
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
            println!("user default: repo {}", ud.repo_id);
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
        assert_eq!(
            state,
            SessionState {
                auto_bind_off: true,
                ..SessionState::default()
            }
        );
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
            "not bound to any repo (workspace mode; run `tracevault repo switch <path>`)"
        );
    }

    #[test]
    fn format_status_repo_flag() {
        let b = binding("r1");
        assert_eq!(
            format_status(Some((&b, BindingSource::RepoFlag))),
            "bound to repo r1 via --path override"
        );
    }

    #[test]
    fn format_status_subagent() {
        let b = binding("r2");
        assert_eq!(
            format_status(Some((&b, BindingSource::Subagent))),
            "bound to repo r2 via subagent worktree override"
        );
    }

    #[test]
    fn format_status_session_active() {
        let b = binding("r3");
        assert_eq!(
            format_status(Some((&b, BindingSource::SessionActive))),
            "bound to repo r3 via session (repo switch)"
        );
    }

    #[test]
    fn format_status_bound() {
        let b = binding("r4");
        assert_eq!(
            format_status(Some((&b, BindingSource::Bound))),
            "bound to repo r4 via bound .tracevault/config.toml"
        );
    }

    #[test]
    fn format_status_user_default() {
        let b = binding("r5");
        assert_eq!(
            format_status(Some((&b, BindingSource::UserDefault))),
            "bound to repo r5 via user default (repo switch --user)"
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
    async fn resolve_switch_codebase_ok_for_registered_repo() {
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

        let codebase = resolve_switch_codebase(tmp.path(), &client)
            .await
            .expect("expected Ok codebase");
        let got = codebase.binding(pick_switch_repo(&codebase, None, &[]).unwrap().0);
        assert_eq!(got.repo_id, repo_id);
        assert_eq!(got.remote_id, Some(remote_id.parse().unwrap()));
        assert_eq!(got.codebase_name.as_deref(), Some("repo"));
    }

    #[tokio::test]
    async fn resolve_switch_codebase_errors_for_unregistered_repo() {
        let tmp = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(tmp.path());
        add_origin_remote(tmp.path(), "git@github.com:org/unregistered.git");

        // A genuine domain 404 carries the server's JSON error envelope
        // (`{"error": "..."}` — what every `AppError` renders), which is
        // what distinguishes "not tracked" from a bare route-404 (see
        // `ApiClient::resolve_remote`).
        let body = r#"{"error":"No remote for that URL"}"#;
        let resp = format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(), body
        );
        let base = spawn_once(Box::leak(resp.into_boxed_str()));
        let client = ApiClient::new(&base, Some("tok"));

        let err = resolve_switch_codebase(tmp.path(), &client)
            .await
            .expect_err("expected Err for unregistered repo");
        assert!(
            err.to_string().contains("not registered"),
            "unexpected error message: {err}"
        );
    }

    #[tokio::test]
    async fn resolve_switch_codebase_flags_bare_404_as_version_skew() {
        // A bare 404 (no JSON error body) is axum's "no route matched"
        // fallback — the shape an old server sends for a route it doesn't
        // have yet. `repo switch` must not misreport this as "not
        // registered"; it should surface a diagnosable version-mismatch
        // error instead.
        let tmp = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(tmp.path());
        add_origin_remote(tmp.path(), "git@github.com:org/unregistered.git");

        let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let base = spawn_once(resp);
        let client = ApiClient::new(&base, Some("tok"));

        let err = resolve_switch_codebase(tmp.path(), &client)
            .await
            .expect_err("expected Err for a bare route-404");
        assert!(
            !err.to_string().contains("not registered"),
            "must not be misreported as 'not registered'; got: {err}"
        );
        assert!(
            err.to_string().to_lowercase().contains("version mismatch"),
            "expected a version-mismatch hint; got: {err}"
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
        let b = resolve_path_to_binding(tmp.path(), &client)
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
        let err = resolve_path_to_binding(tmp.path(), &client)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no tracked repo"), "got: {err}");
    }

    // ── VIS-1018: several repo records for one codebase ───────────────────────

    const LOW: &str = "44000761-0000-4000-8000-000000000001";
    const HIGH: &str = "7995d35d-0000-4000-8000-000000000002";

    fn codebase_with(repo_ids: &[&str]) -> ResolvedCodebase {
        ResolvedCodebase {
            git_url: "git@github.com:org/repo.git".into(),
            remote: crate::api_client::ResolveRemoteResponse {
                remote_id: uuid::Uuid::from_u128(7),
                name: Some("repo".into()),
                normalized_url: "github.com/org/repo".into(),
                clone_status: "ready".into(),
            },
            repos: repo_ids
                .iter()
                .enumerate()
                .map(|(i, id)| crate::api_client::RemoteRepoRef {
                    id: id.parse().unwrap(),
                    name: format!("repo-{i}"),
                })
                .collect(),
        }
    }

    fn in_use(repo_id: &str, source: &str) -> InUse {
        InUse {
            repo_id: repo_id.parse().unwrap(),
            source: source.into(),
        }
    }

    /// The prod split: the session streamed to the higher-id repo (its
    /// config.toml binding); a switch to the same checkout must keep it, not
    /// move the session to the lowest id.
    #[test]
    fn switch_keeps_the_current_binding_over_the_lowest_id() {
        let codebase = codebase_with(&[LOW, HIGH]);
        let (id, kept) =
            pick_switch_repo(&codebase, None, &[in_use(HIGH, "the current binding")]).unwrap();
        assert_eq!(id.to_string(), HIGH);
        assert_eq!(kept.as_deref(), Some("the current binding"));
    }

    /// The current binding outranks a checkout's config.toml.
    #[test]
    fn switch_prefers_the_current_binding_over_a_checkout_config() {
        let codebase = codebase_with(&[LOW, HIGH]);
        let (id, _) = pick_switch_repo(
            &codebase,
            None,
            &[
                in_use(LOW, "the current binding"),
                in_use(HIGH, "config.toml"),
            ],
        )
        .unwrap();
        assert_eq!(id.to_string(), LOW);
    }

    /// A current binding to another codebase says nothing about this one:
    /// the next in-use repo that this codebase links decides.
    #[test]
    fn switch_skips_in_use_repos_of_other_codebases() {
        let codebase = codebase_with(&[LOW, HIGH]);
        let other = "99999999-0000-4000-8000-000000000009";
        let (id, kept) = pick_switch_repo(
            &codebase,
            None,
            &[
                in_use(other, "the current binding"),
                in_use(HIGH, "config.toml"),
            ],
        )
        .unwrap();
        assert_eq!(id.to_string(), HIGH);
        assert_eq!(kept.as_deref(), Some("config.toml"));
    }

    /// Several repos and none in use: an error naming them, never a guess.
    #[test]
    fn switch_reports_ambiguity_instead_of_picking() {
        let codebase = codebase_with(&[LOW, HIGH]);
        let err = pick_switch_repo(&codebase, None, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains(LOW) && err.contains(HIGH), "got: {err}");
        assert!(err.contains("--repo-id"), "got: {err}");
    }

    /// `--repo-id` is the explicit choice, also against the current binding.
    #[test]
    fn switch_repo_id_flag_overrides_the_current_binding() {
        let codebase = codebase_with(&[LOW, HIGH]);
        let (id, kept) = pick_switch_repo(
            &codebase,
            Some(LOW.parse().unwrap()),
            &[in_use(HIGH, "the current binding")],
        )
        .unwrap();
        assert_eq!(id.to_string(), LOW);
        assert_eq!(kept, None);
    }

    #[test]
    fn switch_repo_id_flag_must_name_a_repo_of_the_codebase() {
        let codebase = codebase_with(&[LOW, HIGH]);
        let other = "99999999-0000-4000-8000-000000000009";
        let err = pick_switch_repo(&codebase, Some(other.parse().unwrap()), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not a repo of codebase"), "got: {err}");
    }

    /// One repo: nothing to choose, whatever is in use elsewhere.
    #[test]
    fn switch_binds_the_only_repo() {
        let codebase = codebase_with(&[LOW]);
        let other = "99999999-0000-4000-8000-000000000009";
        let (id, kept) =
            pick_switch_repo(&codebase, None, &[in_use(other, "the current binding")]).unwrap();
        assert_eq!(id.to_string(), LOW);
        assert_eq!(kept, None);
    }

    /// `resolve_path_to_binding` (used by `check` and `repo status --path`)
    /// picks the repo the checkout's own config.toml binds — the one its hooks
    /// stream to — rather than the lowest id.
    #[tokio::test]
    async fn resolve_path_prefers_the_checkout_config_repo() {
        let tmp = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(tmp.path());
        add_origin_remote(tmp.path(), "git@github.com:org/repo.git");
        std::fs::create_dir_all(tmp.path().join(".tracevault")).unwrap();
        std::fs::write(
            tmp.path().join(".tracevault/config.toml"),
            format!("repo_id = \"{HIGH}\"\n"),
        )
        .unwrap();
        let remote_id = "44000761-8d22-4256-bd2c-27a0ba278c6f";
        let remote = format!(
            r#"{{"remote_id":"{remote_id}","name":"repo","normalized_url":"github.com/org/repo","clone_status":"ready"}}"#
        );
        let detail = format!(
            r#"{{"name":"repo","normalized_url":"github.com/org/repo","clone_status":"ready","repos":[{{"id":"{LOW}","name":"tracevault"}},{{"id":"{HIGH}","name":"tracing"}}]}}"#
        );
        let base = spawn_n(vec![http_200(&remote), http_200(&detail)]);
        let client = ApiClient::new(&base, Some("tok"));

        let got = resolve_path_to_binding(tmp.path(), &client)
            .await
            .unwrap()
            .expect("registered");
        assert_eq!(got.repo_id, HIGH);
    }

    // ── in_use_repos: what switch keeps, read from real state ─────────────────

    const TARGET_CFG: &str = "c0000000-0000-4000-8000-00000000000c";
    const CWD_CFG: &str = "b0000000-0000-4000-8000-00000000000b";
    const ACTIVE: &str = "a0000000-0000-4000-8000-00000000000a";
    const USER: &str = "d0000000-0000-4000-8000-00000000000d";

    /// A git checkout whose `.tracevault/config.toml` binds `repo_id`.
    fn checkout_bound_to(repo_id: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(dir.path());
        std::fs::create_dir_all(dir.path().join(".tracevault")).unwrap();
        std::fs::write(
            dir.path().join(".tracevault/config.toml"),
            format!("repo_id = \"{repo_id}\"\n"),
        )
        .unwrap();
        dir
    }

    fn ids(in_use: &[InUse]) -> Vec<String> {
        in_use.iter().map(|u| u.repo_id.to_string()).collect()
    }

    /// Session destination: the session's own binding first, then the target
    /// checkout's config, then this checkout's.
    #[tokio::test]
    async fn in_use_puts_the_session_binding_first() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let home = tempfile::tempdir().unwrap();
        let mut guard = crate::test_helpers::EnvVarGuard::new();
        guard.set("XDG_STATE_HOME", home.path());
        guard.set("XDG_CONFIG_HOME", home.path());
        let target = checkout_bound_to(TARGET_CFG);
        let cwd = checkout_bound_to(CWD_CFG);
        let state = SessionState {
            active: Some(binding(ACTIVE)),
            ..Default::default()
        };
        session_state::save("sess-inuse", &state).unwrap();

        let got = in_use_repos(
            &SwitchDest::Session("sess-inuse".into()),
            target.path(),
            cwd.path(),
        );

        assert_eq!(ids(&got), [ACTIVE, TARGET_CFG, CWD_CFG]);
    }

    /// A session without its own binding streams to this checkout's
    /// config.toml, so that is its current binding.
    #[tokio::test]
    async fn in_use_falls_back_to_what_the_session_streams_to() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let home = tempfile::tempdir().unwrap();
        let mut guard = crate::test_helpers::EnvVarGuard::new();
        guard.set("XDG_STATE_HOME", home.path());
        guard.set("XDG_CONFIG_HOME", home.path());
        let target = checkout_bound_to(TARGET_CFG);
        let cwd = checkout_bound_to(CWD_CFG);

        let got = in_use_repos(
            &SwitchDest::Session("sess-unbound".into()),
            target.path(),
            cwd.path(),
        );

        assert_eq!(ids(&got)[0], CWD_CFG);
        assert!(
            got[0].source.contains("current binding"),
            "{}",
            got[0].source
        );
    }

    /// `--user`: the current user default comes first, not any session.
    #[tokio::test]
    async fn in_use_for_the_user_default_starts_with_the_user_default() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let home = tempfile::tempdir().unwrap();
        let mut guard = crate::test_helpers::EnvVarGuard::new();
        guard.set("XDG_STATE_HOME", home.path());
        guard.set("XDG_CONFIG_HOME", home.path());
        crate::user_default::save(&binding(USER)).unwrap();
        let target = checkout_bound_to(TARGET_CFG);
        let cwd = checkout_bound_to(CWD_CFG);

        let got = in_use_repos(&SwitchDest::UserDefault, target.path(), cwd.path());

        assert_eq!(ids(&got), [USER, TARGET_CFG, CWD_CFG]);
    }

    /// End to end: an ambiguous codebase fails the switch and leaves the
    /// session's binding as it was.
    #[tokio::test]
    async fn ambiguous_switch_leaves_the_session_binding_unchanged() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let home = tempfile::tempdir().unwrap();
        let other = "99999999-0000-4000-8000-000000000009";
        let state = SessionState {
            active: Some(binding(other)),
            ..Default::default()
        };
        let target = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(target.path());
        add_origin_remote(target.path(), "git@github.com:org/repo.git");
        let remote = r#"{"remote_id":"2280a1c9-0000-4000-8000-000000000003","name":"tracing","normalized_url":"github.com/org/repo","clone_status":"ready"}"#;
        let detail = format!(
            r#"{{"name":"tracing","normalized_url":"github.com/org/repo","clone_status":"ready","repos":[{{"id":"{LOW}","name":"tracevault"}},{{"id":"{HIGH}","name":"tracing"}}]}}"#
        );
        let base = spawn_n(vec![http_200(remote), http_200(&detail)]);
        let mut guard = crate::test_helpers::EnvVarGuard::new();
        guard.set("XDG_STATE_HOME", home.path());
        guard.set("XDG_CONFIG_HOME", home.path());
        guard.set("TRACEVAULT_SERVER_URL", &base);
        guard.set("TRACEVAULT_API_KEY", "tok");
        session_state::save("sess-ambiguous", &state).unwrap();

        let err = switch(
            Some(&target.path().to_string_lossy()),
            None,
            None,
            false,
            Some("sess-ambiguous"),
            home.path(),
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("--repo-id"), "got: {err}");
        assert_eq!(session_state::load("sess-ambiguous"), state);
    }
}
