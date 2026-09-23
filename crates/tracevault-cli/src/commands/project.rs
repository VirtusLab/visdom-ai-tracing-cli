//! `tracevault project` — workspace/detached-mode project-attribution
//! commands. Mirrors `commands::repo`'s `switch`/`status` structure, but binds
//! the project-attribution axis (design §7/Task 7) rather than the repo axis.

use std::collections::HashSet;
use std::path::Path;

use crate::api_client::{resolve_client, ApiClient, ProjectListItem};
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
        /// Who owns attribution. `explicit` stamps the named project
        /// without a repo/project membership check and marks the session
        /// forced. `switch` itself checks nothing: the requirement — a
        /// Control Plane identity (never a `tvk_` API key) and Operator on
        /// the project — is enforced at INGEST, which refuses the force with
        /// a 403 and falls back to repo-derived attribution. A persisted
        /// force lapses after ~one working day.
        #[arg(long, value_parser = ["derived", "explicit"], default_value = "derived")]
        project_attribution: String,
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
            project_attribution,
        } => {
            switch(
                &name,
                user,
                session_id.as_deref(),
                &project_attribution,
                project_root,
                cwd,
            )
            .await
        }
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
        forced_until: None,
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

/// Stamp or clear the persisted force on a binding about to be saved.
///
/// Clearing on a plain switch is deliberate: without it, a user who forced
/// once and later switched normally would keep forcing until the clock ran
/// out, which is exactly the forgotten-force failure the lifetime exists to
/// bound.
fn apply_force(mut binding: ProjectBinding, explicit: bool) -> ProjectBinding {
    binding.forced_until = explicit.then(|| {
        (chrono::Utc::now()
            + chrono::Duration::hours(crate::user_project_default::DEFAULT_FORCE_LIFETIME_HOURS))
        .to_rfc3339()
    });
    binding
}

/// The lapse detail for a binding that carries a PERSISTED force.
///
/// Only ever called when `forced_until.is_some()` (see
/// [`attribution_report`]): the `None` arm exists solely so the
/// unparseable-timestamp case can fail safe by rendering identically to
/// "no force", and must not be printed as a standalone verdict — the
/// process-wide `TRACEVAULT_PROJECT_ATTRIBUTION` force leaves no
/// `forced_until` anywhere, so "derived" here would contradict the header
/// every hook in that shell is actually sending.
fn format_force_line(forced_until: Option<&str>) -> String {
    match forced_until.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()) {
        Some(until) if until > chrono::Utc::now() => format!(
            "attribution: forced by this caller (membership not checked); lapses {}",
            until.to_rfc3339()
        ),
        Some(until) => format!(
            "attribution: derived (a force lapsed {}; membership is checked again)",
            until.to_rfc3339()
        ),
        None => "attribution: derived (TraceVault checks repo/project membership)".to_string(),
    }
}

/// The attribution lines both `project status` and `project switch` print
/// for the binding they just reported or persisted.
///
/// The FIRST line is the effective mode — the thing that actually decides the
/// header — which incorporates `TRACEVAULT_PROJECT_ATTRIBUTION` as well as
/// `b`'s own `forced_until`. The lapse detail follows ONLY when `b` really
/// carries a persisted force, because that is the only case it describes.
///
/// Printing `format_force_line` unconditionally was the bug this replaces: in
/// a shell with `TRACEVAULT_PROJECT_ATTRIBUTION=explicit` and no persisted
/// force, the two lines flatly contradicted each other —
///
/// ```text
/// attribution mode: explicit (this caller owns attribution; ...)
/// attribution: derived (TraceVault checks repo/project membership)
/// ```
///
/// — while every hook in that shell sent `explicit`. `switch` had the same
/// lie with no mode line at all to offset it.
fn attribution_report(b: &ProjectBinding) -> Vec<String> {
    let mode = crate::commands::stream::attribution_mode(Some(b));
    // The gloss `format_force_line` used to carry is folded in here rather
    // than dropped: it is the part that tells a reader what the mode MEANS,
    // and it applies to every mode, not only to a persisted force.
    let gloss = if mode == "explicit" {
        "this caller owns attribution; membership is not checked"
    } else {
        "TraceVault checks repo/project membership"
    };
    let mut lines = vec![format!("attribution mode: {mode} ({gloss})")];
    if b.forced_until.is_some() {
        lines.push(format_force_line(b.forced_until.as_deref()));
    }
    lines
}

