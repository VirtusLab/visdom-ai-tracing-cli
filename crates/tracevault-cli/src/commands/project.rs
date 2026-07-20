//! `tracevault project` — workspace/detached-mode project-attribution
//! commands. Mirrors `commands::repo`'s `switch`/`status` structure, but binds
//! the project-attribution axis (design §7/Task 7) rather than the repo axis.

use std::collections::HashSet;
use std::path::Path;

use crate::api_client::{resolve_credentials, ApiClient, ProjectListItem};
use crate::resolution::{
    effective_project, git_remote_url, resolve_effective_project, ProjectResolveInputs,
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
        /// `--project` precedence tier instead of the session/config
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

/// Resolve credentials into an `ApiClient`.
async fn resolve_client(project_root: &Path) -> Result<ApiClient, Box<dyn std::error::Error>> {
    let (server_url, token) = resolve_credentials(project_root);
    let server_url = server_url
        .ok_or("no server URL configured: set TRACEVAULT_SERVER_URL or run `tracevault login`")?;
    Ok(ApiClient::new(&server_url, token.as_deref()))
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
    let client = resolve_client(project_root).await?;
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

/// If `effective`'s source is `Deduced`, its binding carries an empty
/// `project_name` (`resolve_effective_project` doesn't enrich it — see the
/// comment there). When a projects list is already available in this scope
/// (fetched for the `--project`/config-default lookups), use it to fill in
/// the friendly name for display; otherwise leave it empty and
/// `format_status` falls back to printing the id. Kept simple: this never
/// triggers an extra API call just for cosmetic enrichment.
fn enrich_deduced_name(
    effective: Option<(ProjectBinding, ProjectSource)>,
    items: Option<&[ProjectListItem]>,
) -> Option<(ProjectBinding, ProjectSource)> {
    effective.map(|(mut binding, source)| {
        if source == ProjectSource::Deduced && binding.project_name.is_empty() {
            if let Some(items) = items {
                if let Ok(id) = binding.project_id.parse::<uuid::Uuid>() {
                    if let Some(matched) = items.iter().find(|p| p.id == id) {
                        binding.project_name = matched.name.clone();
                    }
                }
            }
        }
        (binding, source)
    })
}

/// Pure formatter for `project status`'s output: which project is
/// attributed, and via which precedence tier. Mirrors
/// `commands::repo::format_status`. A `Deduced` binding carries an empty
/// `project_name` (resolution.rs doesn't enrich it), so this falls back to
/// the id for display.
fn format_status(effective: Option<(&ProjectBinding, ProjectSource)>) -> String {
    match effective {
        Some((b, source)) => {
            let label = if b.project_name.is_empty() {
                &b.project_id
            } else {
                &b.project_name
            };
            format!("project: {label} via {source}")
        }
        None => "no project bound".to_string(),
    }
}

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

    // Resolving a project *name* (--project override, or the config-file
    // default_project) into a binding needs the server; running the full
    // rung 4/5 chain (deduction, user default) needs it too. Best-effort: no
    // client degrades to the pure local rungs (flag/config_default stay
    // unresolved) rather than failing the whole inspector.
    let effective = match resolve_client(project_root).await {
        Ok(client) => {
            let items = if project_flag_name.is_some() || config_default_name.is_some() {
                client.list_projects().await.ok()
            } else {
                None
            };
            let to_binding = |name: &str| -> Option<ProjectBinding> {
                let matched = items
                    .as_ref()
                    .and_then(|items| resolve_project_name(items, name).ok())?;
                Some(ProjectBinding {
                    project_id: matched.id.to_string(),
                    project_name: matched.name.clone(),
                    updated_at: chrono::Utc::now().to_rfc3339(),
                })
            };

            let project_flag = project_flag_name.and_then(to_binding);
            if let Some(name) = project_flag_name {
                if project_flag.is_none() {
                    eprintln!(
                        "warning: --project '{name}' could not be resolved; ignoring the override"
                    );
                }
            }
            let config_default = config_default_name.as_deref().and_then(to_binding);
            if let Some(name) = config_default_name.as_deref() {
                if config_default.is_none() {
                    eprintln!(
                        "warning: configured default_project '{name}' could not be resolved; ignoring it"
                    );
                }
            }

            let inputs = ProjectResolveInputs {
                project_flag,
                session: &session,
                worktree_path: Some(&worktree),
                config_default,
            };
            // `status` is a read-only inspector: unlike the callers that need
            // an authoritative binding to act on, an unresolvable rung here
            // (notably the ambiguous/409 "belongs to multiple projects" case)
            // is informational, not fatal — report it and exit 0 rather than
            // propagating the error up through `run`/`main` as a hard
            // failure. `resolve_effective_project` itself keeps returning
            // `Err` unchanged; only this call site swallows it.
            let resolved =
                match resolve_effective_project(&inputs, user_default, git_url.as_deref(), &client)
                    .await
                {
                    Ok(effective) => effective,
                    Err(e) => {
                        println!("project: unresolved — {e}");
                        return Ok(());
                    }
                };
            enrich_deduced_name(resolved, items.as_deref())
        }
        Err(e) => {
            eprintln!("warning: could not resolve credentials ({e}); showing local status only");
            let inputs = ProjectResolveInputs {
                project_flag: None,
                session: &session,
                worktree_path: Some(&worktree),
                config_default: None,
            };
            effective_project(&inputs)
        }
    };

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
    fn format_status_none() {
        assert_eq!(format_status(None), "no project bound");
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
    fn format_status_deduced_falls_back_to_id_when_name_empty() {
        let b = ProjectBinding {
            project_id: "deduced-id".into(),
            project_name: String::new(),
            updated_at: "".into(),
        };
        assert_eq!(
            format_status(Some((&b, ProjectSource::Deduced))),
            "project: deduced-id via repo deduction"
        );
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

    fn deduced(id: uuid::Uuid) -> (ProjectBinding, ProjectSource) {
        (
            ProjectBinding {
                project_id: id.to_string(),
                project_name: String::new(),
                updated_at: "".into(),
            },
            ProjectSource::Deduced,
        )
    }

    #[test]
    fn enrich_deduced_name_fills_in_name_when_id_found_in_list() {
        let effective = Some(deduced(uuid::Uuid::from_u128(2)));
        let (b, source) = enrich_deduced_name(effective, Some(&items())).unwrap();
        assert_eq!(b.project_name, "web");
        assert_eq!(source, ProjectSource::Deduced);
    }

    #[test]
    fn enrich_deduced_name_leaves_empty_when_no_list_available() {
        let effective = Some(deduced(uuid::Uuid::from_u128(2)));
        let (b, _source) = enrich_deduced_name(effective, None).unwrap();
        assert_eq!(b.project_name, "");
    }

    #[test]
    fn enrich_deduced_name_leaves_empty_when_id_not_in_list() {
        let effective = Some(deduced(uuid::Uuid::from_u128(999)));
        let (b, _source) = enrich_deduced_name(effective, Some(&items())).unwrap();
        assert_eq!(b.project_name, "");
    }

    #[test]
    fn enrich_deduced_name_leaves_non_deduced_sources_untouched() {
        // Only the Deduced source carries an empty name by design; other
        // sources must pass through unchanged even if a list is available.
        let b = pb("payments");
        let effective = Some((b.clone(), ProjectSource::SessionActive));
        let (out, source) = enrich_deduced_name(effective, Some(&items())).unwrap();
        assert_eq!(out, b);
        assert_eq!(source, ProjectSource::SessionActive);
    }

    #[test]
    fn enrich_deduced_name_passes_through_none() {
        assert!(enrich_deduced_name(None, Some(&items())).is_none());
    }

    /// F1: an ambiguous ("this repo belongs to multiple projects") deduction
    /// result is a hard `Err` from `resolve_effective_project` — but `status`
    /// is a read-only inspector, so it must swallow that into an
    /// informational line and still exit `Ok`, not propagate the error up
    /// through `run`/`main` as a fatal exit. Mocks the `/projects/resolve`
    /// endpoint with a 409, mirroring resolution.rs's
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

        let result = status(None, None, tmp.path(), tmp.path()).await;

        assert!(
            result.is_ok(),
            "status must degrade gracefully on an ambiguous deduction, not propagate the error: {result:?}"
        );
    }
}
