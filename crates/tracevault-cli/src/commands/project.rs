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
        /// Who owns attribution. `explicit` stamps the named project
        /// without a repo/project membership check and marks the session
        /// forced — so it also binds a project this checkout is not a member
        /// of, which an ordinary switch refuses; it says so when it does.
        /// `switch` itself checks nothing: the requirement — a Control Plane
        /// identity (never a `tvk_` API key) and Operator on the project —
        /// is enforced at INGEST, which refuses the force with a 403 and
        /// queues the event rather than re-attributing it. A persisted force
        /// lapses after ~one working day.
        #[arg(long, value_parser = ["derived", "explicit"], default_value = "derived")]
        project_attribution: String,
    },
    /// Show the project the current session is attributed to.
    Status {
        /// Session to target; defaults to $TRACEVAULT_SESSION_ID.
        #[arg(long)]
        session_id: Option<String>,
        /// What-if, display only: resolve this project name and feed it in at
        /// the top of the precedence chain, above the session and user-default
        /// bindings, to preview what `status` would report. It binds nothing;
        /// use `project switch` to change what events are attributed to.
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

/// What a `switch` does about a project that does not contain the current
/// checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodebaseCheck {
    /// Not performed at all: the user-level default is session-independent
    /// and not tied to any checkout, so there is nothing to check it against.
    Skip,
    /// An ordinary session switch. Binding a project this checkout is not a
    /// member of is almost always a typo, so it is an error.
    Enforce,
    /// `--project-attribution explicit`. Binding a project this checkout is
    /// not a member of is the POINT of the flag — a repo deliberately shared
    /// by two projects, or a launcher attributing work to a project the
    /// checkout was never registered under. The requirement is a trust claim
    /// the SERVER enforces at ingest (Operator on the project, and a Control
    /// Plane identity), not something the client can or should adjudicate at
    /// switch time; refusing here contradicts both the flag's help text and
    /// the design. So: report the fact, never block on it.
    Report,
}

/// Which [`CodebaseCheck`] a `switch` runs, from where it is persisting and
/// whether the caller claimed attribution. The one place the rule is written
/// down, so `switch` and its test cannot disagree about it.
fn codebase_check_for(dest: &SwitchDest, explicit: bool) -> CodebaseCheck {
    match (dest, explicit) {
        // Session-independent: not tied to any checkout, nothing to check.
        (SwitchDest::UserDefault, _) => CodebaseCheck::Skip,
        (SwitchDest::Session(_), true) => CodebaseCheck::Report,
        (SwitchDest::Session(_), false) => CodebaseCheck::Enforce,
    }
}

/// Resolve `name` to a registered project (via `list_projects`) and, unless
/// `check` is [`CodebaseCheck::Skip`], test whether that project contains the
/// current codebase (resolved from `cwd`'s git origin remote, mirroring
/// `resolve_path_to_binding`). Kept separate from `switch` so the
/// client-dependent flow is unit-testable with a mock `ApiClient`, mirroring
/// `commands::repo::resolve_switch_binding`.
///
/// Returns the binding plus, for [`CodebaseCheck::Report`] when the check
/// would have failed, the informational line the caller prints. A failing
/// check under [`CodebaseCheck::Enforce`] is still an `Err`.
async fn resolve_switch_project(
    name: &str,
    client: &ApiClient,
    check: CodebaseCheck,
    cwd: &Path,
) -> Result<(ProjectBinding, Option<String>), Box<dyn std::error::Error>> {
    let items = client.list_projects().await?;
    let matched = resolve_project_name(&items, name)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let project_id = matched.id;
    let project_name = matched.name.clone();
    let mut note = None;

    if check != CodebaseCheck::Skip {
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
                    match check {
                        CodebaseCheck::Enforce => {
                            return Err(format!(
                                "project '{name}' does not contain the current codebase"
                            )
                            .into())
                        }
                        // Deliberate, but the user should still learn the
                        // fact — they are just not blocked by it.
                        CodebaseCheck::Report => note = Some(forced_non_member_note(name)),
                        CodebaseCheck::Skip => unreachable!("guarded above"),
                    }
                }
            }
            // Codebase not registered with the server → nothing to check
            // against; allow the switch (mirrors resolve_path_to_binding's
            // Ok(None) for an untracked remote).
        }
        // No git origin remote at all (workspace mode, no checkout) → nothing
        // to check against; allow the switch.
    }

    Ok((
        ProjectBinding {
            project_id: project_id.to_string(),
            project_name,
            updated_at: chrono::Utc::now().to_rfc3339(),
            forced_until: None,
        },
        note,
    ))
}

