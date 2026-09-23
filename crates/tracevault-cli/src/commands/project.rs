//! `tracevault project` — workspace/detached-mode project-attribution
//! commands. Mirrors `commands::repo`'s `switch`/`status` structure, but binds
//! the project-attribution axis (design §7/Task 7) rather than the repo axis.

use std::collections::HashSet;
use std::path::Path;

use crate::api_client::{resolve_client, ApiClient, ProjectListItem, ResolveProjectOutcome};
use crate::resolution::{
    capture_project_binding, capture_project_id, git_remote_url, CaptureProjectInputs,
    ProjectSource,
};
use crate::session_state::{self, ProjectBinding, SessionState};

/// Sub-actions for `tracevault project` (project-attribution axis).
#[derive(clap::Subcommand)]
pub enum ProjectCmd {
    /// Bind project attribution to a registered project for the current
    /// session (or, with `--user`, the session-independent user default).
    Switch {
        /// Exact, case-sensitive name of a registered project.
        name: String,
        /// Write a session-independent user-level default instead of a
        /// session binding. Implied when no session id is available. Also
        /// skips the current-codebase containment check (a user default
        /// isn't tied to any one checkout).
        #[arg(long)]
        user: bool,
        /// Session to target; defaults to $TRACEVAULT_SESSION_ID.
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Show the project the current session is attributed to.
    Status {
        /// Session to target; defaults to $TRACEVAULT_SESSION_ID.
        #[arg(long)]
        session_id: Option<String>,
        /// One-off: resolve this project name and feed it in at the
        /// `--project` precedence tier, above the session and user-default
        /// bindings.
        #[arg(long)]
        project: Option<String>,
    },
}

pub async fn run(
    cmd: ProjectCmd,
    project_root: &Path,
    cwd: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        ProjectCmd::Switch {
            name,
            user,
            session_id,
        } => switch(&name, user, session_id.as_deref(), project_root, cwd).await,
        ProjectCmd::Status {
            session_id,
            project,
        } => status(session_id.as_deref(), project.as_deref(), project_root, cwd).await,
    }
}

/// Exact, case-sensitive name → item. Miss lists the available names.
fn resolve_project_name<'a>(
    items: &'a [ProjectListItem],
    name: &str,
) -> Result<&'a ProjectListItem, String> {
    items.iter().find(|p| p.name == name).ok_or_else(|| {
        let mut names: Vec<&str> = items.iter().map(|p| p.name.as_str()).collect();
        names.sort_unstable();
        format!(
            "project '{name}' not found. Available: {}",
            names.join(", ")
        )
    })
}