/// Everything `switch` prints once the binding is persisted: what was bound
/// where, then [`attribution_report`]. Pulled out as a pure function so the
/// wording — including the ABSENCE of a contradictory "attribution: derived"
/// line — is assertable without capturing stdout.
fn switch_report(dest: &SwitchDest, b: &ProjectBinding) -> Vec<String> {
    let mut lines = match dest {
        SwitchDest::Session(id) => {
            vec![format!("bound session {id} to project {}", b.project_name)]
        }
        SwitchDest::UserDefault => vec![format!(
            "set user-level default project {}; applies to new sessions without their own binding (the current session, if any, is unchanged — omit --user to bind this session)",
            b.project_name
        )],
    };
    lines.extend(attribution_report(b));
    lines
}

async fn switch(
    name: &str,
    user: bool,
    session_id: Option<&str>,
    project_attribution: &str,
    project_root: &Path,
    cwd: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = resolve_client(project_root)?;
    let session = crate::commands::repo::resolve_session_id(session_id).ok();
    let dest = switch_destination(user, session);
    let check_codebase = matches!(dest, SwitchDest::Session(_));
    let binding = resolve_switch_project(name, &client, check_codebase, cwd).await?;
    let binding = apply_force(binding, project_attribution == "explicit");

    match &dest {
        SwitchDest::Session(id) => {
            let mut state = session_state::load(id);
            state.active_project = Some(binding.clone());
            session_state::save(id, &state)?;
        }
        SwitchDest::UserDefault => {
            crate::user_project_default::save(&binding)?;
        }
    }
    for line in switch_report(&dest, &binding) {
        println!("{line}");
    }
    Ok(())
}