/// The line a forced switch prints when the project does not contain this
/// checkout. Not a warning: with `--project-attribution explicit` this is the
/// intended case, and the message says so — while still naming the fact, and
/// where the requirement is actually enforced, so a user who reached it by
/// typo is not left thinking the switch did what they meant.
fn forced_non_member_note(name: &str) -> String {
    format!(
        "note: project '{name}' does not contain the current codebase — binding it anyway \
         because --project-attribution explicit says this caller owns attribution. The \
         server enforces that claim at ingest (Operator on the project, and a Control Plane \
         identity — never a `tvk_` API key) and refuses it with a 403 otherwise."
    )
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

/// The state of the PERSISTED force stored on a binding — a fact about what
/// is on disk, never a verdict about what this process will send.
///
/// That distinction is the whole point of the wording. The verdict is
/// [`attribution_report`]'s first line, which is the only thing that can
/// answer "derived or explicit?", because it is the only one that sees
/// `TRACEVAULT_PROJECT_ATTRIBUTION` as well as this timestamp. An earlier
/// version of this function rendered "attribution: derived (a force lapsed
/// …; membership is checked again)", which read as a second, competing
/// answer — and in a shell with `TRACEVAULT_PROJECT_ATTRIBUTION=explicit`
/// and a lapsed persisted force it directly contradicted the true one while
/// every hook in that shell sent `explicit`. Stating the fact instead makes
/// the contradiction unrepresentable, whatever the environment says.
///
/// Only ever called when `forced_until.is_some()` (see
/// [`attribution_report`]), so there is no "no force at all" arm: that case
/// prints no line.
///
/// This only WORDS a verdict; it does not reach one.
/// [`crate::session_state::force_status`] is the single parse-and-compare,
/// shared with `commands::stream::attribution_mode`, which is the function
/// that actually decides the header — so the line and the header cannot
/// disagree about whether a stored force is live, lapsed or unreadable. An
/// unparseable timestamp (hand-edited or corrupted state file) therefore
/// fails SAFE here too, and says so.
fn format_force_line(forced_until: &str) -> String {
    match crate::session_state::force_status(forced_until) {
        crate::session_state::ForceStatus::Live(until) => {
            format!("persisted force: active until {}", until.to_rfc3339())
        }
        crate::session_state::ForceStatus::Lapsed(until) => {
            format!("persisted force: lapsed {}", until.to_rfc3339())
        }
        crate::session_state::ForceStatus::Unreadable => format!(
            "persisted force: unreadable timestamp ('{forced_until}'), treated as not in force"
        ),
    }
}

/// The attribution lines both `project status` and `project switch` print
/// for the binding they just reported or persisted.
///
/// Exactly one line answers "derived or explicit?": the FIRST, the effective
/// mode, which is the only one that sees both `TRACEVAULT_PROJECT_ATTRIBUTION`
/// and `b`'s own `forced_until` — i.e. everything
/// `commands::stream::attribution_mode` sees when it fills the header.
/// [`format_force_line`] follows only when `b` carries a persisted force, and
/// states a FACT about that stored force rather than a second verdict, so the
/// two can never disagree.
///
/// Two bugs are closed by that split, both reached the same way — a second
/// line that answered a question it had no business answering:
///
/// * printing the force line unconditionally, so a shell with
///   `TRACEVAULT_PROJECT_ATTRIBUTION=explicit` and NO persisted force got
///   "attribution mode: explicit …" followed by "attribution: derived
///   (TraceVault checks repo/project membership)" (finding 3);
/// * guarding it on `forced_until.is_some()`, which a LAPSED force still
///   satisfies, so the same shell with an EXPIRED persisted force got
///   "attribution mode: explicit …" followed by "attribution: derived (a
///   force lapsed …; membership is checked again)" (Copilot on PR #54).
///
/// In both cases every hook in that shell was in fact sending `explicit`.
/// The `is_some()` guard is right and stays — a binding that was never
/// forced has nothing to report — but the guard was never what made the
/// output honest; the wording is.
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
    if let Some(forced_until) = b.forced_until.as_deref() {
        lines.push(format_force_line(forced_until));
    }
    lines
}

/// Everything `switch` prints once the binding is persisted: what was bound
/// where, then `codebase_note` (see [`forced_non_member_note`]) when a forced
/// switch bound a project this checkout is not a member of, then
/// [`attribution_report`]. Pulled out as a pure function so the wording —
/// including the ABSENCE of a contradictory "attribution: derived" line — is
/// assertable without capturing stdout.
///
/// The note sits between the two because it explains why the force on the
/// line below it is load-bearing rather than decorative.
fn switch_report(
    dest: &SwitchDest,
    b: &ProjectBinding,
    codebase_note: Option<&str>,
) -> Vec<String> {
    let mut lines = match dest {
        SwitchDest::Session(id) => {
            vec![format!("bound session {id} to project {}", b.project_name)]
        }
        SwitchDest::UserDefault => vec![format!(
            "set user-level default project {}; applies to new sessions without their own binding (the current session, if any, is unchanged — omit --user to bind this session)",
            b.project_name
        )],
    };
    lines.extend(codebase_note.map(str::to_string));
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
    let explicit = project_attribution == "explicit";
    let check = codebase_check_for(&dest, explicit);
    let (binding, codebase_note) = resolve_switch_project(name, &client, check, cwd).await?;
    let binding = apply_force(binding, explicit);

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
    for line in switch_report(&dest, &binding, codebase_note.as_deref()) {
        println!("{line}");
    }
    Ok(())
}

/// The project `project status` treats as effective: exactly the capture-time
/// chain ingest uses ([`capture_project_binding`]: `--project` flag →
/// subagent worktree override → `TRACEVAULT_PROJECT` → session
/// `active_project` → user default).
/// Pure — `status` supplies `user_default` from `user_project_default::load()`
/// and `env_project` from `commands::stream::env_project_binding()`, the same
/// file and the same reader `commands::stream::capture_project` uses, so the
/// two cannot drift (pinned by
/// `project_status_effective_project_matches_capture_project`).
///
/// A binding whose stored id is not a UUID is dropped exactly as ingest drops
/// it ([`capture_project_id`] → `None`), so the effective project is `None`
/// and the second element carries the warning naming the tier it came from.
fn status_effective(
    project_flag: Option<ProjectBinding>,
    env_project: Option<ProjectBinding>,
    session: &SessionState,
    worktree: Option<&str>,
    user_default: Option<ProjectBinding>,
) -> (Option<(ProjectBinding, ProjectSource)>, Option<String>) {
    match capture_project_binding(&CaptureProjectInputs {
        project_flag,
        env_project,
        session,
        worktree_path: worktree,
        user_default,
    }) {
        Some((binding, source)) if capture_project_id(&binding).is_none() => {
            let warning = invalid_capture_id_warning(&binding, source);
            (None, Some(warning))
        }
        other => (other, None),
    }
}

/// Fill in a binding's friendly `project_name` from an already-fetched
/// projects list, when the binding has none.
///
/// The UUID form of `TRACEVAULT_PROJECT` arrives nameless: it is parsed
/// locally by `commands::stream::env_project_binding` and never looked up, so
/// without this `status` prints a bare UUID for a tier it just named. Gating
/// on the EMPTY NAME rather than on a particular [`ProjectSource`] keeps this
/// to the condition actually being repaired; a binding that already has a
/// name is untouched.
///
/// Cosmetic only, and it never changes WHICH project is effective — the id is
/// not touched — so it cannot make `status` disagree with ingest.
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