/// Resolve `name` to a registered project (via `list_projects`) and, unless
/// `check_codebase` is `false`, verify that project contains the current
/// codebase (resolved from `cwd`'s git origin remote, mirroring
/// `resolve_path_to_binding`). Kept separate from `switch` so the
/// client-dependent flow is unit-testable with a mock `ApiClient`, mirroring
/// `commands::repo::resolve_switch_binding`.
async fn resolve_switch_project(
    name: &str,
    client: &ApiClient,
    check_codebase: bool,
    cwd: &Path,
) -> Result<ProjectBinding, Box<dyn std::error::Error>> {
    let items = client.list_projects().await?;
    let matched = resolve_project_name(&items, name)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let project_id = matched.id;
    let project_name = matched.name.clone();

    if check_codebase {
        if let Some(git_url) = git_remote_url(cwd) {
            if let Some(remote) = client.resolve_remote(&git_url).await? {
                let codebase_repo_ids: HashSet<uuid::Uuid> = client
                    .get_remote_repos(remote.remote_id)
                    .await?
                    .into_iter()
                    .map(|r| r.id)
                    .collect();
                let project_repo_ids: HashSet<uuid::Uuid> = client
                    .get_project(project_id)
                    .await?
                    .repos
                    .into_iter()
                    .map(|r| r.id)
                    .collect();
                if codebase_repo_ids.is_disjoint(&project_repo_ids) {
                    return Err(
                        format!("project '{name}' does not contain the current codebase").into(),
                    );
                }
            }
            // Codebase not registered with the server → nothing to check
            // against; allow the switch (mirrors resolve_path_to_binding's
            // Ok(None) for an untracked remote).
        }
        // No git origin remote at all (workspace mode, no checkout) → nothing
        // to check against; allow the switch.
    }

    Ok(ProjectBinding {
        project_id: project_id.to_string(),
        project_name,
        updated_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Where a `project switch` should persist its binding: a specific session,
/// or the session-independent user-level default. Mirrors
/// `commands::repo::SwitchDest`/`switch_destination`.
#[derive(Debug)]
enum SwitchDest {
    Session(String),
    UserDefault,
}

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
    name: &str,
    user: bool,
    session_id: Option<&str>,
    project_root: &Path,
    cwd: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = resolve_client(project_root)?;
    let session = crate::commands::repo::resolve_session_id(session_id).ok();
    let dest = switch_destination(user, session);
    let check_codebase = matches!(dest, SwitchDest::Session(_));
    let binding = resolve_switch_project(name, &client, check_codebase, cwd).await?;

    match dest {
        SwitchDest::Session(id) => {
            let mut state = session_state::load(&id);
            state.active_project = Some(binding.clone());
            session_state::save(&id, &state)?;
            println!("bound session {id} to project {}", binding.project_name);
        }
        SwitchDest::UserDefault => {
            crate::user_project_default::save(&binding)?;
            println!(
                "set user-level default project {}; applies to new sessions without their own binding (the current session, if any, is unchanged — omit --user to bind this session)",
                binding.project_name
            );
        }
    }
    Ok(())
}

/// The project `project status` treats as effective: exactly the capture-time
/// chain ingest uses ([`capture_project_binding`]: `--project` flag →
/// subagent worktree override → session `active_project` → user default).
/// Pure — `status` supplies `user_default` from `user_project_default::load()`,
/// the same file `commands::stream::capture_project` reads, so the two cannot
/// drift (pinned by `project_status_effective_project_matches_capture_project`).
fn effective_capture_project(
    project_flag: Option<ProjectBinding>,
    session: &SessionState,
    worktree: Option<&str>,
    user_default: Option<ProjectBinding>,
) -> Option<(ProjectBinding, ProjectSource)> {
    capture_project_binding(&CaptureProjectInputs {
        project_flag,
        session,
        worktree_path: worktree,
        user_default,
    })
}

/// Display label for a binding: the friendly name, falling back to the id.
fn binding_label(b: &ProjectBinding) -> &str {
    if b.project_name.is_empty() {
        &b.project_id
    } else {
        &b.project_name
    }
}

/// Pure formatter for `project status`'s effective-project line: the project
/// ingest attributes events to, and via which precedence tier. Mirrors
/// `commands::repo::format_status`. `None` means ingest sends events
/// repo-scoped and leaves attribution to the server.
fn format_status(effective: Option<(&ProjectBinding, ProjectSource)>) -> String {
    match effective {
        Some((b, source)) => format!("project: {} via {source}", binding_label(b)),
        None => "no project bound locally; events are sent repo-scoped and the server deduces the project from the repo".to_string(),
    }
}

/// Warning for a capture binding whose stored `project_id` is not a UUID:
/// ingest drops it (`capture_project_id` → `None`), so `status` reports the
/// session as unbound. Same rule and wording as `commands::status`'s
/// `project_binding_check`.
fn invalid_capture_id_warning(binding: &ProjectBinding, source: ProjectSource) -> String {
    format!(
        "warning: project {} — {source}, but the saved project_id is not a valid id and is dropped at capture time; run `tracevault project switch <name>` to rewrite it",
        binding_label(binding)
    )
}

/// Warning for a `.tracevault/config.toml` `default_project`: it is a name,
/// ingest never resolves it (that would be a per-event network call), so it
/// never decides attribution. `status` neither resolves it nor feeds it into
/// the chain.
fn config_default_warning(name: &str) -> String {
    format!(
        "warning: .tracevault/config.toml default_project '{name}' is not used for attribution at capture time; bind explicitly with `tracevault project switch <name>` or --project"
    )
}

/// The name of a deduced project id, if the projects list is already in
/// scope (fetched for `--project`). Never triggers an API call of its own:
/// the name is cosmetic, and the id is printed when it is unknown.
fn deduced_project_name(pid: uuid::Uuid, items: Option<&[ProjectListItem]>) -> Option<&str> {
    items?.iter().find(|p| p.id == pid).map(|p| p.name.as_str())
}

/// Pure formatter for the server-deduction line: what the server would deduce
/// from this repo's git remote if no local binding were set. Shown separately
/// from the effective project because ingest never deduces client-side — it
/// only matters when nothing is bound locally. `resolved_name` is the name of
/// a `Resolved` project when already known (see [`deduced_project_name`]);
/// otherwise the id is printed.
fn format_deduction(
    effective_is_bound: bool,
    outcome: Result<ResolveProjectOutcome, String>,
    resolved_name: Option<&str>,
) -> String {
    const PREFIX: &str = "server deduction for this repo:";
    match outcome {
        Ok(ResolveProjectOutcome::Resolved(pid)) => {
            let pid = pid.to_string();
            let label = resolved_name.unwrap_or(&pid);
            if effective_is_bound {
                format!("{PREFIX} {label} (not used: the local binding above wins)")
            } else {
                format!("{PREFIX} {label} (this is where events will land)")
            }
        }
        Ok(ResolveProjectOutcome::Ambiguous) => {
            if effective_is_bound {
                format!(
                    "{PREFIX} ambiguous (multiple projects) — not used, the local binding above wins"
                )
            } else {
                format!(
                    "{PREFIX} ambiguous (multiple projects) — events will be refused until you run `tracevault project switch <name>` or pass --project"
                )
            }
        }
        Ok(ResolveProjectOutcome::None) => format!("{PREFIX} none"),
        Err(msg) => format!("{PREFIX} unavailable ({msg})"),
    }
}

/// `tracevault project status`: a read-only inspector that always returns
/// `Ok(())`. The effective project is the capture-time chain (local, no
/// network); the server's deduction from the git remote is shown on its own
/// line, best-effort, as what would apply if nothing were bound locally.
async fn status(
    session_id: Option<&str>,
    project_flag_name: Option<&str>,
    project_root: &Path,
    cwd: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    // Session state is best-effort: if a session id resolves, load it; else
    // warn and fall back to an empty SessionState.
    let session = match crate::commands::repo::resolve_session_id(session_id) {
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
    let config_default_name =
        crate::config::TracevaultConfig::load(project_root).and_then(|c| c.default_project);
    let user_default = crate::user_project_default::load();
    let git_url = git_remote_url(cwd);

    // The server is needed only to resolve a `--project` NAME and to show the
    // deduction line; the effective project itself is computed locally.
    let client = match resolve_client(project_root) {
        Ok(client) => Some(client),
        Err(e) => {
            eprintln!("warning: could not resolve credentials ({e}); server deduction not shown");
            None
        }
    };

    let mut items: Option<Vec<ProjectListItem>> = None;
    let project_flag = match (project_flag_name, client.as_ref()) {
        (Some(name), Some(client)) => {
            items = client.list_projects().await.ok();
            let flag = items
                .as_deref()
                .and_then(|items| resolve_project_name(items, name).ok())
                .map(|matched| ProjectBinding {
                    project_id: matched.id.to_string(),
                    project_name: matched.name.clone(),
                    updated_at: chrono::Utc::now().to_rfc3339(),
                });
            if flag.is_none() {
                eprintln!(
                    "warning: --project '{name}' could not be resolved; ignoring the override"
                );
            }
            flag
        }
        (Some(name), None) => {
            eprintln!(
                "warning: --project '{name}' cannot be resolved without server credentials; ignoring the override"
            );
            None
        }
        (None, _) => None,
    };

    let effective =
        match effective_capture_project(project_flag, &session, Some(&worktree), user_default) {
            Some((binding, source)) if capture_project_id(&binding).is_none() => {
                eprintln!("{}", invalid_capture_id_warning(&binding, source));
                None
            }
            other => other,
        };

    if let Some(name) = config_default_name.as_deref() {
        eprintln!("{}", config_default_warning(name));
    }

    println!(
        "{}",
        format_status(effective.as_ref().map(|(b, s)| (b, *s)))
    );

    if let (Some(client), Some(url)) = (client.as_ref(), git_url.as_deref()) {
        // Best-effort and informational: an ambiguous or failed deduction is
        // reported, never propagated — `status` exits 0 regardless.
        let outcome = client.resolve_project(url).await.map_err(|e| e.to_string());
        let name = match &outcome {
            Ok(ResolveProjectOutcome::Resolved(pid)) => {
                deduced_project_name(*pid, items.as_deref())
            }
            _ => None,
        };
        println!("{}", format_deduction(effective.is_some(), outcome, name));
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
    fn resolve_project_name_exact_match_and_miss() {
        let items = vec![
            ProjectListItem {
                id: uuid::Uuid::nil(),
                name: "payments".into(),
            },
            ProjectListItem {
                id: uuid::Uuid::from_u128(2),
                name: "web".into(),
            },
        ];
        let ok = resolve_project_name(&items, "web").unwrap();
        assert_eq!(ok.id, uuid::Uuid::from_u128(2));
        let err = resolve_project_name(&items, "Web").unwrap_err(); // case-sensitive
        assert!(err.contains("payments") && err.contains("web")); // lists available names
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

    fn pb(name: &str) -> ProjectBinding {
        ProjectBinding {
            project_id: format!("id-{name}"),
            project_name: name.into(),
            updated_at: "t".into(),
        }
    }

    #[test]
    fn format_status_unbound() {
        assert_eq!(
            format_status(None),
            "no project bound locally; events are sent repo-scoped and the server deduces the project from the repo"
        );
    }

    #[test]
    fn format_status_project_flag() {
        let b = pb("web");
        assert_eq!(
            format_status(Some((&b, ProjectSource::ProjectFlag))),
            "project: web via --project override"
        );
    }

    #[test]
    fn format_status_session_active() {
        let b = pb("payments");
        assert_eq!(
            format_status(Some((&b, ProjectSource::SessionActive))),
            "project: payments via session (project switch)"
        );
    }

    #[test]
    fn format_status_falls_back_to_id_when_name_empty() {
        // `Deduced` can no longer be the effective tier (the capture chain
        // never deduces); a bound tier with an empty name still prints the id.
        let b = ProjectBinding {
            project_id: "some-id".into(),
            project_name: String::new(),
            updated_at: "".into(),
        };
        assert_eq!(
            format_status(Some((&b, ProjectSource::UserDefault))),
            "project: some-id via user default (project switch --user)"
        );
    }

    #[test]
    fn invalid_capture_id_warning_names_tier_and_rewrite_command() {
        let b = ProjectBinding {
            project_id: "not-a-uuid".into(),
            project_name: "payments".into(),
            updated_at: "".into(),
        };
        assert_eq!(
            invalid_capture_id_warning(&b, ProjectSource::UserDefault),
            "warning: project payments — user default (project switch --user), but the saved project_id is not a valid id and is dropped at capture time; run `tracevault project switch <name>` to rewrite it"
        );
    }

    #[test]
    fn config_default_warning_says_it_is_not_used_at_capture_time() {
        assert_eq!(
            config_default_warning("web"),
            "warning: .tracevault/config.toml default_project 'web' is not used for attribution at capture time; bind explicitly with `tracevault project switch <name>` or --project"
        );
    }

    const DEDUCED: uuid::Uuid = uuid::Uuid::from_u128(2);

    #[test]
    fn format_deduction_resolved_bound() {
        assert_eq!(
            format_deduction(true, Ok(ResolveProjectOutcome::Resolved(DEDUCED)), None),
            format!(
                "server deduction for this repo: {DEDUCED} (not used: the local binding above wins)"
            )
        );
    }

    #[test]
    fn format_deduction_resolved_unbound() {
        assert_eq!(
            format_deduction(false, Ok(ResolveProjectOutcome::Resolved(DEDUCED)), None),
            format!("server deduction for this repo: {DEDUCED} (this is where events will land)")
        );
    }

    #[test]
    fn format_deduction_resolved_uses_name_when_known() {
        let items = items();
        let name = deduced_project_name(DEDUCED, Some(&items));
        assert_eq!(name, Some("web"));
        assert_eq!(
            format_deduction(false, Ok(ResolveProjectOutcome::Resolved(DEDUCED)), name),
            "server deduction for this repo: web (this is where events will land)"
        );
    }

    #[test]
    fn deduced_project_name_none_without_list_or_match() {
        assert_eq!(deduced_project_name(DEDUCED, None), None);
        assert_eq!(
            deduced_project_name(uuid::Uuid::from_u128(999), Some(&items())),
            None
        );
    }

    #[test]
    fn format_deduction_ambiguous_bound() {
        assert_eq!(
            format_deduction(true, Ok(ResolveProjectOutcome::Ambiguous), None),
            "server deduction for this repo: ambiguous (multiple projects) — not used, the local binding above wins"
        );
    }

    #[test]
    fn format_deduction_ambiguous_unbound() {
        assert_eq!(
            format_deduction(false, Ok(ResolveProjectOutcome::Ambiguous), None),
            "server deduction for this repo: ambiguous (multiple projects) — events will be refused until you run `tracevault project switch <name>` or pass --project"
        );
    }

    #[test]
    fn format_deduction_none() {
        for bound in [true, false] {
            assert_eq!(
                format_deduction(bound, Ok(ResolveProjectOutcome::None), None),
                "server deduction for this repo: none"
            );
        }
    }

    #[test]
    fn format_deduction_err() {
        for bound in [true, false] {
            assert_eq!(
                format_deduction(bound, Err("connection refused".into()), None),
                "server deduction for this repo: unavailable (connection refused)"
            );
        }
    }

    #[test]
    fn format_status_user_default() {
        let b = pb("payments");
        assert_eq!(
            format_status(Some((&b, ProjectSource::UserDefault))),
            "project: payments via user default (project switch --user)"
        );
    }

    /// Spawn a one-shot raw-HTTP server that returns `response` to the first
    /// request it accepts (mirrors `commands::repo`'s test helper).
    fn spawn_once(response: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let response: &'static str = Box::leak(response.into_boxed_str());
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

    /// Generalizes `spawn_once` to a listener that answers each of
    /// `responses` in order, one per accepted connection (the
    /// containment-check flow makes several sequential requests).
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

    fn http_200(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    }

    /// Happy-path flow test (mirrors `resolve_switch_binding_ok_for_registered_repo`
    /// in `commands/repo.rs`): mock `GET /projects`, resolve the switch's
    /// project binding, apply it to a (uniquely-named, cleaned-up-afterward)
    /// session's state, and confirm the write round-trips through the real
    /// `session_state::save`/`load_from` — the exact assertion the brief
    /// calls for.
    #[tokio::test]
    async fn project_switch_happy_path_writes_active_project_to_session_file() {
        let tmp = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(tmp.path()); // no origin remote → containment check is a no-op

        // C3: isolate `session_state::sessions_dir()` (which honors
        // `$XDG_STATE_HOME`) to a tempdir for the duration of this test, so
        // it never touches the developer's real state dir.
        //
        // SAFETY: test-scoped env mutation, mirroring the precedent in
        // `commands::project`'s tests (`status_reports_ambiguous_deduction_
        // as_informational_not_fatal`). No other test in this crate reads
        // or sets XDG_STATE_HOME, so this can't race another test's
        // expectations; restored in a guard so a panic mid-test still
        // cleans up the process env.
        let state_tmp = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_STATE_HOME", state_tmp.path());

        let list = r#"[{"id":"11111111-1111-4111-8111-111111111111","name":"payments"},{"id":"22222222-2222-4222-8222-222222222222","name":"web"}]"#;
        let base = spawn_once(http_200(list));
        let client = ApiClient::new(&base, Some("tok"));

        let binding = resolve_switch_project("web", &client, true, tmp.path())
            .await
            .expect("expected Ok binding");
        assert_eq!(binding.project_id, "22222222-2222-4222-8222-222222222222");
        assert_eq!(binding.project_name, "web");

        let session_id = format!("project-switch-test-{}", uuid::Uuid::new_v4());
        let mut state = session_state::load(&session_id);
        state.active_project = Some(binding.clone());
        session_state::save(&session_id, &state).expect("save must succeed");

        let sessions_dir = session_state::sessions_dir().expect("sessions dir must resolve");
        assert!(sessions_dir.starts_with(state_tmp.path()));
        let loaded = session_state::load_from(&sessions_dir, &session_id);
        assert_eq!(loaded.active_project, Some(binding));
    }

    #[tokio::test]
    async fn project_switch_errors_when_name_not_found() {
        let list = r#"[{"id":"11111111-1111-4111-8111-111111111111","name":"payments"}]"#;
        let base = spawn_once(http_200(list));
        let client = ApiClient::new(&base, Some("tok"));

        let err = resolve_switch_project("web", &client, false, Path::new("/nonexistent"))
            .await
            .expect_err("expected Err for an unknown project name");
        let msg = err.to_string();
        assert!(msg.contains("not found"), "got: {msg}");
        assert!(msg.contains("payments"), "got: {msg}");
    }

    #[tokio::test]
    async fn project_switch_errors_when_project_does_not_contain_codebase() {
        let tmp = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(tmp.path());
        let ok = std::process::Command::new("git")
            .args([
                "-C",
                &tmp.path().to_string_lossy(),
                "remote",
                "add",
                "origin",
                "git@github.com:org/repo.git",
            ])
            .status()
            .expect("git remote add failed")
            .success();
        assert!(ok, "git remote add must succeed");

        let list =
            r#"[{"id":"11111111-1111-4111-8111-111111111111","name":"payments"}]"#.to_string();
        let remote_id = "44000761-8d22-4256-bd2c-27a0ba278c6f";
        let remote = format!(
            r#"{{"remote_id":"{remote_id}","name":"repo","normalized_url":"github.com/org/repo","clone_status":"ready"}}"#
        );
        let codebase_repo_id = "55555555-5555-4555-8555-555555555555";
        let detail = format!(
            r#"{{"id":"{remote_id}","name":"repo","normalized_url":"github.com/org/repo","clone_url":"https://github.com/org/repo.git","clone_status":"ready","clone_error":null,"last_fetched_at":null,"repo_count":1,"created_at":"2026-01-01T00:00:00Z","repos":[{{"id":"{codebase_repo_id}","name":"repo"}}]}}"#
        );
        let project_detail = r#"{"repos":[{"id":"66666666-6666-4666-8666-666666666666"}]}"#;
        let base = spawn_n(vec![
            http_200(&list),
            http_200(&remote),
            http_200(&detail),
            http_200(project_detail),
        ]);
        let client = ApiClient::new(&base, Some("tok"));

        let err = resolve_switch_project("payments", &client, true, tmp.path())
            .await
            .expect_err("expected Err: project doesn't contain the current codebase");
        assert!(
            err.to_string()
                .contains("does not contain the current codebase"),
            "got: {err}"
        );
    }

    /// Regression for C2: a no-session, non-`--user` `switch` resolves to
    /// `SwitchDest::UserDefault` (mirroring `switch_destination`'s
    /// no-session fallback), so the codebase-containment check must be
    /// skipped even though `--user` wasn't passed. `cwd` has a git origin
    /// remote, but `check_codebase` being correctly derived from the
    /// destination (not from `!user`) means `resolve_switch_project` never
    /// calls out to `resolve_remote`/`get_project` to check it — the mock
    /// server below only ever answers one request (`list_projects`). If the
    /// gating regressed back to `!user`, the second HTTP call this test
    /// deliberately can't serve (mirrors `spawn_once`'s one-shot listener,
    /// which closes after the first `accept()`) would fail fast, and the
    /// switch would error instead of succeeding.
    ///
    /// Credentials are supplied two ways at once: a `.tracevault/config.toml`
    /// in `cwd`, and `TRACEVAULT_SERVER_URL`/`_API_KEY` env vars pinned (via
    /// the guard below) at the same mock server/values. The env vars sit
    /// above the config file in `resolve_credentials`'s precedence, so
    /// without pinning them explicitly, an ambient shell that already
    /// exports `TRACEVAULT_SERVER_URL` (etc.) would leak in and point this
    /// test's client at the wrong server — deterministic in CI (which
    /// doesn't set them) but flaky on a developer machine that has them
    /// exported. The guard's `set` calls make the test's behavior
    /// independent of the ambient environment. `_env_lock` (taken below)
    /// also serializes this against `status_reports_ambiguous_deduction_as_
    /// informational_not_fatal`, which touches the same vars.
    /// `XDG_CONFIG_HOME` is redirected so this doesn't read the developer's
    /// real `credentials.json` (which could otherwise short-circuit
    /// `resolve_credentials` before the config file/env vars are consulted,
    /// and race for real on a machine with actual TraceVault credentials)
    /// and so the resulting user-default write lands in a tempdir, not
    /// `~/.config`. `user_project_default`'s own real-path round-trip test
    /// mutates the same var, so both hold `test_helpers::lock_env_mutation()`
    /// for their duration.
    #[tokio::test]
    async fn switch_without_session_or_user_flag_skips_codebase_check() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(tmp.path());
        let ok = std::process::Command::new("git")
            .args([
                "-C",
                &tmp.path().to_string_lossy(),
                "remote",
                "add",
                "origin",
                "git@github.com:org/repo.git",
            ])
            .status()
            .expect("git remote add failed")
            .success();
        assert!(ok, "git remote add must succeed");

        let list =
            r#"[{"id":"11111111-1111-4111-8111-111111111111","name":"payments"}]"#.to_string();
        let base = spawn_once(http_200(&list));

        let config_dir = tmp.path().join(".tracevault");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            format!("agent = \"claude-code\"\nserver_url = \"{base}\"\napi_key = \"tok\"\n"),
        )
        .unwrap();

        // SAFETY: test-scoped env mutation, restored in a guard so a panic
        // in `switch` still cleans up the process env. `_env_lock` (taken
        // above) also covers `resolve_credentials`'s *reads* of
        // TRACEVAULT_SERVER_URL/_API_KEY: those env vars sit above
        // `.tracevault/config.toml` in precedence, so if the developer's
        // ambient shell happens to already export them (e.g. pointing at a
        // real server), `resolve_credentials` would pick those up instead of
        // this test's config file and the request would go to the wrong
        // place — deterministic in CI (which doesn't set them) but flaky
        // locally. Pin them explicitly at this test's own mock server so
        // behavior doesn't depend on the ambient environment, mirroring the
        // precedent in `status_reports_ambiguous_deduction_as_informational_
        // not_fatal`. `_env_lock` (taken above) also serializes this against
        // that test and any other test in the crate touching the same vars.
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.remove("TRACEVAULT_SESSION_ID");
        _guard.set("XDG_CONFIG_HOME", tmp.path());
        _guard.set("TRACEVAULT_SERVER_URL", &base);
        _guard.set("TRACEVAULT_API_KEY", "tok");

        let result = switch("payments", false, None, tmp.path(), tmp.path()).await;
        assert!(
            result.is_ok(),
            "expected a no-session, non-`--user` switch to skip the codebase check: {result:?}"
        );

        let saved = crate::user_project_default::load();
        assert_eq!(
            saved.map(|b| b.project_name),
            Some("payments".to_string()),
            "no-session switch must fall back to the user-level default binding"
        );
    }

    fn items() -> Vec<ProjectListItem> {
        vec![
            ProjectListItem {
                id: uuid::Uuid::from_u128(1),
                name: "payments".into(),
            },
            ProjectListItem {
                id: uuid::Uuid::from_u128(2),
                name: "web".into(),
            },
        ]
    }

    /// VIS-316 pin: the project `project status` reports as effective is the
    /// project ingest attributes events to. For each configuration, the id
    /// `status` would treat as effective (its `effective_capture_project`
    /// binding, dropped when the id is not a UUID — exactly what `status`
    /// does) must equal `commands::stream::capture_project`. Both sides read
    /// the user default through `user_project_default::load()`, from a
    /// `user_project.toml` written under a temp config dir (`XDG_CONFIG_HOME`
    /// on Linux, `HOME` for macOS's `dirs::config_dir()`), with the env lock
    /// held for the whole test. Each case also asserts the expected id, so
    /// the equality can't pass vacuously with both sides `None`.
    #[tokio::test]
    async fn project_status_effective_project_matches_capture_project() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", tmp.path());
        _guard.set("HOME", tmp.path());

        let u = uuid::Uuid::from_u128;
        let bind = |id: String| ProjectBinding {
            project_id: id,
            project_name: "n".into(),
            updated_at: "".into(),
        };
        let worktree = "/wt";
        let subagent_session = {
            let mut s = SessionState {
                active_project: Some(bind(u(2).to_string())),
                ..Default::default()
            };
            s.subagent_projects
                .insert(worktree.into(), bind(u(1).to_string()));
            s
        };
        let active_session = SessionState {
            active_project: Some(bind(u(2).to_string())),
            ..Default::default()
        };
        let bad_active_session = SessionState {
            active_project: Some(bind("not-a-uuid".into())),
            ..Default::default()
        };

        // (label, session, user default written to user_project.toml, expected id)
        let cases: Vec<(&str, SessionState, Option<String>, Option<uuid::Uuid>)> = vec![
            (
                "(a) subagent override + session active + user default",
                subagent_session,
                Some(u(3).to_string()),
                Some(u(1)),
            ),
            (
                "(b) session active + user default",
                active_session,
                Some(u(3).to_string()),
                Some(u(2)),
            ),
            (
                "(c) user default only",
                SessionState::default(),
                Some(u(3).to_string()),
                Some(u(3)),
            ),
            ("(d) nothing", SessionState::default(), None, None),
            (
                "(e) user default with a non-UUID project_id",
                SessionState::default(),
                Some("not-a-uuid".into()),
                None,
            ),
            (
                "(f) non-UUID session active shadows a valid user default",
                bad_active_session,
                Some(u(3).to_string()),
                None,
            ),
        ];

        for (label, session, user_default, expected) in cases {
            match &user_default {
                Some(id) => crate::user_project_default::save(&bind(id.clone())).unwrap(),
                None => crate::user_project_default::clear().unwrap(),
            }
            let status_pid = effective_capture_project(
                None,
                &session,
                Some(worktree),
                crate::user_project_default::load(),
            )
            .and_then(|(b, _)| b.project_id.parse::<uuid::Uuid>().ok());
            let ingest_pid = crate::commands::stream::capture_project(&session, Some(worktree));
            assert_eq!(
                status_pid, ingest_pid,
                "{label}: status and ingest disagree"
            );
            assert_eq!(status_pid, expected, "{label}");
        }
        crate::user_project_default::clear().unwrap();
    }

    /// F1: `status` is a read-only inspector — an ambiguous ("this repo
    /// belongs to multiple projects") deduction is reported on the deduction
    /// line, not propagated up through `run`/`main` as a fatal exit. Mocks
    /// the `/projects/resolve` endpoint with a 409, mirroring resolution.rs's
    /// `ambiguous_deduction_errors_when_no_higher_rung`.
    #[tokio::test]
    async fn status_reports_ambiguous_deduction_as_informational_not_fatal() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(tmp.path());
        let ok = std::process::Command::new("git")
            .args([
                "-C",
                &tmp.path().to_string_lossy(),
                "remote",
                "add",
                "origin",
                "git@github.com:org/repo.git",
            ])
            .status()
            .expect("git remote add failed")
            .success();
        assert!(ok, "git remote add must succeed");

        let base = spawn_once(
            "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: 20\r\nConnection: close\r\n\r\n{\"error\":\"multiple\"}"
                .to_string(),
        );

        // SAFETY: test-scoped env mutation, mirroring the precedent in
        // `commands::login`'s tests, restored in a guard so a panic in
        // `status` still cleans up the process env. `_env_lock` (taken
        // above) serializes this against any other test in the crate that
        // reads or sets TRACEVAULT_SERVER_URL/TRACEVAULT_API_KEY (e.g.
        // `switch_without_session_or_user_flag_skips_codebase_check`, whose
        // credential resolution would otherwise observe these values while
        // they're set here and get routed at this test's mock server instead
        // of its own).
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("TRACEVAULT_SERVER_URL", &base);
        _guard.set("TRACEVAULT_API_KEY", "tok");
        // No ambient user default: the effective project is unbound, so the
        // ambiguous deduction is the one that would decide attribution — the
        // case most tempting to treat as fatal.
        _guard.set("XDG_CONFIG_HOME", tmp.path());

        let result = status(None, None, tmp.path(), tmp.path()).await;

        assert!(
            result.is_ok(),
            "status must degrade gracefully on an ambiguous deduction, not propagate the error: {result:?}"
        );
    }
}