/// Fill in a binding's friendly `project_name` from an already-fetched
/// projects list, when the binding has none.
///
/// Two tiers arrive nameless: `Deduced` (`resolve_effective_project` doesn't
/// enrich it — see the comment there) and the UUID form of
/// `TRACEVAULT_PROJECT`, which is parsed locally and never looked up. This
/// used to gate on `source == Deduced`, which meant `status` broadened its
/// `items` fetch for any `TRACEVAULT_PROJECT` and then threw the result away
/// for the UUID form, printing a bare id. Gating on the EMPTY NAME instead —
/// the condition actually being repaired — covers both without special-casing
/// either. A binding that already has a name is untouched.
///
/// Kept simple: this never triggers an extra API call just for cosmetic
/// enrichment; if no list is in scope, the name stays empty and
/// `format_status` falls back to printing the id.
fn enrich_project_name(
    effective: Option<(ProjectBinding, ProjectSource)>,
    items: Option<&[ProjectListItem]>,
) -> Option<(ProjectBinding, ProjectSource)> {
    effective.map(|(mut binding, source)| {
        if binding.project_name.is_empty() {
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

/// The `project status` annotation for a `TRACEVAULT_PROJECT` that holds a
/// NAME rather than a UUID.
///
/// Mirrors the `Env` arm of `commands::status`'s `project_binding_check`, and
/// exists for the same reason: the name resolves HERE (this command has a
/// client) and nowhere on the capture path, so reporting the `Env` tier
/// without qualification claims a tier the wire ignores. `uuid_form` is the
/// capture path's own verdict — `env_project_binding().is_some()` — so the
/// two surfaces cannot drift.
fn env_name_form_note(source: ProjectSource, uuid_form: bool) -> Option<String> {
    (source == ProjectSource::Env && !uuid_form).then(|| {
        "note: TRACEVAULT_PROJECT holds a NAME, resolved here for display only; the capture \
         path honours only the UUID form, so hooks in this shell attribute via the next tier"
            .to_string()
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
    match resolve_status_effective(session_id, project_flag_name, project_root, cwd).await {
        Ok(effective) => {
            println!(
                "{}",
                format_status(effective.as_ref().map(|(b, s)| (b, *s)))
            );
            if let Some((b, source)) = &effective {
                if let Some(note) = env_name_form_note(
                    *source,
                    crate::commands::stream::env_project_binding().is_some(),
                ) {
                    println!("{note}");
                }
                for line in attribution_report(b) {
                    println!("{line}");
                }
            }
        }
        // `status` is a read-only inspector: unlike the callers that need an
        // authoritative binding to act on, an unresolvable rung here (notably
        // the ambiguous/409 "belongs to multiple projects" case) is
        // informational, not fatal — report it and exit 0 rather than
        // propagating the error up through `run`/`main` as a hard failure.
        Err(e) => println!("project: unresolved — {e}"),
    }
    Ok(())
}

/// The resolution core of `status`, pulled out so it can be exercised
/// directly in tests (notably for `TRACEVAULT_PROJECT`) without capturing
/// stdout: `status` itself is a thin wrapper that calls this, then prints
/// `format_status` of the result (or the informational "unresolved" line on
/// `Err`).
///
/// This chain is deliberately RICHER than the one `capture_project` runs at
/// send time — it has a client, so it also resolves names and can deduce
/// from the repo — which means the binding it picks, and therefore the
/// attribution mode reported from that binding's `forced_until`, is
/// indicative of what a hook will do rather than authoritative about it.
async fn resolve_status_effective(
    session_id: Option<&str>,
    project_flag_name: Option<&str>,
    project_root: &Path,
    cwd: &Path,
) -> Result<Option<(ProjectBinding, ProjectSource)>, Box<dyn std::error::Error>> {
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
    let effective = match resolve_client(project_root) {
        Ok(client) => {
            let items = if project_flag_name.is_some()
                || config_default_name.is_some()
                || std::env::var("TRACEVAULT_PROJECT").is_ok()
            {
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
                    forced_until: None,
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

            let env_project = match std::env::var("TRACEVAULT_PROJECT").ok() {
                Some(raw) if !raw.trim().is_empty() => {
                    let raw = raw.trim().to_string();
                    match raw.parse::<uuid::Uuid>() {
                        Ok(id) => Some(ProjectBinding {
                            project_id: id.to_string(),
                            project_name: String::new(),
                            updated_at: String::new(),
                            forced_until: None,
                        }),
                        // A name: resolvable here because `status` already has a client.
                        Err(_) => {
                            let resolved = to_binding(&raw);
                            // Mirrors the `--project` and `default_project`
                            // warnings above. The design's failure-mode table
                            // calls for "CLI resolution error at the command"
                            // here; dropping it silently left the operator
                            // staring at whichever lower tier won instead.
                            if resolved.is_none() {
                                eprintln!(
                                    "warning: TRACEVAULT_PROJECT '{raw}' could not be resolved; ignoring it"
                                );
                            }
                            resolved
                        }
                    }
                }
                _ => None,
            };

            let inputs = ProjectResolveInputs {
                project_flag,
                env_project,
                session: &session,
                worktree_path: Some(&worktree),
                config_default,
            };
            let resolved =
                resolve_effective_project(&inputs, user_default, git_url.as_deref(), &client)
                    .await?;
            enrich_project_name(resolved, items.as_deref())
        }
        Err(e) => {
            eprintln!("warning: could not resolve credentials ({e}); showing local status only");
            // No client here, so — unlike the branch above — only the UUID
            // form of `TRACEVAULT_PROJECT` can be honoured: a name needs
            // `list_projects`, which needs a client.
            let env_project = std::env::var("TRACEVAULT_PROJECT")
                .ok()
                .and_then(|raw| raw.trim().parse::<uuid::Uuid>().ok())
                .map(|id| ProjectBinding {
                    project_id: id.to_string(),
                    project_name: String::new(),
                    updated_at: String::new(),
                    forced_until: None,
                });
            let inputs = ProjectResolveInputs {
                project_flag: None,
                env_project,
                session: &session,
                worktree_path: Some(&worktree),
                config_default: None,
            };
            effective_project(&inputs)
        }
    };

    Ok(effective)
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

    #[test]
    fn switching_with_explicit_stamps_a_lapse_time() {
        let before = chrono::Utc::now();
        let binding = apply_force(
            ProjectBinding {
                project_id: "3f2504e0-4f89-11d3-9a0c-0305e82c3301".into(),
                project_name: "p".into(),
                updated_at: before.to_rfc3339(),
                forced_until: None,
            },
            true,
        );
        let until = chrono::DateTime::parse_from_rfc3339(
            binding.forced_until.as_deref().expect("force is stamped"),
        )
        .unwrap();
        let expected = before
            + chrono::Duration::hours(crate::user_project_default::DEFAULT_FORCE_LIFETIME_HOURS);
        assert!((until.timestamp() - expected.timestamp()).abs() < 5);
    }

    #[test]
    fn switching_without_explicit_stamps_no_lapse_time() {
        let binding = apply_force(
            ProjectBinding {
                project_id: "3f2504e0-4f89-11d3-9a0c-0305e82c3301".into(),
                project_name: "p".into(),
                updated_at: chrono::Utc::now().to_rfc3339(),
                forced_until: Some("2020-01-01T00:00:00Z".into()),
            },
            false,
        );
        assert_eq!(
            binding.forced_until, None,
            "switching without --project-attribution explicit must CLEAR a stale force"
        );
    }

    /// Renamed from `status_names_the_mode_and_the_lapse`, which claimed to
    /// cover the mode line and never called `attribution_mode` at all — it
    /// would have kept passing if the mode line were deleted outright. The
    /// mode line is covered by the `attribution_report_*` tests below; this
    /// one covers exactly what it exercises, `format_force_line`'s two
    /// lapse states.
    #[test]
    fn format_force_line_names_a_live_force_and_the_unforced_default() {
        let future = (chrono::Utc::now() + chrono::Duration::hours(4)).to_rfc3339();
        let line = format_force_line(Some(&future));
        assert!(line.contains("forced"), "got: {line}");

        let derived = format_force_line(None);
        assert!(derived.contains("derived"), "got: {derived}");
    }

    /// Discriminates the SIGN of the lapse comparison in `format_force_line`:
    /// a `forced_until` already in the past must report as lapsed (back to
    /// `derived`), not as still-forced. Complements
    /// `format_force_line_names_a_live_force_and_the_unforced_default`,
    /// which only exercises the future/none cases.
    #[test]
    fn status_names_a_lapsed_force_as_derived_not_forced() {
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let line = format_force_line(Some(&past));
        assert!(line.contains("derived"), "got: {line}");
        assert!(!line.contains("forced by"), "got: {line}");
    }

    /// A `forced_until` that doesn't even parse as RFC3339 (a hand-edited or
    /// corrupted `user_project.toml`/session-state file) must fail SAFE —
    /// reported exactly like `None`, never as forced, and never a panic.
    /// Pins the fail-safe direction against a future refactor to
    /// `.expect(...)`; complements `commands::stream::attribution_mode`'s
    /// equivalent test, since both are readers of this same field.
    #[test]
    fn status_names_unparseable_forced_until_as_derived() {
        let line = format_force_line(Some("not-a-timestamp"));
        assert_eq!(line, format_force_line(None));
    }

    /// VIS-305 whole-branch review, Important finding 3: `project status`
    /// printed the effective mode and THEN printed `format_force_line`
    /// unconditionally, so a shell with
    /// `TRACEVAULT_PROJECT_ATTRIBUTION=explicit` and no persisted force got
    /// two lines that contradicted each other — "attribution mode: explicit"
    /// immediately followed by "attribution: derived (TraceVault checks
    /// repo/project membership)" — while every hook in that shell sent
    /// `explicit`. The lapse detail describes a PERSISTED force, so it may
    /// only be printed when one exists.
    #[test]
    fn attribution_report_omits_the_lapse_line_when_there_is_no_persisted_force() {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("TRACEVAULT_PROJECT_ATTRIBUTION", "explicit");

        let b = ProjectBinding {
            project_id: "3f2504e0-4f89-11d3-9a0c-0305e82c3301".into(),
            project_name: "payments".into(),
            updated_at: "".into(),
            forced_until: None,
        };
        let lines = attribution_report(&b);
        assert_eq!(
            lines.len(),
            1,
            "an env-only force has no lapse to report, and must not be \
             contradicted by a `derived` line: {lines:?}"
        );
        assert!(
            lines[0].starts_with("attribution mode: explicit"),
            "got: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("derived")),
            "got: {lines:?}"
        );
    }

    /// The other half of the same rule: a binding that DOES carry a
    /// persisted force still gets its lapse time, so finding 3's fix is
    /// "print it only when it applies", not "stop printing it".
    #[test]
    fn attribution_report_keeps_the_lapse_line_for_a_persisted_force() {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.remove("TRACEVAULT_PROJECT_ATTRIBUTION");

        let until = (chrono::Utc::now() + chrono::Duration::hours(4)).to_rfc3339();
        let b = ProjectBinding {
            project_id: "3f2504e0-4f89-11d3-9a0c-0305e82c3301".into(),
            project_name: "payments".into(),
            updated_at: "".into(),
            forced_until: Some(until.clone()),
        };
        let lines = attribution_report(&b);
        assert_eq!(lines.len(), 2, "got: {lines:?}");
        assert!(
            lines[0].starts_with("attribution mode: explicit"),
            "got: {}",
            lines[0]
        );
        assert!(lines[1].contains("lapses"), "got: {}", lines[1]);
        assert!(lines[1].contains(&until), "got: {}", lines[1]);
    }

    /// A LAPSED persisted force is the one case where "attribution: derived"
    /// is true AND worth printing: the binding really does carry a force,
    /// it has simply run out, and saying when is the useful part.
    #[test]
    fn attribution_report_reports_a_lapsed_persisted_force_as_derived() {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.remove("TRACEVAULT_PROJECT_ATTRIBUTION");

        let b = ProjectBinding {
            project_id: "3f2504e0-4f89-11d3-9a0c-0305e82c3301".into(),
            project_name: "payments".into(),
            updated_at: "".into(),
            forced_until: Some((chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339()),
        };
        let lines = attribution_report(&b);
        assert_eq!(lines.len(), 2, "got: {lines:?}");
        assert!(
            lines[0].starts_with("attribution mode: derived"),
            "got: {}",
            lines[0]
        );
        assert!(lines[1].contains("a force lapsed"), "got: {}", lines[1]);
    }

    /// VIS-305 whole-branch review, Important finding 4: Part C fixed
    /// `status` and left `switch` printing the same lie with no mode line to
    /// offset it. In a shell with `TRACEVAULT_PROJECT_ATTRIBUTION=explicit`,
    /// `tracevault project switch foo` reported "attribution: derived" while
    /// every hook in that shell sent `explicit`. Asserts the whole rendered
    /// output, including the ABSENCE of the contradictory line, for both
    /// destinations.
    #[test]
    fn switch_report_names_the_env_force_and_never_claims_derived() {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("TRACEVAULT_PROJECT_ATTRIBUTION", "explicit");

        let b = ProjectBinding {
            project_id: "3f2504e0-4f89-11d3-9a0c-0305e82c3301".into(),
            project_name: "payments".into(),
            updated_at: "".into(),
            forced_until: None,
        };

        for dest in [
            SwitchDest::Session("sess-1".to_string()),
            SwitchDest::UserDefault,
        ] {
            let lines = switch_report(&dest, &b);
            assert!(
                lines[0].contains("payments"),
                "the first line still says what was bound: {lines:?}"
            );
            assert!(
                lines
                    .iter()
                    .any(|l| l.starts_with("attribution mode: explicit")),
                "a `switch` in an explicit shell must say so: {lines:?}"
            );
            assert!(
                !lines.iter().any(|l| l.contains("derived")),
                "{dest:?}: `switch` must not claim membership is checked while \
                 every hook in this shell sends `explicit`: {lines:?}"
            );
        }
    }

    /// `switch --project-attribution explicit` still reports the lapse it
    /// just stamped — the fix to finding 4 must not swallow the one piece of
    /// information a forced switch owes the caller.
    #[test]
    fn switch_report_names_the_lapse_it_just_stamped() {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.remove("TRACEVAULT_PROJECT_ATTRIBUTION");

        let b = apply_force(
            ProjectBinding {
                project_id: "3f2504e0-4f89-11d3-9a0c-0305e82c3301".into(),
                project_name: "payments".into(),
                updated_at: "".into(),
                forced_until: None,
            },
            true,
        );
        let lines = switch_report(&SwitchDest::Session("sess-1".to_string()), &b);
        assert_eq!(lines.len(), 3, "got: {lines:?}");
        assert!(
            lines[1].starts_with("attribution mode: explicit"),
            "got: {}",
            lines[1]
        );
        assert!(lines[2].contains("lapses"), "got: {}", lines[2]);
    }

    fn pb(name: &str) -> ProjectBinding {
        ProjectBinding {
            project_id: format!("id-{name}"),
            project_name: name.into(),
            updated_at: "t".into(),
            forced_until: None,
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
            forced_until: None,
        };
        assert_eq!(
            format_status(Some((&b, ProjectSource::Deduced))),
            "project: deduced-id via repo deduction"
        );
    }

    #[test]
    fn format_status_env() {
        let b = pb("payments");
        assert_eq!(
            format_status(Some((&b, ProjectSource::Env))),
            "project: payments via TRACEVAULT_PROJECT (environment)"
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

        let result = switch("payments", false, None, "derived", tmp.path(), tmp.path()).await;
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
                forced_until: None,
            },
            ProjectSource::Deduced,
        )
    }

    #[test]
    fn enrich_project_name_fills_in_name_when_id_found_in_list() {
        let effective = Some(deduced(uuid::Uuid::from_u128(2)));
        let (b, source) = enrich_project_name(effective, Some(&items())).unwrap();
        assert_eq!(b.project_name, "web");
        assert_eq!(source, ProjectSource::Deduced);
    }

    #[test]
    fn enrich_project_name_leaves_empty_when_no_list_available() {
        let effective = Some(deduced(uuid::Uuid::from_u128(2)));
        let (b, _source) = enrich_project_name(effective, None).unwrap();
        assert_eq!(b.project_name, "");
    }

    #[test]
    fn enrich_project_name_leaves_empty_when_id_not_in_list() {
        let effective = Some(deduced(uuid::Uuid::from_u128(999)));
        let (b, _source) = enrich_project_name(effective, Some(&items())).unwrap();
        assert_eq!(b.project_name, "");
    }

    #[test]
    fn enrich_project_name_leaves_a_populated_name_untouched() {
        // A binding that already has a friendly name must pass through
        // unchanged, whatever its tier — enrichment repairs an EMPTY name
        // and nothing else.
        let b = pb("payments");
        let effective = Some((b.clone(), ProjectSource::SessionActive));
        let (out, source) = enrich_project_name(effective, Some(&items())).unwrap();
        assert_eq!(out, b);
        assert_eq!(source, ProjectSource::SessionActive);
    }

    /// Small finding 8: `status` broadens its `items` fetch for ANY
    /// `TRACEVAULT_PROJECT`, but the UUID form never goes through
    /// `to_binding`, so its binding arrives nameless. Gating enrichment on
    /// `source == Deduced` meant the list was fetched and thrown away and
    /// the operator saw a bare UUID.
    #[test]
    fn enrich_project_name_fills_in_an_env_sourced_uuid_binding() {
        let (mut b, _) = deduced(uuid::Uuid::from_u128(2));
        b.project_name = String::new();
        let effective = Some((b, ProjectSource::Env));
        let (out, source) = enrich_project_name(effective, Some(&items())).unwrap();
        assert_eq!(out.project_name, "web");
        assert_eq!(source, ProjectSource::Env);
    }

    #[test]
    fn enrich_project_name_passes_through_none() {
        assert!(enrich_project_name(None, Some(&items())).is_none());
    }

    /// Item 2, `project status`'s half: a NAME in `TRACEVAULT_PROJECT` is
    /// resolved here for display but ignored by the capture path, so the
    /// `Env` line must be annotated rather than reported flat — the same
    /// rule `commands::status`'s `project_binding_check` applies.
    #[test]
    fn env_name_form_note_fires_only_for_a_name_at_the_env_tier() {
        let note = env_name_form_note(ProjectSource::Env, false).expect("a NAME must be noted");
        assert!(note.contains("UUID form"), "got: {note}");
        assert!(note.contains("display only"), "got: {note}");

        assert!(
            env_name_form_note(ProjectSource::Env, true).is_none(),
            "the UUID form is honoured at capture time and needs no caveat"
        );
        assert!(
            env_name_form_note(ProjectSource::SessionActive, false).is_none(),
            "no other tier reads TRACEVAULT_PROJECT"
        );
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
        // `resolve_status_effective` reads `TRACEVAULT_PROJECT` at a rung
        // ABOVE deduction, so an ambient export would resolve the binding
        // locally and this test would never reach the 409 it exists to
        // exercise — passing for the wrong reason.
        _guard.remove("TRACEVAULT_PROJECT");

        let result = status(None, None, tmp.path(), tmp.path()).await;

        assert!(
            result.is_ok(),
            "status must degrade gracefully on an ambiguous deduction, not propagate the error: {result:?}"
        );
    }

    /// A3, UUID form: with a client available, `TRACEVAULT_PROJECT` set to a
    /// UUID must still resolve at the `Env` rung, not by falling through to
    /// deduction/user-default. `spawn_once`'s listener only ever answers one
    /// request — the broadened `items` fetch this env var now triggers — so
    /// if resolution regressed to skipping the local `Env` rung and instead
    /// fell through to `resolve_project` (deduction), the second HTTP call
    /// would find no listener and the test would fail on that, not just on
    /// the source assertion below.
    #[tokio::test]
    async fn status_resolves_env_uuid_as_the_effective_source() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();

        let list =
            r#"[{"id":"11111111-1111-4111-8111-111111111111","name":"payments"}]"#.to_string();
        let base = spawn_once(http_200(&list));

        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", tmp.path());
        _guard.set("TRACEVAULT_SERVER_URL", &base);
        _guard.set("TRACEVAULT_API_KEY", "tok");
        let uuid = "22222222-2222-4222-8222-222222222222";
        _guard.set("TRACEVAULT_PROJECT", uuid);

        let (b, source) = resolve_status_effective(None, None, tmp.path(), tmp.path())
            .await
            .unwrap()
            .expect("TRACEVAULT_PROJECT must resolve to a binding");
        assert_eq!(source, ProjectSource::Env);
        assert_eq!(b.project_id, uuid);
    }

    /// A3, name form: unlike the capture path (`stream::env_project_binding`,
    /// UUID-only), `status` already has a client in scope, so a NAME in
    /// `TRACEVAULT_PROJECT` must resolve via `list_projects` — the same
    /// one-shot mock response the broadened `items` fetch consumes, so no
    /// second request is made.
    #[tokio::test]
    async fn status_resolves_env_name_via_the_client() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();

        let list =
            r#"[{"id":"11111111-1111-4111-8111-111111111111","name":"payments"}]"#.to_string();
        let base = spawn_once(http_200(&list));

        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", tmp.path());
        _guard.set("TRACEVAULT_SERVER_URL", &base);
        _guard.set("TRACEVAULT_API_KEY", "tok");
        _guard.set("TRACEVAULT_PROJECT", "payments");

        let (b, source) = resolve_status_effective(None, None, tmp.path(), tmp.path())
            .await
            .unwrap()
            .expect("a name should resolve via list_projects");
        assert_eq!(source, ProjectSource::Env);
        assert_eq!(b.project_name, "payments");
        assert_eq!(b.project_id, "11111111-1111-4111-8111-111111111111");
    }

    /// A3, item 3: with credentials unresolved (no client at all — the
    /// `Err` arm of `resolve_client`), `status`'s resolution core must still
    /// honour the UUID form of `TRACEVAULT_PROJECT`, which needs no server
    /// call. Only the NAME form is unavailable on this branch (it would need
    /// `list_projects`), which is unchanged/untested here since it was
    /// already correctly unresolved before this fix.
    #[tokio::test]
    async fn status_resolves_env_uuid_even_without_a_client() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();

        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", tmp.path());
        _guard.remove("TRACEVAULT_SERVER_URL");
        _guard.remove("TRACEVAULT_API_KEY");
        let uuid = "33333333-3333-4333-8333-333333333333";
        _guard.set("TRACEVAULT_PROJECT", uuid);

        let (b, source) = resolve_status_effective(None, None, tmp.path(), tmp.path())
            .await
            .unwrap()
            .expect("TRACEVAULT_PROJECT (UUID form) must resolve even without a client");
        assert_eq!(source, ProjectSource::Env);
        assert_eq!(b.project_id, uuid);
    }

    /// VIS-305 Part C review, small finding 3: `project status` must report
    /// the EFFECTIVE attribution mode, not just the winning binding's own
    /// `forced_until`. `TRACEVAULT_PROJECT_ATTRIBUTION=explicit` drives
    /// `explicit` on its own, with no persisted force anywhere — a status
    /// line built from `format_force_line` alone would show "derived" here,
    /// which is exactly the lie item 3 flags: every hook in this shell is
    /// actually sending `explicit`.
    #[tokio::test]
    async fn status_reports_explicit_via_env_even_when_the_binding_itself_is_not_forced() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();

        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", tmp.path());
        _guard.remove("TRACEVAULT_SERVER_URL");
        _guard.remove("TRACEVAULT_API_KEY");
        let uuid = "44444444-4444-4444-8444-444444444444";
        _guard.set("TRACEVAULT_PROJECT", uuid);
        _guard.set("TRACEVAULT_PROJECT_ATTRIBUTION", "explicit");

        let (b, _source) = resolve_status_effective(None, None, tmp.path(), tmp.path())
            .await
            .unwrap()
            .expect("TRACEVAULT_PROJECT (UUID form) must resolve even without a client");
        assert_eq!(
            b.forced_until, None,
            "the env-derived binding itself carries no persisted force"
        );
        assert_eq!(
            crate::commands::stream::attribution_mode(Some(&b)),
            "explicit",
            "the EFFECTIVE mode must reflect TRACEVAULT_PROJECT_ATTRIBUTION even when \
             the winning binding has no force of its own"
        );
    }
}