/// Warning for a `TRACEVAULT_PROJECT` that holds a NAME rather than a UUID.
///
/// Same rule, and the same shape of warning, as [`config_default_warning`]:
/// resolving a name needs a `list_projects` round trip, the per-event capture
/// path never makes one, so the variable decides nothing and attribution
/// falls through to the next tier. `status` therefore does NOT resolve it
/// either — reporting a name-resolved project as effective would be exactly
/// the status/ingest disagreement this command exists to remove.
fn env_name_form_warning(raw: &str) -> String {
    format!(
        "warning: TRACEVAULT_PROJECT '{raw}' is a NAME; only the UUID form is used for attribution at capture time, so this value is ignored — export the project's id instead, or bind it with `tracevault project switch <name>`"
    )
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
        None => "no project bound locally; if a repo is bound, events are sent repo-scoped and the server deduces the project from the repo".to_string(),
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
        "warning: .tracevault/config.toml default_project '{name}' is not used for attribution at capture time; bind explicitly with `tracevault project switch <name>`"
    )
}

/// Warning for `--project <name>` when no credentials resolved: the name
/// cannot be looked up, so the override is ignored.
fn offline_project_flag_warning(name: &str) -> String {
    format!(
        "warning: --project '{name}' cannot be resolved without server credentials; ignoring the override"
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
                format!("{PREFIX} {label} (events will land here if a repo is bound)")
            }
        }
        Ok(ResolveProjectOutcome::Ambiguous) => {
            if effective_is_bound {
                format!(
                    "{PREFIX} ambiguous (multiple projects) — not used, the local binding above wins"
                )
            } else {
                format!(
                    "{PREFIX} ambiguous (multiple projects) — if a repo is bound, events will be refused until you run `tracevault project switch <name>`"
                )
            }
        }
        Ok(ResolveProjectOutcome::None) => format!("{PREFIX} none"),
        Err(msg) => format!("{PREFIX} unavailable ({msg})"),
    }
}

/// Everything `project status` prints: `warnings` go to stderr, `lines`
/// (the effective-project line, then the deduction line when shown) to
/// stdout. Built by [`status_report`] so tests assert the exact output.
#[derive(Debug, Default)]
struct StatusReport {
    warnings: Vec<String>,
    lines: Vec<String>,
}

/// `tracevault project status`: a read-only inspector that always returns
/// `Ok(())`. Prints the [`StatusReport`] built by [`status_report`].
async fn status(
    session_id: Option<&str>,
    project_flag_name: Option<&str>,
    project_root: &Path,
    cwd: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let report = status_report(session_id, project_flag_name, project_root, cwd).await;
    for warning in &report.warnings {
        eprintln!("{warning}");
    }
    for line in &report.lines {
        println!("{line}");
    }
    Ok(())
}

/// The content of `project status`. The effective project is the
/// capture-time chain (local, no network); the server's deduction from the
/// git remote is shown on its own line, best-effort, as what would apply if
/// nothing were bound locally. Never fails: every problem becomes a warning.
async fn status_report(
    session_id: Option<&str>,
    project_flag_name: Option<&str>,
    project_root: &Path,
    cwd: &Path,
) -> StatusReport {
    let mut report = StatusReport::default();
    // Session state is best-effort: if a session id resolves, load it; else
    // warn and fall back to an empty SessionState.
    let session = match crate::commands::repo::resolve_session_id(session_id) {
        Ok(id) => session_state::load(&id),
        Err(_) => {
            report.warnings.push(
                "warning: no session id (pass --session-id or set TRACEVAULT_SESSION_ID); \
                 showing binding without session context"
                    .to_string(),
            );
            SessionState::default()
        }
    };
    let worktree = crate::paths::worktree_toplevel(cwd);
    let config_default_name =
        crate::config::TracevaultConfig::load(project_root).and_then(|c| c.default_project);
    let user_default = crate::user_project_default::load();
    let git_url = git_remote_url(cwd);

    // `TRACEVAULT_PROJECT` enters the chain through the capture path's OWN
    // reader, which is UUID-only: `status` must not resolve a NAME into the
    // chain, or it would report a project ingest never attributes to. The
    // name form gets a warning instead.
    let env_project = crate::commands::stream::env_project_binding();
    let env_raw = std::env::var("TRACEVAULT_PROJECT")
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty());
    if env_project.is_none() {
        if let Some(raw) = env_raw.as_deref() {
            report.warnings.push(env_name_form_warning(raw));
        }
    }

    // The server is needed only to resolve a `--project` NAME, to put a
    // friendly name on the env binding's bare UUID, and to show the
    // deduction line; the effective project itself is computed locally.
    let client = match resolve_client(project_root) {
        Ok(client) => Some(client),
        Err(e) => {
            report.warnings.push(format!(
                "warning: could not resolve credentials ({e}); server deduction not shown"
            ));
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
                    forced_until: None,
                });
            if flag.is_none() {
                report.warnings.push(format!(
                    "warning: --project '{name}' could not be resolved; ignoring the override"
                ));
            }
            flag
        }
        (Some(name), None) => {
            report.warnings.push(offline_project_flag_warning(name));
            None
        }
        (None, _) => None,
    };

    // The UUID form of `TRACEVAULT_PROJECT` resolves to a NAMELESS binding
    // (no lookup on the capture path), so fetch the list once here purely to
    // print a friendly name for it. Cosmetic: the id, and therefore which
    // project is effective, is unaffected either way.
    if items.is_none() && env_project.is_some() {
        if let Some(client) = client.as_ref() {
            items = client.list_projects().await.ok();
        }
    }

    let (effective, invalid_id_warning) = status_effective(
        project_flag,
        env_project,
        &session,
        Some(&worktree),
        user_default,
    );
    report.warnings.extend(invalid_id_warning);
    let effective = enrich_project_name(effective, items.as_deref());

    if let Some(name) = config_default_name.as_deref() {
        report.warnings.push(config_default_warning(name));
    }

    report
        .lines
        .push(format_status(effective.as_ref().map(|(b, s)| (b, *s))));
    // The attribution mode of the binding that actually won, read off THAT
    // binding (plus `TRACEVAULT_PROJECT_ATTRIBUTION`) — the same value
    // `commands::stream::send_stream_event` puts in the header.
    if let Some((b, _)) = effective.as_ref() {
        report.lines.extend(attribution_report(b));
    }

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
        report
            .lines
            .push(format_deduction(effective.is_some(), outcome, name));
    }
    report
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

    /// `format_force_line` reports the STATE of the stored force and never a
    /// verdict, so no output of it may contain the words the mode line owns.
    /// A live force says when it runs out; a lapsed one says when it did.
    #[test]
    fn format_force_line_distinguishes_a_live_force_from_a_lapsed_one() {
        let future = (chrono::Utc::now() + chrono::Duration::hours(4)).to_rfc3339();
        let live = format_force_line(&future);
        assert!(live.contains("active until"), "got: {live}");
        assert!(live.contains(&future), "got: {live}");

        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let lapsed = format_force_line(&past);
        assert!(lapsed.contains("lapsed"), "got: {lapsed}");
        assert!(lapsed.contains(&past), "got: {lapsed}");

        // Neither may render a mode: that is `attribution_report`'s first
        // line, and a second answer here is what produced both contradiction
        // bugs (see `attribution_report`'s doc comment).
        for line in [&live, &lapsed] {
            assert!(!line.contains("derived"), "got: {line}");
            assert!(!line.contains("explicit"), "got: {line}");
            assert!(!line.contains("membership"), "got: {line}");
        }
    }

    /// Discriminates the SIGN of the lapse comparison in `format_force_line`:
    /// a `forced_until` already in the past must report as lapsed, not as
    /// still active.
    #[test]
    fn status_names_a_lapsed_force_as_lapsed_not_active() {
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let line = format_force_line(&past);
        assert!(line.contains("lapsed"), "got: {line}");
        assert!(!line.contains("active until"), "got: {line}");
    }

    /// A `forced_until` that doesn't even parse as RFC3339 (a hand-edited or
    /// corrupted `user_project.toml`/session-state file) must fail SAFE —
    /// reported as not in force, never as active, and never a panic. Pins
    /// the fail-safe direction against a future refactor to `.expect(...)`.
    ///
    /// The second half — that the printed state agrees with
    /// `commands::stream::attribution_mode`, the one that fills the header —
    /// used to reconcile two independently written predicates. Both now go
    /// through `session_state::force_status`, so it pins that neither reader
    /// has quietly grown a second opinion around the shared one.
    #[test]
    fn status_names_unparseable_forced_until_as_not_in_force() {
        let line = format_force_line("not-a-timestamp");
        assert!(line.contains("not in force"), "got: {line}");
        assert!(!line.contains("active until"), "got: {line}");

        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.remove("TRACEVAULT_PROJECT_ATTRIBUTION");
        let b = ProjectBinding {
            project_id: "3f2504e0-4f89-11d3-9a0c-0305e82c3301".into(),
            project_name: "payments".into(),
            updated_at: "".into(),
            forced_until: Some("not-a-timestamp".into()),
        };
        assert_eq!(
            crate::commands::stream::attribution_mode(Some(&b)),
            "derived",
            "the reported state and the header must agree on an unreadable force"
        );
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
        assert!(lines[1].contains("active until"), "got: {}", lines[1]);
        assert!(lines[1].contains(&until), "got: {}", lines[1]);
    }

    /// A LAPSED persisted force still gets its detail line — the binding
    /// really does carry a force, it has simply run out, and saying when is
    /// the useful part. With no env override the mode line correctly reads
    /// `derived`; the detail line states the lapse as a fact, and the two
    /// agree without the detail line having to render a verdict of its own.
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
        assert!(
            lines[1].starts_with("persisted force: lapsed"),
            "got: {}",
            lines[1]
        );
    }

    /// Copilot on PR #54: the `is_some()` guard is the wrong test, because a
    /// LAPSED force is still `is_some()`. With
    /// `TRACEVAULT_PROJECT_ATTRIBUTION=explicit` and a binding whose
    /// persisted force has expired, the report used to read
    ///
    /// ```text
    /// attribution mode: explicit (this caller owns attribution; ...)
    /// attribution: derived (a force lapsed ...; membership is checked again)
    /// ```
    ///
    /// — the exact contradiction `attribution_report` exists to remove, just
    /// reached by a different route than finding 3's. The fix is not a
    /// narrower guard (the lapse time is still worth printing) but wording:
    /// the second line states a FACT about the persisted binding, so it is
    /// detail under the verdict rather than a competing verdict. The
    /// unparseable case is the same shape and is covered too.
    #[test]
    fn a_lapsed_persisted_force_never_contradicts_an_env_force() {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("TRACEVAULT_PROJECT_ATTRIBUTION", "explicit");

        let lapsed = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        for forced_until in [lapsed.as_str(), "not-a-timestamp"] {
            let b = ProjectBinding {
                project_id: "3f2504e0-4f89-11d3-9a0c-0305e82c3301".into(),
                project_name: "payments".into(),
                updated_at: "".into(),
                forced_until: Some(forced_until.to_string()),
            };
            let lines = attribution_report(&b);
            assert_eq!(lines.len(), 2, "{forced_until}: got {lines:?}");
            assert!(
                lines[0].starts_with("attribution mode: explicit"),
                "{forced_until}: the env force decides the mode: {lines:?}"
            );
            // The detail line may say the PERSISTED force has lapsed — that
            // is a true fact about the stored binding — but it must not
            // answer the question the mode line already answered.
            assert!(
                !lines[1].contains("derived"),
                "{forced_until}: the detail line must not render a competing \
                 verdict while the env force makes this send explicit: {lines:?}"
            );
            assert!(
                !lines[1].contains("membership is checked"),
                "{forced_until}: membership is NOT being checked here: {lines:?}"
            );
        }
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
            let lines = switch_report(&dest, &b, None);
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
        let lines = switch_report(&SwitchDest::Session("sess-1".to_string()), &b, None);
        assert_eq!(lines.len(), 3, "got: {lines:?}");
        assert!(
            lines[1].starts_with("attribution mode: explicit"),
            "got: {}",
            lines[1]
        );
        assert!(
            lines[2].starts_with("persisted force: active until"),
            "got: {}",
            lines[2]
        );
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
    fn format_status_unbound() {
        assert_eq!(
            format_status(None),
            "no project bound locally; if a repo is bound, events are sent repo-scoped and the server deduces the project from the repo"
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
            forced_until: None,
        };
        assert_eq!(
            format_status(Some((&b, ProjectSource::UserDefault))),
            "project: some-id via user default (project switch --user)"
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
    fn invalid_capture_id_warning_names_tier_and_rewrite_command() {
        let b = ProjectBinding {
            project_id: "not-a-uuid".into(),
            project_name: "payments".into(),
            updated_at: "".into(),
            forced_until: None,
        };
        assert_eq!(
            invalid_capture_id_warning(&b, ProjectSource::UserDefault),
            "warning: project payments — user default (project switch --user), but the saved project_id is not a valid id and is dropped at capture time; run `tracevault project switch <name>` to rewrite it"
        );
    }

    /// `status`'s own drop rule: a non-UUID binding is not effective, and the
    /// warning returned for it names the tier it came from.
    #[test]
    fn status_effective_drops_a_non_uuid_binding_with_a_warning_naming_the_tier() {
        let session = SessionState {
            active_project: Some(ProjectBinding {
                project_id: "not-a-uuid".into(),
                project_name: "payments".into(),
                updated_at: "".into(),
                forced_until: None,
            }),
            ..Default::default()
        };
        let (effective, warning) = status_effective(None, None, &session, None, None);
        assert!(effective.is_none(), "{effective:?}");
        let warning = warning.expect("a dropped binding must be explained");
        assert!(
            warning.contains("session (project switch)"),
            "must name the tier: {warning}"
        );
        assert!(warning.contains("payments"), "{warning}");

        // A valid binding is effective and carries no warning.
        let pid = uuid::Uuid::from_u128(5);
        let valid = SessionState {
            active_project: Some(ProjectBinding {
                project_id: pid.to_string(),
                project_name: "web".into(),
                updated_at: "".into(),
                forced_until: None,
            }),
            ..Default::default()
        };
        let (effective, warning) = status_effective(None, None, &valid, None, None);
        assert_eq!(
            effective.map(|(b, s)| (b.project_id, s)),
            Some((pid.to_string(), ProjectSource::SessionActive))
        );
        assert_eq!(warning, None);
    }

    /// `TRACEVAULT_PROJECT` is the `Env` rung of the SAME chain ingest runs,
    /// so `status_effective` must place it above session-active and the user
    /// default, and below `--project`. Pure: the binding is passed in, as
    /// `status_report` passes what `stream::env_project_binding` returned.
    #[test]
    fn status_effective_places_env_at_rung_three() {
        let u = uuid::Uuid::from_u128;
        let bind = |id: uuid::Uuid, name: &str| ProjectBinding {
            project_id: id.to_string(),
            project_name: name.into(),
            updated_at: "".into(),
            forced_until: None,
        };
        let session = SessionState {
            active_project: Some(bind(u(2), "session")),
            ..Default::default()
        };

        // env beats session active and the user default
        let (effective, _) = status_effective(
            None,
            Some(bind(u(9), "env")),
            &session,
            None,
            Some(bind(u(3), "user")),
        );
        assert_eq!(
            effective.map(|(b, s)| (b.project_id, s)),
            Some((u(9).to_string(), ProjectSource::Env))
        );

        // --project still beats env
        let (effective, _) = status_effective(
            Some(bind(u(1), "flag")),
            Some(bind(u(9), "env")),
            &session,
            None,
            Some(bind(u(3), "user")),
        );
        assert_eq!(
            effective.map(|(b, s)| (b.project_id, s)),
            Some((u(1).to_string(), ProjectSource::ProjectFlag))
        );
    }

    #[test]
    fn offline_project_flag_warning_says_the_override_is_ignored() {
        assert_eq!(
            offline_project_flag_warning("web"),
            "warning: --project 'web' cannot be resolved without server credentials; ignoring the override"
        );
    }

    /// The NAME form of `TRACEVAULT_PROJECT` is not resolved into the chain
    /// (that would need a per-event `list_projects` the hook never makes), so
    /// `status` warns instead of reporting a tier ingest ignores.
    #[test]
    fn env_name_form_warning_says_only_the_uuid_form_is_used() {
        let warning = env_name_form_warning("payments");
        assert!(warning.contains("payments"), "{warning}");
        assert!(warning.contains("UUID form"), "{warning}");
        assert!(
            warning.contains("not used") || warning.contains("ignored"),
            "{warning}"
        );
    }

    #[test]
    fn config_default_warning_says_it_is_not_used_at_capture_time() {
        assert_eq!(
            config_default_warning("web"),
            "warning: .tracevault/config.toml default_project 'web' is not used for attribution at capture time; bind explicitly with `tracevault project switch <name>`"
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
            format!("server deduction for this repo: {DEDUCED} (events will land here if a repo is bound)")
        );
    }

    #[test]
    fn format_deduction_resolved_uses_name_when_known() {
        let items = items();
        let name = deduced_project_name(DEDUCED, Some(&items));
        assert_eq!(name, Some("web"));
        assert_eq!(
            format_deduction(false, Ok(ResolveProjectOutcome::Resolved(DEDUCED)), name),
            "server deduction for this repo: web (events will land here if a repo is bound)"
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
            "server deduction for this repo: ambiguous (multiple projects) — if a repo is bound, events will be refused until you run `tracevault project switch <name>`"
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
        // Held under the env lock: other tests (`flush`'s `run_flush` test,
        // `status_in_the_orchestrator_configuration_names_the_user_default`)
        // also set XDG_STATE_HOME. Restored in a guard so a panic mid-test
        // still cleans up the process env.
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let state_tmp = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_STATE_HOME", state_tmp.path());

        let list = r#"[{"id":"11111111-1111-4111-8111-111111111111","name":"payments"},{"id":"22222222-2222-4222-8222-222222222222","name":"web"}]"#;
        let base = spawn_once(http_200(list));
        let client = ApiClient::new(&base, Some("tok"));

        let (binding, note) =
            resolve_switch_project("web", &client, CodebaseCheck::Enforce, tmp.path())
                .await
                .expect("expected Ok binding");
        assert_eq!(binding.project_id, "22222222-2222-4222-8222-222222222222");
        assert_eq!(binding.project_name, "web");
        assert_eq!(note, None, "no remote to check against, so nothing to note");

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

        let err = resolve_switch_project(
            "web",
            &client,
            CodebaseCheck::Skip,
            Path::new("/nonexistent"),
        )
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

        let err = resolve_switch_project("payments", &client, CodebaseCheck::Enforce, tmp.path())
            .await
            .expect_err("expected Err: project doesn't contain the current codebase");
        assert!(
            err.to_string()
                .contains("does not contain the current codebase"),
            "got: {err}"
        );
    }

    /// Copilot on PR #54: `--project-attribution explicit` was refused for
    /// exactly the projects it exists to bind. `check_codebase` was derived
    /// from the destination alone, so inside any instrumented session (the
    /// default whenever `TRACEVAULT_SESSION_ID` is set) `resolve_switch_project`
    /// errored on a project that does not contain this checkout — a repo
    /// deliberately shared by two projects, say. That contradicts the flag's
    /// own help text and the design, which puts the requirement on the
    /// SERVER at ingest.
    ///
    /// Same mock traffic as
    /// `project_switch_errors_when_project_does_not_contain_codebase` — the
    /// check genuinely runs and genuinely fails — but under
    /// `CodebaseCheck::Report` it must bind anyway AND say so, so a user who
    /// got here by typo still learns the fact.
    #[tokio::test]
    async fn explicit_attribution_binds_a_non_member_project_and_says_so() {
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

        let (binding, note) =
            resolve_switch_project("payments", &client, CodebaseCheck::Report, tmp.path())
                .await
                .expect("a forced switch must not be blocked by the containment check");
        assert_eq!(binding.project_id, "11111111-1111-4111-8111-111111111111");

        let note = note.expect("the user must still be told the project is not a member");
        assert!(
            note.contains("does not contain the current codebase"),
            "the fact must be stated: {note}"
        );
        assert!(
            note.contains("--project-attribution explicit"),
            "must say what made it deliberate: {note}"
        );
        assert!(
            note.contains("Operator") && note.contains("ingest"),
            "must say where the requirement is actually enforced: {note}"
        );

        // It reads as a note, not a refusal: the switch succeeded.
        assert!(
            !note.starts_with("error"),
            "this is informational, not a failure: {note}"
        );
    }

    /// The other direction, and the reason `Report` is opt-in: without the
    /// flag an ordinary session switch still `Enforce`s, so a typo'd project
    /// name that happens to exist is caught exactly as before (the erroring
    /// half is `project_switch_errors_when_project_does_not_contain_codebase`).
    /// Calls the same [`codebase_check_for`] `switch` calls, so the mapping
    /// cannot drift away from this test.
    #[test]
    fn only_an_explicit_session_switch_downgrades_the_containment_check() {
        let session = SwitchDest::Session("s".into());
        assert_eq!(
            codebase_check_for(&session, true),
            CodebaseCheck::Report,
            "--project-attribution explicit must not be blocked"
        );
        assert_eq!(
            codebase_check_for(&session, false),
            CodebaseCheck::Enforce,
            "an ordinary session switch keeps erroring"
        );
        // The user-level default is not tied to a checkout either way.
        assert_eq!(
            codebase_check_for(&SwitchDest::UserDefault, true),
            CodebaseCheck::Skip
        );
        assert_eq!(
            codebase_check_for(&SwitchDest::UserDefault, false),
            CodebaseCheck::Skip
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

    /// A nameless binding at a given tier. The UUID form of
    /// `TRACEVAULT_PROJECT` is the one such tier that can be EFFECTIVE (the
    /// capture path parses the id locally and never looks the name up);
    /// `Deduced` is nameless too, but only ever reaches the separate
    /// deduction line, which has [`deduced_project_name`] of its own.
    fn nameless(id: uuid::Uuid, source: ProjectSource) -> (ProjectBinding, ProjectSource) {
        (
            ProjectBinding {
                project_id: id.to_string(),
                project_name: String::new(),
                updated_at: "".into(),
                forced_until: None,
            },
            source,
        )
    }

    /// Small finding 8: the UUID form of `TRACEVAULT_PROJECT` never goes
    /// through a name lookup, so its binding arrives nameless and the
    /// operator would see a bare UUID on the effective line.
    #[test]
    fn enrich_project_name_fills_in_name_when_id_found_in_list() {
        let effective = Some(nameless(uuid::Uuid::from_u128(2), ProjectSource::Env));
        let (b, source) = enrich_project_name(effective, Some(&items())).unwrap();
        assert_eq!(b.project_name, "web");
        assert_eq!(source, ProjectSource::Env);
    }

    #[test]
    fn enrich_project_name_leaves_empty_when_no_list_available() {
        let effective = Some(nameless(uuid::Uuid::from_u128(2), ProjectSource::Env));
        let (b, _source) = enrich_project_name(effective, None).unwrap();
        assert_eq!(b.project_name, "");
    }

    #[test]
    fn enrich_project_name_leaves_empty_when_id_not_in_list() {
        let effective = Some(nameless(uuid::Uuid::from_u128(999), ProjectSource::Env));
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

    #[test]
    fn enrich_project_name_passes_through_none() {
        assert!(enrich_project_name(None, Some(&items())).is_none());
    }

    /// VIS-316 pin: the project `project status` reports as effective is the
    /// project ingest attributes events to. For each configuration, the id
    /// `status` would treat as effective (`status_effective`, the function
    /// `status` itself calls, including its non-UUID drop rule) must equal `commands::stream::capture_project`. Both sides read
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
            forced_until: None,
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

        // (label, session, user default written to user_project.toml,
        //  TRACEVAULT_PROJECT, expected id)
        type Case = (
            &'static str,
            SessionState,
            Option<String>,
            Option<String>,
            Option<uuid::Uuid>,
        );
        let cases: Vec<Case> = vec![
            (
                "(a) subagent override + session active + user default",
                subagent_session,
                Some(u(3).to_string()),
                None,
                Some(u(1)),
            ),
            (
                "(b) session active + user default",
                active_session.clone(),
                Some(u(3).to_string()),
                None,
                Some(u(2)),
            ),
            (
                "(c) user default only",
                SessionState::default(),
                Some(u(3).to_string()),
                None,
                Some(u(3)),
            ),
            ("(d) nothing", SessionState::default(), None, None, None),
            (
                "(e) user default with a non-UUID project_id",
                SessionState::default(),
                Some("not-a-uuid".into()),
                None,
                None,
            ),
            (
                "(f) non-UUID session active shadows a valid user default",
                bad_active_session,
                Some(u(3).to_string()),
                None,
                None,
            ),
            // The `Env` rung is part of the SAME shared chain, so it has to
            // be covered here too or `status` and ingest could drift on it.
            (
                "(g) TRACEVAULT_PROJECT (UUID) outranks session active and the user default",
                active_session.clone(),
                Some(u(3).to_string()),
                Some(u(4).to_string()),
                Some(u(4)),
            ),
            // A NAME in TRACEVAULT_PROJECT is not a tier on either side:
            // both must fall through to the session's active binding.
            (
                "(h) TRACEVAULT_PROJECT (NAME) is ignored by both",
                active_session,
                Some(u(3).to_string()),
                Some("payments".into()),
                Some(u(2)),
            ),
        ];

        for (label, session, user_default, env_project, expected) in cases {
            match &user_default {
                Some(id) => crate::user_project_default::save(&bind(id.clone())).unwrap(),
                None => crate::user_project_default::clear().unwrap(),
            }
            match &env_project {
                Some(v) => _guard.set("TRACEVAULT_PROJECT", v),
                None => _guard.remove("TRACEVAULT_PROJECT"),
            }
            let status_pid = status_effective(
                None,
                crate::commands::stream::env_project_binding(),
                &session,
                Some(worktree),
                crate::user_project_default::load(),
            )
            .0
            .and_then(|(b, _)| capture_project_id(&b));
            let ingest_pid = crate::commands::stream::capture_project(&session, Some(worktree));
            assert_eq!(
                status_pid, ingest_pid,
                "{label}: status and ingest disagree"
            );
            assert_eq!(status_pid, expected, "{label}");
        }
        crate::user_project_default::clear().unwrap();
    }

    /// F1: `status` returns `Ok` when the `/projects/resolve` deduction
    /// answers 409 (ambiguous) — the deduction is informational, never fatal.
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

        // One 409 for `status`, one for `status_report`.
        let conflict = "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: 20\r\nConnection: close\r\n\r\n{\"error\":\"multiple\"}";
        let base = spawn_n(vec![conflict.to_string(), conflict.to_string()]);

        // Env mutation under `_env_lock`, restored by the guard even if
        // `status` panics. The config dir (and so any user default) is
        // isolated to the tempdir, and no ambient session id leaks in.
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("TRACEVAULT_SERVER_URL", &base);
        _guard.set("TRACEVAULT_API_KEY", "tok");
        _guard.set("XDG_CONFIG_HOME", tmp.path());
        _guard.set("HOME", tmp.path());
        _guard.remove("TRACEVAULT_SESSION_ID");
        // `status_effective` takes `TRACEVAULT_PROJECT` at a rung ABOVE the
        // "nothing is bound" state this test asserts, and an env-sourced
        // binding also triggers an extra `list_projects` call this mock does
        // not stage — an ambient export would make the test fail, or pass,
        // for the wrong reason.
        _guard.remove("TRACEVAULT_PROJECT");
        _guard.remove("TRACEVAULT_PROJECT_ATTRIBUTION");

        let result = status(None, None, tmp.path(), tmp.path()).await;
        assert!(
            result.is_ok(),
            "status must degrade gracefully on an ambiguous deduction, not propagate the error: {result:?}"
        );

        let report = status_report(None, None, tmp.path(), tmp.path()).await;
        assert_eq!(
            report.lines,
            vec![
                format_status(None),
                format_deduction(false, Ok(ResolveProjectOutcome::Ambiguous), None),
            ],
            "the ambiguous deduction is reported on its own line"
        );
    }

    /// VIS-316 orchestrator configuration: the container sets a machine-wide
    /// user default B before the session exists, the session itself binds
    /// nothing, and the server would deduce A from the git remote. `status`
    /// must name B (what ingest uses) as effective, and show A only as an
    /// unused deduction.
    #[tokio::test]
    async fn status_in_the_orchestrator_configuration_names_the_user_default() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        crate::test_helpers::init_git_repo(&repo);
        let ok = std::process::Command::new("git")
            .args([
                "-C",
                &repo.to_string_lossy(),
                "remote",
                "add",
                "origin",
                "git@github.com:org/repo.git",
            ])
            .status()
            .expect("git remote add failed")
            .success();
        assert!(ok, "git remote add must succeed");

        let deduced_a = uuid::Uuid::from_u128(0xA);
        let base = spawn_once(http_200(&format!(r#"{{"project_id":"{deduced_a}"}}"#)));

        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("TRACEVAULT_SERVER_URL", &base);
        _guard.set("TRACEVAULT_API_KEY", "tok");
        _guard.set("XDG_CONFIG_HOME", tmp.path());
        _guard.set("HOME", tmp.path());
        _guard.set("XDG_STATE_HOME", tmp.path().join("state"));
        _guard.remove("TRACEVAULT_SESSION_ID");
        // Both would change which project is effective / which mode is
        // reported, and the env binding would also trigger a `list_projects`
        // call this one-shot mock does not stage.
        _guard.remove("TRACEVAULT_PROJECT");
        _guard.remove("TRACEVAULT_PROJECT_ATTRIBUTION");

        let default_b = ProjectBinding {
            project_id: uuid::Uuid::from_u128(0xB).to_string(),
            project_name: "project-b".into(),
            updated_at: "".into(),
            forced_until: None,
        };
        crate::user_project_default::save(&default_b).unwrap();

        let report = status_report(None, None, &repo, &repo).await;

        assert_eq!(
            report.lines,
            vec![
                "project: project-b via user default (project switch --user)".to_string(),
                "attribution mode: derived (TraceVault checks repo/project membership)"
                    .to_string(),
                format!(
                    "server deduction for this repo: {deduced_a} (not used: the local binding above wins)"
                ),
            ]
        );
    }

    /// A3, UUID form, through the whole report: `TRACEVAULT_PROJECT` set to
    /// a UUID resolves at the `Env` rung, and the one `list_projects` call
    /// the env binding triggers is used to put its friendly name on the line
    /// (small finding 8 — otherwise a bare UUID is printed). `spawn_once`'s
    /// listener only ever answers one request, so a regression that fell
    /// through to the `/projects/resolve` deduction would fail on the
    /// missing listener as well as on the line itself.
    #[tokio::test]
    async fn status_reports_env_uuid_as_the_effective_tier_with_its_name() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();

        let uuid = "11111111-1111-4111-8111-111111111111";
        let list = format!(r#"[{{"id":"{uuid}","name":"payments"}}]"#);
        let base = spawn_once(http_200(&list));

        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", tmp.path());
        _guard.set("HOME", tmp.path());
        _guard.set("TRACEVAULT_SERVER_URL", &base);
        _guard.set("TRACEVAULT_API_KEY", "tok");
        _guard.set("TRACEVAULT_PROJECT", uuid);
        _guard.remove("TRACEVAULT_PROJECT_ATTRIBUTION");
        _guard.remove("TRACEVAULT_SESSION_ID");

        let report = status_report(None, None, tmp.path(), tmp.path()).await;
        assert_eq!(
            report.lines[0], "project: payments via TRACEVAULT_PROJECT (environment)",
            "got: {:?}",
            report.lines
        );
        assert!(
            report
                .warnings
                .iter()
                .all(|w| !w.contains("TRACEVAULT_PROJECT")),
            "the UUID form is honoured at capture time and needs no caveat: {:?}",
            report.warnings
        );
    }

    /// A3, name form: unlike our earlier design, `status` does NOT resolve a
    /// NAME in `TRACEVAULT_PROJECT` into the chain. VIS-316's rule is that
    /// `project status` reports what ingest does, and the capture path is
    /// UUID-only (a name would need a per-event `list_projects`), so a
    /// resolved name would be a project no event is attributed to. The value
    /// is reported as an ignored tier instead, and the session's own binding
    /// stays effective.
    #[tokio::test]
    async fn status_warns_that_an_env_name_is_not_used_and_falls_through() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();

        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", tmp.path());
        _guard.set("HOME", tmp.path());
        _guard.set("XDG_STATE_HOME", tmp.path().join("state"));
        _guard.remove("TRACEVAULT_SERVER_URL");
        _guard.remove("TRACEVAULT_API_KEY");
        _guard.remove("TRACEVAULT_PROJECT_ATTRIBUTION");
        _guard.set("TRACEVAULT_PROJECT", "payments");

        let session_id = "vis305-env-name-form";
        let pid = uuid::Uuid::from_u128(0x5E);
        crate::session_state::save(
            session_id,
            &SessionState {
                active_project: Some(ProjectBinding {
                    project_id: pid.to_string(),
                    project_name: "session-project".into(),
                    updated_at: "".into(),
                    forced_until: None,
                }),
                ..Default::default()
            },
        )
        .unwrap();

        let report = status_report(Some(session_id), None, tmp.path(), tmp.path()).await;
        assert_eq!(
            report.lines[0], "project: session-project via session (project switch)",
            "a NAME must not become the effective tier: {:?}",
            report.lines
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("TRACEVAULT_PROJECT") && w.contains("UUID form")),
            "the ignored NAME must be explained: {:?}",
            report.warnings
        );
    }

    /// A3, item 3: with credentials unresolved (no client at all — the `Err`
    /// arm of `resolve_client`), `status` must still honour the UUID form of
    /// `TRACEVAULT_PROJECT`, which needs no server call. The friendly name
    /// is simply unavailable, so the id is printed.
    #[tokio::test]
    async fn status_reports_env_uuid_even_without_a_client() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();

        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", tmp.path());
        _guard.set("HOME", tmp.path());
        _guard.remove("TRACEVAULT_SERVER_URL");
        _guard.remove("TRACEVAULT_API_KEY");
        _guard.remove("TRACEVAULT_PROJECT_ATTRIBUTION");
        let uuid = "33333333-3333-4333-8333-333333333333";
        _guard.set("TRACEVAULT_PROJECT", uuid);

        let report = status_report(None, None, tmp.path(), tmp.path()).await;
        assert_eq!(
            report.lines[0],
            format!("project: {uuid} via TRACEVAULT_PROJECT (environment)"),
            "got: {:?}",
            report.lines
        );
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
        _guard.set("HOME", tmp.path());
        _guard.remove("TRACEVAULT_SERVER_URL");
        _guard.remove("TRACEVAULT_API_KEY");
        let uuid = "44444444-4444-4444-8444-444444444444";
        _guard.set("TRACEVAULT_PROJECT", uuid);
        _guard.set("TRACEVAULT_PROJECT_ATTRIBUTION", "explicit");

        let report = status_report(None, None, tmp.path(), tmp.path()).await;
        assert_eq!(
            report.lines,
            vec![
                format!("project: {uuid} via TRACEVAULT_PROJECT (environment)"),
                "attribution mode: explicit (this caller owns attribution; membership is not checked)".to_string(),
            ],
            "the EFFECTIVE mode must reflect TRACEVAULT_PROJECT_ATTRIBUTION even when \
             the winning binding has no force of its own, and the lapse line must NOT \
             appear (there is no persisted force to lapse)"
        );
    }
}
