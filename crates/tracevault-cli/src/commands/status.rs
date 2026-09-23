//! `tracevault status` — surface every piece of state a user might need to
//! debug "why doesn't it work". The command runs read-only checks across
//! credentials, the project tree, and the server, and prints a grouped
//! report with ✓ / ✗ / ! markers. Exits non-zero if anything actionable is
//! broken.

use crate::api_client::{ApiClient, GetMeError};
use crate::config::TracevaultConfig;
use crate::credentials::{Credential, Credentials};
use crate::resolution::{
    effective_project, git_remote_url, git_repo_name, resolve_effective_project,
    ProjectResolveInputs, ProjectSource,
};
use std::fs;
use std::path::{Path, PathBuf};

const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_RED: &str = "\x1b[31m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_RESET: &str = "\x1b[0m";

/// Severity classification of a single check. Anything at `Error` level
/// bumps the final exit code to 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Ok,
    Warn,
    Error,
    /// Check skipped because a prerequisite failed (e.g. can't validate
    /// token if no token was found). Does not affect exit code.
    Skip,
}

#[derive(Debug)]
struct Check {
    label: String,
    level: Level,
    detail: String,
}

impl Check {
    fn ok(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            level: Level::Ok,
            detail: detail.into(),
        }
    }
    fn warn(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            level: Level::Warn,
            detail: detail.into(),
        }
    }
    fn err(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            level: Level::Error,
            detail: detail.into(),
        }
    }
    fn skip(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            level: Level::Skip,
            detail: detail.into(),
        }
    }
}

fn marker(l: Level) -> &'static str {
    match l {
        Level::Ok => "\x1b[32m✓\x1b[0m",
        Level::Warn => "\x1b[33m!\x1b[0m",
        Level::Error => "\x1b[31m✗\x1b[0m",
        Level::Skip => "\x1b[2m·\x1b[0m",
    }
}

fn print_section(title: &str, checks: &[Check]) {
    println!("{ANSI_DIM}──{ANSI_RESET} {title}");
    for c in checks {
        if c.detail.is_empty() {
            println!("  {} {}", marker(c.level), c.label);
        } else {
            println!(
                "  {} {:<32} {ANSI_DIM}{}{ANSI_RESET}",
                marker(c.level),
                c.label,
                c.detail
            );
        }
    }
    println!();
}

// --- Authentication ---

struct AuthContext {
    server_url: Option<String>,
    /// The resolved credential, if any. Cloned per client construction
    /// because a Keycloak session may be refreshed by the checks below.
    credential: Option<Credential>,
    source: &'static str, // "env", "credentials", or "none"
    email_from_creds: Option<String>,
    /// `(file's server_url, TRACEVAULT_SERVER_URL)` when the saved credential
    /// is for a different instance than the env var targets.
    ///
    /// This inspector deliberately reports the FILE's URL (that is the
    /// credential it validates), so without this field `status` would print a
    /// green "Logged in" while every other command refuses to run — the exact
    /// situation someone runs `status` to diagnose.
    url_override_mismatch: Option<(String, String)>,
}

/// `config_server_url`/`config_api_key` are the lowest rung, mirroring
/// `credentials::resolve_credentials`. Without them this inspector reported a
/// repo authenticated purely through `.tracevault/config.toml` as "no
/// credentials found" and exited non-zero, while every command using that same
/// config worked — the inspector called a working setup broken.
fn resolve_auth(config_server_url: Option<&str>, config_api_key: Option<&str>) -> AuthContext {
    // Env var wins. Match the server-side resolution order in
    // resolve_credentials (env > creds), except we treat the env var as
    // authoritative without looking at the credentials file email.
    let env_key = std::env::var("TRACEVAULT_API_KEY").ok();
    let env_url = std::env::var("TRACEVAULT_SERVER_URL").ok();

    if let Some(token) = env_key {
        return AuthContext {
            // `.or(config)` because `resolve_credentials` resolves the URL and
            // the credential independently: an env key with the URL pinned only
            // in the config is a working setup, not "no server URL".
            server_url: env_url.or_else(|| config_server_url.map(str::to_string)),
            credential: Some(Credential::ApiKey(token)),
            source: "env (TRACEVAULT_API_KEY)",
            email_from_creds: None,
            // An explicitly supplied API key may target any URL by design.
            url_override_mismatch: None,
        };
    }

    let creds = Credentials::load();
    if let Some(c) = creds {
        let source = match &c.auth {
            Some(_) => "credentials file (Keycloak session)",
            None => "credentials file (API key)",
        };
        // Any credential read from the FILE is instance-bound — a `tvk_` key as
        // much as a Keycloak session (see `api_client::resolve_credentials`,
        // which refuses this pairing for both).
        let url_override_mismatch = env_url.filter(|env| {
            c.credential().is_some() && !crate::credentials::same_server(&c.server_url, env)
        });
        return AuthContext {
            server_url: Some(c.server_url.clone()),
            credential: c.credential(),
            source,
            // An empty email means a login saved credentials before
            // `/auth/me` could resolve the identity (e.g. the account lacks
            // the `tracing` role); there is nothing to compare against.
            email_from_creds: Some(c.email).filter(|e| !e.is_empty()),
            url_override_mismatch: url_override_mismatch.map(|env| (c.server_url, env)),
        };
    }

    // Lowest rung: a hand-authored `config.toml` may carry `api_key` and
    // `server_url`. Only reached when the env and the credentials file yielded
    // nothing, exactly as in `resolve_credentials`.
    AuthContext {
        server_url: env_url.or_else(|| config_server_url.map(str::to_string)),
        credential: config_api_key.map(|k| Credential::ApiKey(k.to_string())),
        source: if config_api_key.is_some() {
            "project config (.tracevault/config.toml)"
        } else {
            "none"
        },
        email_from_creds: None,
        url_override_mismatch: None,
    }
}

async fn auth_checks(auth: &AuthContext, config_server_url: Option<&str>) -> Vec<Check> {
    let mut out = Vec::new();

    match (auth.credential.as_ref(), auth.server_url.as_ref()) {
        (None, _) => {
            out.push(Check::err(
                "Logged in",
                "no credentials found. Run `tracevault login --server-url <URL>`.",
            ));
            out.push(Check::skip("Token valid", "no token to check"));
            return out;
        }
        (Some(_), None) => {
            out.push(Check::err(
                "Logged in",
                "token found but no server URL (set TRACEVAULT_SERVER_URL)",
            ));
            out.push(Check::skip("Token valid", "no server URL to call"));
            return out;
        }
        (Some(_), Some(url)) => {
            out.push(Check::ok("Logged in", format!("{url} via {}", auth.source)));
        }
    }

    // Reported as an ERROR, not a warning: in this state every command that
    // needs auth refuses to run (see `api_client::resolve_credentials`), so
    // "everything is fine except this note" would be misleading.
    if let Some((file_url, env_url)) = &auth.url_override_mismatch {
        out.push(Check::err(
            "Server URL",
            format!(
                "TRACEVAULT_SERVER_URL is '{env_url}' but the saved login is for '{file_url}'. \
                 Commands will refuse to run rather than send that session's token to another \
                 instance. Unset TRACEVAULT_SERVER_URL, or run `tracevault login --server-url \
                 {env_url}`."
            ),
        ));
    }

    // Reported as a WARNING, not an error: unlike the override above, commands
    // still run — they just use the login's instance while the repo's config
    // says otherwise. Without this the inspector is actively misleading: it
    // reads only the credentials file, so it would print a green "Logged in"
    // against a URL the repo never asked for. Shares
    // `credentials::config_url_conflict` with the resolution itself so the two
    // cannot drift apart.
    if let Some((file_url, config_url)) = crate::credentials::config_url_conflict(
        config_server_url,
        auth.server_url.as_deref(),
        std::env::var("TRACEVAULT_SERVER_URL").is_ok(),
    ) {
        out.push(Check::warn(
            "Server URL",
            format!(
                ".tracevault/config.toml pins '{config_url}' but the saved login is for \
                 '{file_url}'. The config URL is ignored — commands use '{file_url}'. Run \
                 `tracevault login --server-url {config_url}` to use the repo's instance, or \
                 remove `server_url` from .tracevault/config.toml."
            ),
        ));
    }

    let server_url = auth.server_url.as_ref().unwrap();
    let client = ApiClient::with_credential(server_url, auth.credential.clone());
    match client.get_me().await {
        Ok(me) => {
            let who = me.name.unwrap_or_else(|| me.email.clone());
            let detail = match &me.role {
                Some(role) => format!("{who} <{}> (role: {role})", me.email),
                None => format!("{who} <{}>", me.email),
            };
            out.push(Check::ok("Token valid", detail));

            if let Some(cached) = &auth.email_from_creds {
                if cached != &me.email {
                    out.push(Check::warn(
                        "Credentials cache",
                        format!(
                            "cached email '{cached}' differs from server '{}' — re-run login",
                            me.email
                        ),
                    ));
                }
            }
        }
        Err(GetMeError::Unauthorized) => {
            out.push(Check::err(
                "Token valid",
                "rejected by server (expired or revoked). Run `tracevault login` again.",
            ));
        }
        // A valid token whose account isn't authorized. Re-running login
        // would NOT help, so this must not say "log in again".
        Err(GetMeError::Forbidden(_)) => {
            out.push(Check::err(
                "Account authorized",
                "the token is valid but this account lacks the `tracing` Keycloak realm role — \
                 ask an administrator to grant `tracing` (or `tracing-admin`).",
            ));
        }
        Err(GetMeError::Network(msg)) => {
            out.push(Check::warn(
                "Server reachable",
                format!("{msg} — cannot confirm token validity"),
            ));
        }
        Err(GetMeError::Server(msg)) => {
            out.push(Check::warn("Token valid", format!("server error: {msg}")));
        }
    }
    out
}

// --- Project ---

/// Subset of project checks that don't need network. Returns the loaded
/// config if present so later sections can reuse it.
fn project_checks(
    project_root: &Path,
    global_settings: Option<&Path>,
    has_global: bool,
    has_binding: bool,
) -> (Vec<Check>, Option<TracevaultConfig>) {
    let mut out = Vec::new();

    let is_git = project_root.join(".git").exists();
    if is_git {
        out.push(Check::ok(
            "Git repository",
            project_root.display().to_string(),
        ));
    } else if has_global || has_binding {
        out.push(Check::skip(
            "Git repository",
            "not a git repo (global/workspace mode)",
        ));
    } else {
        out.push(Check::err(
            "Git repository",
            "current directory is not a git repo",
        ));
    }

    let tv_present = project_root.join(".tracevault").exists();
    out.push(tracevault_init_check(tv_present, has_global, has_binding));

    // Distinguish "no config.toml" (expected in workspace/detached mode —
    // just a warning) from "config.toml present but malformed" (a genuine
    // error) via `try_load`, rather than the lenient `load` which collapses
    // both cases to `None`.
    let config = match TracevaultConfig::try_load(project_root) {
        Ok(Some(c)) => {
            let url = c.server_url.as_deref().unwrap_or("<unset>");
            out.push(Check::ok("Project config", format!("server={url}")));
            Some(c)
        }
        Ok(None) => {
            if tv_present {
                out.push(Check::warn(
                    "Project config",
                    "No .tracevault/config.toml — run `tracevault init`, or use workspace mode (`tracevault repo status`).",
                ));
            }
            None
        }
        Err(e) => {
            out.push(Check::err(
                "Project config",
                format!(".tracevault/config.toml malformed: {e}"),
            ));
            None
        }
    };

    if tv_present {
        // Hooks — presence of our markers, not just existence of the hook file.
        out.push(git_hook_check(
            project_root,
            "pre-push",
            "# tracevault:enforce",
        ));
        out.push(git_hook_check(
            project_root,
            "post-commit",
            "# tracevault:post-commit",
        ));
    } else if has_global || has_binding {
        out.push(Check::skip(
            "Git hooks",
            "not used in global/workspace mode",
        ));
    } else {
        out.push(Check::skip(
            "Git hooks",
            "not installed (no .tracevault/ — run `tracevault init`)",
        ));
    }

    out.push(claude_hook_check_in(
        &project_root.join(".claude/settings.json"),
        global_settings,
    ));

    (out, config)
}

fn git_hook_check(project_root: &Path, name: &str, marker: &str) -> Check {
    let path = project_root.join(".git/hooks").join(name);
    let label = format!("Git hook: {name}");
    if !path.exists() {
        return Check::warn(
            label,
            format!(".git/hooks/{name} missing — rerun `tracevault init`"),
        );
    }
    match fs::read_to_string(&path) {
        Ok(s) if s.contains(marker) => Check::ok(label, "installed"),
        Ok(_) => Check::warn(
            label,
            format!("{name} exists but no tracevault block — rerun `tracevault init`"),
        ),
        Err(e) => Check::warn(label, format!("cannot read hook: {e}")),
    }
}

/// True if a Claude settings.json body wires any tracevault hook command.
fn settings_has_tracevault_hooks(contents: &str) -> bool {
    contents.contains("tracevault stream")
        || contents.contains("tracevault session-start")
        || contents.contains("tracevault user-prompt")
}

/// Whether a Claude `settings.json` wires tracevault hooks — distinguishing a
/// genuinely-absent file from one that exists but can't be read, so an
/// IO/permission error isn't silently reported as "not installed".
enum SettingsHooks {
    Present,
    NoHooks,
    Missing,
    Unreadable(String),
}

fn read_settings_hooks(path: &Path) -> SettingsHooks {
    match fs::read_to_string(path) {
        Ok(s) if settings_has_tracevault_hooks(&s) => SettingsHooks::Present,
        Ok(_) => SettingsHooks::NoHooks,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SettingsHooks::Missing,
        Err(e) => SettingsHooks::Unreadable(e.to_string()),
    }
}

/// Check a global `~/.claude/settings.json` for the `init --global` install.
/// Absent / no tracevault hooks → Skip (a global install is optional); an
/// existing-but-unreadable file → Warn (don't hide a permission/IO problem).
fn global_hook_check_in(settings_path: &Path) -> Check {
    match read_settings_hooks(settings_path) {
        SettingsHooks::Present => Check::ok(
            "Global install",
            "hooks installed in ~/.claude/settings.json",
        ),
        SettingsHooks::NoHooks => Check::skip(
            "Global install",
            "~/.claude/settings.json has no tracevault hooks",
        ),
        SettingsHooks::Missing => Check::skip("Global install", "no ~/.claude/settings.json"),
        SettingsHooks::Unreadable(e) => Check::warn(
            "Global install",
            format!("cannot read ~/.claude/settings.json: {e}"),
        ),
    }
}

/// Claude Code hooks may live per-repo (`<repo>/.claude/settings.json`) OR
/// globally (`~/.claude/settings.json`, `init --global`). Ok if EITHER wires
/// tracevault; an existing-but-unreadable settings file is surfaced as a Warn
/// with the path (not a generic "not registered"); Warn if neither wires it.
fn claude_hook_check_in(repo_settings: &Path, global_settings: Option<&Path>) -> Check {
    let repo = read_settings_hooks(repo_settings);
    if matches!(repo, SettingsHooks::Present) {
        return Check::ok("Claude Code hooks", "registered in .claude/settings.json");
    }
    let global = global_settings.map(read_settings_hooks);
    if matches!(global, Some(SettingsHooks::Present)) {
        return Check::ok("Claude Code hooks", "via global install (see Installation)");
    }
    if let SettingsHooks::Unreadable(e) = &repo {
        return Check::warn(
            "Claude Code hooks",
            format!("cannot read <repo>/.claude/settings.json: {e}"),
        );
    }
    if let Some(SettingsHooks::Unreadable(e)) = &global {
        return Check::warn(
            "Claude Code hooks",
            format!("cannot read ~/.claude/settings.json: {e}"),
        );
    }
    Check::warn(
        "Claude Code hooks",
        "not registered in <repo>/.claude/settings.json or ~/.claude/settings.json (capture will miss some events)",
    )
}

/// Severity of the "TraceVault initialized" check. A per-repo `.tracevault/`
/// present is always Ok. Absent is EXPECTED (Skip) when the user has a global
/// install or an active workspace binding; only a truly-unconfigured setup
/// (none of the three) is an Error.
fn tracevault_init_check(tv_dir_present: bool, has_global: bool, has_binding: bool) -> Check {
    if tv_dir_present {
        Check::ok("TraceVault initialized", ".tracevault/ present")
    } else if has_global || has_binding {
        Check::skip(
            "TraceVault initialized",
            "no per-repo .tracevault/ — global/workspace mode",
        )
    } else {
        Check::err(
            "TraceVault initialized",
            "not set up: run `tracevault init` (per-repo) or `tracevault init --global`",
        )
    }
}

/// The most-recently-modified session in `sessions_dir` that HAS an active
/// binding, as `(session_id, binding)`. Scans ALL session files (newest first)
/// rather than only the newest file, so a real binding isn't missed when the
/// most-recently-touched session happens to have none. `None` if no session
/// carries an active binding.
fn latest_active_binding_in(
    sessions_dir: &Path,
) -> Option<(String, crate::session_state::RepoBinding)> {
    let mut sessions: Vec<(std::time::SystemTime, String)> = Vec::new();
    for entry in fs::read_dir(sessions_dir).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_str().unwrap_or("");
        let Some(id) = name.strip_suffix(".toml") else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        sessions.push((mtime, id.to_string()));
    }
    sessions.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime)); // newest first
    for (_, id) in sessions {
        if let Some(b) = crate::session_state::load_from(sessions_dir, &id).active {
            return Some((id, b));
        }
    }
    None
}

/// One clear rule for the "Workspace repo" check, shared by both arms of
/// [`workspace_binding_check_in`] below: `Ok` only when
/// `binding_repo_id_is_valid` — the SAME predicate `attribution_for` filters
/// through before honoring a repo binding — passes. A corrupted/hand-edited
/// session-state file can carry a `repo_id` that isn't a real UUID;
/// `attribution_for` silently drops such a binding, so showing it as `Ok`
/// here would be green for something the hook will not honor.
/// `detail_suffix` carries the arm-specific "(session {id})" / "most recent
/// session..." wording; the base "repo {id} via repo switch" text stays
/// identical to before this check existed.
fn workspace_repo_binding_check(
    b: &crate::session_state::RepoBinding,
    detail_suffix: &str,
) -> Check {
    let base = format!("repo {} via repo switch{detail_suffix}", b.repo_id);
    if crate::commands::stream::binding_repo_id_is_valid(&b.repo_id) {
        Check::ok("Workspace repo", base)
    } else {
        Check::warn(
            "Workspace repo",
            format!(
                "{base} — this repo_id is not a valid id and will be dropped at capture time; run `tracevault repo switch <path>` to rewrite it"
            ),
        )
    }
}

/// Resolve the workspace binding for `status` — the repo axis of
/// attribution. With an explicit session id the user named a target, but a
/// missing binding is only a Skip here: this check sees the repo axis alone,
/// so it cannot tell a genuinely unbound machine (nothing will record) from
/// one that's fully attributed via the project axis (`project switch
/// --user`). Only [`recording_check`], which sees both axes, can make that
/// call — so it also owns the escalation and the remediation hint; this
/// check states the bare fact and stops (in particular, it does NOT suggest
/// `tracevault repo switch …`: since v0.28.0 that command takes a checkout
/// path and rejects binding by session name, which is exactly the situation
/// on the repo-less machine this check is describing).
///
/// Without an explicit id, best-effort scan across all sessions (state is
/// global): show the most recent that HAS a binding, and stay quiet (Skip)
/// when none does — so a bound-mode user with a stray empty session file
/// isn't warned. Returns the Check AND the resolved binding (reused by the
/// caller as the `has_binding` mode signal).
fn workspace_binding_check_in(
    sessions_dir: &Path,
    explicit_session_id: Option<&str>,
) -> (Check, Option<crate::session_state::RepoBinding>) {
    match explicit_session_id {
        Some(id) => {
            let state = crate::session_state::load_from(sessions_dir, id);
            match state.active {
                Some(b) => {
                    let check = workspace_repo_binding_check(&b, &format!(" (session {id})"));
                    (check, Some(b))
                }
                None => (
                    Check::skip(
                        "Workspace repo",
                        format!("session {id} has no active binding"),
                    ),
                    None,
                ),
            }
        }
        None => match latest_active_binding_in(sessions_dir) {
            Some((id, b)) => {
                let suffix = format!(
                    " — most recent session with a binding: {id} (may be another repo; pass --session-id to target one)"
                );
                let check = workspace_repo_binding_check(&b, &suffix);
                (check, Some(b))
            }
            None => (
                Check::skip("Workspace repo", "no active workspace binding found"),
                None,
            ),
        },
    }
}

// --- Attribution: project axis + the combined recording verdict ---

/// The already-resolved project-attribution outcome consumed by
/// [`project_binding_check`]: either the full server-aware chain
/// (`resolve_effective_project`) or, when no API client is available, the
/// pure local tiers (`effective_project`) wrapped in `Ok` so both code paths
/// in `run_status` produce the same shape. Reuses `resolution.rs`'s own
/// `Option<(ProjectBinding, ProjectSource)>`/`Box<dyn Error>` types rather
/// than introducing a parallel enum, so `ProjectSource`'s tier labels (and
/// its `Display` impl) stay the single source of truth.
type ProjectOutcome = Result<
    Option<(crate::session_state::ProjectBinding, ProjectSource)>,
    Box<dyn std::error::Error>,
>;

/// Display label for a project binding: the friendly name, falling back to
/// the id when unset. A `Deduced` binding carries an empty `project_name`
/// (`resolve_effective_project` doesn't enrich it) and is shown by id rather
/// than name: the credentialed arm below only ever calls `list_projects`
/// when a config `default_project` is set, so a name lookup for `Deduced` is
/// not reliably available — falling back to the id keeps this consistent
/// across both cases rather than sometimes showing a name and sometimes an
/// id depending on unrelated config state.
fn project_label(binding: &crate::session_state::ProjectBinding) -> &str {
    if binding.project_name.is_empty() {
        &binding.project_id
    } else {
        &binding.project_name
    }
}

/// Maps the resolved project-attribution outcome to the "Project" check.
/// Pure and network-free — the resolution already happened in `run_status`.
/// DISPLAY ONLY: this outcome never feeds [`recording_check`]'s verdict — see
/// that function's doc comment for why.
///
/// One rule decides `Ok`: the tier must be one `capture_project` itself
/// honors (flag/subagent/session-active/user-default — NOT `ConfigDefault`
/// or `Deduced`, which it excludes/never produces), AND the binding's
/// `project_id` must parse as a UUID, since `capture_project` ends with
/// `.parse::<uuid::Uuid>().ok()?` and silently drops anything that doesn't.
/// Either failure renders `Warn`, not `Ok` — showing green for a binding the
/// capture path will not honor is the exact confusion this command exists to
/// eliminate (a hand-edited or corrupted `user_project.toml` with a garbage
/// `project_id` is a real way to hit the id-validity half of this).
///
/// `Err` — the ambiguous "this repo belongs to multiple projects" case, or
/// any transport/5xx failure of the deduction call (`resolve_effective_project`
/// propagates both the same way via `?`) — is also at most a `Warn`, never an
/// `Error`: this axis is display-only, so a network blip must never be able
/// to redden the exit code on its own. Matches how the rest of `status`
/// treats transport failure (`GetMeError::Network`, a failed `list_repos`) as
/// Warn, not Error.
///
/// `None` (nothing bound on this axis) is a `Skip` — severity for "nothing
/// will be recorded" is [`recording_check`]'s job, since only it consults the
/// actual recording gate.
fn project_binding_check(outcome: &ProjectOutcome) -> Check {
    match outcome {
        Err(e) => Check::warn("Project", e.to_string()),
        Ok(None) => Check::skip(
            "Project",
            "not bound — run `tracevault project switch --user \"<name>\"`, or `tracevault repo switch <path>` inside a checkout",
        ),
        Ok(Some((binding, source))) => {
            // `Env` (`TRACEVAULT_PROJECT`) joined this list once
            // `commands::stream::capture_project` started passing
            // `env_project_binding()` into `effective_project` — it is now
            // one of the tiers capture time actually resolves, same as
            // Subagent/SessionActive/UserDefault, so it must read Ok here
            // too or this Check would warn "not used for attribution at
            // capture time" about a tier that is, in fact, used.
            let honored_tier = matches!(
                source,
                ProjectSource::ProjectFlag
                    | ProjectSource::Subagent
                    | ProjectSource::Env
                    | ProjectSource::SessionActive
                    | ProjectSource::UserDefault
            );
            let label = project_label(binding);
            if !honored_tier {
                Check::warn(
                    "Project",
                    format!(
                        "{label} — {source}, not used for attribution at capture time; set one explicitly with `tracevault project switch <name>` or --project"
                    ),
                )
            } else if binding.project_id.parse::<uuid::Uuid>().is_err() {
                Check::warn(
                    "Project",
                    format!(
                        "{label} — {source}, but the saved project_id is not a valid id and will be dropped at capture time; run `tracevault project switch <name>` to rewrite it"
                    ),
                )
            } else {
                Check::ok("Project", format!("{label} — {source}"))
            }
        }
    }
}

/// Surfaces a `.tracevault/config.toml` `default_project` NAME that failed
/// to resolve to a real project (network failure, no matching name, or a
/// case mismatch — names are matched case-sensitively). Without this, the
/// tier is silently dropped: `commands::project`'s `status` warns for the
/// same gap (project.rs:284-289) via a plain `eprintln!`, but `status` has
/// no equivalent, so a stale/misspelled `default_project` is invisible here.
///
/// Always a `Warn`, and the wording says so explicitly: even a `default_project`
/// that DOES resolve is a `ConfigDefault` tier `capture_project` excludes (see
/// [`project_binding_check`]), so this unresolved case must not imply fixing
/// the name would make attribution work at capture time — it would still need
/// `tracevault project switch <name>` or `--project`.
fn unresolved_config_default_check(name: &str) -> Check {
    Check::warn(
        "Project default",
        format!(
            "configured default_project '{name}' could not be resolved (network error, or no matching project); even if it resolved, this tier is not used for attribution at capture time — set one with `tracevault project switch <name>` or --project instead"
        ),
    )
}

/// The one check that reflects whether anything will record this session at
/// all. Takes the ALREADY-RESOLVED [`crate::commands::stream::Attribution`] —
/// the exact same value `attribution_for(resolve_stream_binding(...),
/// capture_project(...))` the `stream` hook itself computes before deciding
/// to no-op — rather than a hand-built approximation of the two axes. Coupling
/// to the hook's own gate this way is load-bearing: the repo axis has tiers
/// (`repo switch --user`, a config with no `repo_id`) and the project axis has
/// tiers (`project switch --user` with no credential, `Deduced`/`ConfigDefault`
/// display values `capture_project` doesn't honor) that a hand-rolled
/// `repo_bound`/`project_bound` pair would have to re-derive and could get
/// wrong; calling the real functions means the two can't drift apart.
///
/// Only fires for an explicitly named session (`--session-id` /
/// `$TRACEVAULT_SESSION_ID`, i.e. `session_id.is_some()`) — a bare scan has no
/// committed target, so there's nothing to escalate about. Pure and
/// network-free: `attribution` is computed by the caller from purely local
/// tiers (mirroring the hook), so this check — and the verdict it drives —
/// can never be swayed by a network hiccup.
fn recording_check(
    session_id: Option<&str>,
    attribution: Option<&crate::commands::stream::Attribution>,
) -> Option<Check> {
    let id = session_id?;
    if attribution.is_some() {
        return None;
    }
    Some(Check::err(
        "Recording",
        format!(
            "nothing will be recorded — session {id} is bound on neither the repo nor the \
             project axis; run `tracevault project switch --user \"<name>\"` on a repo-less \
             machine, or `tracevault repo switch <path>` inside a checkout"
        ),
    ))
}

/// Compose the "will this session record anything" gate exactly as
/// `run_status` and the `stream` hook do: load the session state, derive the
/// worktree key from `cwd`, extract a bound-config `RepoBinding` (if any),
/// and call `resolve_stream_binding` -> `capture_project` -> `attribution_for`
/// with those inputs. Pure and synchronous — network-free, matching the two
/// functions it calls. Returns the worktree key alongside the attribution so
/// callers that also need it (the project-display resolution in `run_status`)
/// don't re-shell out to git for the same value.
///
/// Deliberately derives its OWN worktree key from `cwd` (via
/// `crate::paths::worktree_toplevel`) rather than taking a pre-computed key as
/// a parameter: `cwd` is the toplevel a `stream` hook invocation actually runs
/// from, which for a LINKED worktree is NOT the same string as `project_root`
/// (the PRIMARY worktree's root, from `resolve_project_root`'s
/// `git rev-parse --git-common-dir`) — subagent overrides are keyed by the
/// former. Keeping the derivation INSIDE this function, with every attribution
/// test calling it directly rather than re-deriving the key by hand, is what
/// makes `recording_attribution_finds_subagent_override_via_linked_worktree_cwd`
/// able to catch a regression back to a primary-root path.
fn recording_attribution(
    sessions_dir: Option<&Path>,
    session_id: Option<&str>,
    cwd: &Path,
    config: Option<&TracevaultConfig>,
) -> (Option<crate::commands::stream::Attribution>, String) {
    let session = match (sessions_dir, session_id) {
        (Some(dir), Some(id)) => crate::session_state::load_from(dir, id),
        _ => crate::session_state::SessionState::default(),
    };
    let worktree = crate::paths::worktree_toplevel(cwd);
    let bound = config.and_then(crate::resolution::binding_from_config);
    let user_default_repo = crate::user_default::load();
    let stream_binding = crate::commands::stream::resolve_stream_binding(
        &session,
        &worktree,
        bound,
        user_default_repo,
    );
    let capture_pid = crate::commands::stream::capture_project(&session, Some(&worktree));
    let attribution =
        crate::commands::stream::attribution_for(stream_binding.as_ref(), capture_pid);
    (attribution, worktree)
}

/// The project axis's client-backed resolution: a configured `default_project`
/// NAME and `TRACEVAULT_PROJECT` (UUID or NAME, mirroring `commands::project`'s
/// `status`) both get to consult `list_projects` here, then the full
/// server-aware precedence chain (`resolve_effective_project`) runs. Pulled
/// out analogously to [`offline_project_outcome`] so this branch is
/// unit-testable without driving all of `run_status`. Returns the outcome
/// plus the configured `default_project` NAME when it failed to resolve, for
/// the caller to surface via `unresolved_config_default_check` — an
/// unresolved `TRACEVAULT_PROJECT` NAME gets no equivalent surfacing; it is
/// silently dropped, the same treatment `commands::project`'s `status` gives
/// an unresolved `--project`.
async fn online_project_outcome(
    client: &ApiClient,
    config_default_name: Option<&str>,
    project_session: &crate::session_state::SessionState,
    worktree: &str,
    user_default_project: Option<crate::session_state::ProjectBinding>,
    project_git_url: Option<&str>,
) -> (ProjectOutcome, Option<String>) {
    // A configured default_project is a NAME; resolving it into a binding
    // needs the project list, same as project.rs's status.
    let mut config_default_unresolved = None;
    let config_default = match config_default_name {
        Some(name) => {
            let resolved = client.list_projects().await.ok().and_then(|items| {
                items.into_iter().find(|p| p.name == name).map(|p| {
                    crate::session_state::ProjectBinding {
                        project_id: p.id.to_string(),
                        project_name: p.name,
                        updated_at: chrono::Utc::now().to_rfc3339(),
                        forced_until: None,
                    }
                })
            });
            if resolved.is_none() {
                config_default_unresolved = Some(name.to_string());
            }
            resolved
        }
        None => None,
    };
    // `TRACEVAULT_PROJECT`: a UUID or a NAME — a client is in scope here, so
    // both forms are honoured (unlike the capture path, which is UUID-only).
    let env_project = match std::env::var("TRACEVAULT_PROJECT").ok() {
        Some(raw) if !raw.trim().is_empty() => {
            let raw = raw.trim().to_string();
            match raw.parse::<uuid::Uuid>() {
                Ok(id) => Some(crate::session_state::ProjectBinding {
                    project_id: id.to_string(),
                    project_name: String::new(),
                    updated_at: String::new(),
                    forced_until: None,
                }),
                // A name: resolvable here because a client is in scope.
                Err(_) => client.list_projects().await.ok().and_then(|items| {
                    items.into_iter().find(|p| p.name == raw).map(|p| {
                        crate::session_state::ProjectBinding {
                            project_id: p.id.to_string(),
                            project_name: p.name,
                            updated_at: chrono::Utc::now().to_rfc3339(),
                            forced_until: None,
                        }
                    })
                }),
            }
        }
        _ => None,
    };
    let inputs = ProjectResolveInputs {
        project_flag: None,
        env_project,
        session: project_session,
        worktree_path: Some(worktree),
        config_default,
    };
    let outcome =
        resolve_effective_project(&inputs, user_default_project, project_git_url, client).await;
    (outcome, config_default_unresolved)
}

/// The project axis's offline/no-client display fallback: the pure local
/// tiers (`effective_project`) plus the user-level project default as the
/// lowest tier. `effective_project` alone never consults the user default —
/// only `resolve_effective_project` (the networked chain) does — so without
/// this, a repo-less pod attributed solely via `project switch --user`, with
/// no credential/server URL configured, would show "not bound" on the
/// "Project" line while `recording_attribution` (which reads this same tier
/// independently via `capture_project`) correctly shows the session as
/// recording. Pulled out as its own pure function so this fallback is
/// unit-testable without driving all of `run_status`.
fn offline_project_outcome(
    inputs: &ProjectResolveInputs,
    user_default_project: Option<crate::session_state::ProjectBinding>,
) -> ProjectOutcome {
    Ok(effective_project(inputs)
        .or_else(|| user_default_project.map(|b| (b, ProjectSource::UserDefault))))
}

// --- Server repo ---

/// How `status` should locate the repo on the server.
#[derive(Debug, Clone, PartialEq)]
enum RepoMatch {
    ByName(String),
    ById(String),
}

/// Bound mode (a loaded project config, match by git repo NAME) takes
/// precedence; otherwise workspace mode (the authoritative binding, match by
/// `repo_id`). `None` when neither is available. Pure — `repo_name` is passed
/// in so this is unit-testable without invoking git.
fn server_repo_lookup(
    config_present: bool,
    repo_name: &str,
    binding: Option<&crate::session_state::RepoBinding>,
) -> Option<RepoMatch> {
    if config_present {
        return Some(RepoMatch::ByName(repo_name.to_string()));
    }
    if let Some(b) = binding {
        return Some(RepoMatch::ById(b.repo_id.clone()));
    }
    None
}

/// Find the repo matching `want`, retrying by workspace-binding id if a
/// `ByName` lookup misses. A directory rename after `tracevault init`
/// registered the repo would otherwise make the (name-based) bound-mode
/// lookup miss even though the binding's `repo_id` still resolves — since this
/// is a read-only inspector, a stale name should not produce a false "not
/// found on server" negative when a binding is available to double-check
/// against. Returns the matched repo plus the [`RepoMatch`] that actually
/// found it, so the caller can tell a direct hit from a fallback hit (the
/// fallback must not be treated as if the name-based lookup had succeeded —
/// see the `RepoMatch::ById` arm of the "Remote URL matches" check below).
/// `ById` lookups have already tried the only id available, so they are never
/// retried.
fn find_repo_with_fallback<'a>(
    repos: &'a [crate::api_client::RepoListItem],
    want: &RepoMatch,
    binding: Option<&crate::session_state::RepoBinding>,
) -> Option<(&'a crate::api_client::RepoListItem, RepoMatch)> {
    if let Some(r) = repos.iter().find(|r| match want {
        RepoMatch::ByName(name) => &r.name == name,
        RepoMatch::ById(id) => &r.id.to_string() == id,
    }) {
        return Some((r, want.clone()));
    }
    if let RepoMatch::ByName(_) = want {
        let b = binding?;
        let r = repos.iter().find(|r| r.id.to_string() == b.repo_id)?;
        return Some((r, RepoMatch::ById(b.repo_id.clone())));
    }
    None
}

async fn server_repo_checks(
    project_root: &Path,
    auth: &AuthContext,
    config: Option<&TracevaultConfig>,
    binding: Option<&crate::session_state::RepoBinding>,
) -> Vec<Check> {
    let mut out = Vec::new();

    let (Some(credential), Some(server_url)) = (auth.credential.clone(), auth.server_url.as_ref())
    else {
        out.push(Check::skip(
            "Repo registered on server",
            "not authenticated",
        ));
        return out;
    };

    let repo_name = git_repo_name(project_root);
    let Some(want) = server_repo_lookup(config.is_some(), &repo_name, binding) else {
        out.push(Check::skip(
            "Repo registered on server",
            "not bound (need a project config.toml or a --session-id workspace binding)",
        ));
        return out;
    };

    let client = ApiClient::with_credential(server_url, Some(credential));
    let repos = match client.list_repos().await {
        Ok(r) => r,
        Err(e) => {
            out.push(Check::warn(
                "Repo registered on server",
                format!("failed to list repos: {e}"),
            ));
            return out;
        }
    };

    let found = find_repo_with_fallback(&repos, &want, binding);

    match found {
        None => {
            let what = match &want {
                RepoMatch::ByName(n) => format!("'{n}'"),
                RepoMatch::ById(id) => format!("repo id {id}"),
            };
            out.push(Check::err(
                "Repo registered on server",
                format!("{what} not found on the server. Run `tracevault init` while logged in, or `tracevault sync`."),
            ));
            return out;
        }
        Some((r, matched_via)) => {
            out.push(Check::ok(
                "Repo registered on server",
                format!("id={}", r.id),
            ));
            match r.clone_status.as_deref() {
                Some("ready") => out.push(Check::ok("Server-side clone", "ready")),
                Some(other @ ("cloning" | "pending")) => out.push(Check::warn(
                    "Server-side clone",
                    format!("{other} — analytics and code browser unavailable until it finishes"),
                )),
                Some("error") => out.push(Check::err(
                    "Server-side clone",
                    "error — check the repo settings page on the dashboard",
                )),
                Some(other) => out.push(Check::warn(
                    "Server-side clone",
                    format!("unknown status '{other}'"),
                )),
                None => out.push(Check::skip(
                    "Server-side clone",
                    "server did not report clone status",
                )),
            }

            match &matched_via {
                RepoMatch::ByName(_) => {
                    let local_remote = git_remote_url(project_root);
                    match (local_remote.as_deref(), r.github_url.as_deref()) {
                        (Some(local), Some(remote))
                            if normalize_remote(local) == normalize_remote(remote) =>
                        {
                            out.push(Check::ok("Remote URL matches", remote.to_string()));
                        }
                        (Some(local), Some(remote)) => out.push(Check::warn(
                            "Remote URL matches",
                            format!("local={local} vs server={remote} — run `tracevault sync`"),
                        )),
                        (Some(local), None) => out.push(Check::warn(
                            "Remote URL matches",
                            format!("server has no github_url; local={local}"),
                        )),
                        (None, _) => out.push(Check::warn(
                            "Remote URL matches",
                            "no local `origin` remote configured",
                        )),
                    }
                }
                RepoMatch::ById(_) => {
                    out.push(Check::skip(
                        "Remote URL matches",
                        "n/a in workspace mode (bound by name/id)",
                    ));
                }
            }
        }
    }

    out
}

// --- Sessions ---

/// True for a per-repo `pending-<id>.jsonl` (nonempty id) queue file —
/// tighter than a bare `starts_with("pending") && ends_with(".jsonl")`
/// check, which would also match e.g. `pendingfoo.jsonl` or `pending-.jsonl`.
/// The legacy `pending.jsonl` name is handled separately by callers, since
/// whether it counts depends on whether there's a bound `repo_id` to
/// attribute it to (see `count_pending_events`).
fn is_pending_queue_filename(name: &str) -> bool {
    crate::commands::flush::repo_id_from_pending_filename(name).is_some()
}

/// Sum the non-empty lines across every per-repo (and, if `include_legacy`,
/// legacy) pending queue file in a session directory: `pending.jsonl` and
/// `pending-<repo_id>.jsonl`.
///
/// `include_legacy` should be true only when the project has a bound
/// `repo_id` (config.toml), since `flush` only drains the legacy
/// `pending.jsonl` in that case (best-effort attribution). Otherwise a
/// stray `pending.jsonl` would be counted here but never cleared by
/// `flush`, leaving `status` permanently reporting stuck events.
fn count_pending_events(session_dir: &Path, include_legacy: bool) -> usize {
    let Ok(read) = fs::read_dir(session_dir) else {
        return 0;
    };

    read.flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|name| {
                    is_pending_queue_filename(name) || (include_legacy && name == "pending.jsonl")
                })
                .unwrap_or(false)
        })
        .map(|entry| {
            fs::read_to_string(entry.path())
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count()
        })
        .sum()
}

fn session_checks(project_root: &Path) -> Vec<Check> {
    let sessions_dir = project_root.join(".tracevault/sessions");
    if !sessions_dir.exists() {
        return vec![Check::skip(
            "Pending events",
            "no .tracevault/sessions/ here (project-local check; a detached worker's queue may live under its own working dir)",
        )];
    }

    // The legacy `pending.jsonl` is only drainable by `flush` when there's a
    // bound `repo_id` to attribute it to (best-effort attribution). Counting
    // it here otherwise would report events that `flush` can never clear.
    let include_legacy = TracevaultConfig::load(project_root)
        .and_then(|c| c.repo_id)
        .is_some();

    let mut total_sessions = 0usize;
    let mut sessions_with_pending = 0usize;
    let mut pending_event_count = 0usize;

    if let Ok(read) = fs::read_dir(&sessions_dir) {
        for entry in read.flatten() {
            if !entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                continue;
            }
            total_sessions += 1;
            let count = count_pending_events(&entry.path(), include_legacy);
            if count > 0 {
                sessions_with_pending += 1;
                pending_event_count += count;
            }
        }
    }

    vec![if sessions_with_pending == 0 {
        Check::ok(
            "Pending events",
            format!("{total_sessions} session(s), all synced (project-local)"),
        )
    } else {
        Check::warn(
            "Pending events",
            format!(
                "{pending_event_count} event(s) in {sessions_with_pending}/{total_sessions} session(s) — run `tracevault flush`"
            ),
        )
    }]
}

// --- Git helpers ---

/// Make two remote URLs comparable by dropping `.git`, trailing slash, and
/// collapsing SSH ↔ HTTPS differences for github.com specifically.
fn normalize_remote(url: &str) -> String {
    let trimmed = url
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_string();

    // git@github.com:org/repo  ->  github.com/org/repo
    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        return format!("github.com/{rest}");
    }
    // https://github.com/org/repo -> github.com/org/repo
    for p in ["https://", "http://"] {
        if let Some(rest) = trimmed.strip_prefix(p) {
            return rest.to_string();
        }
    }
    trimmed
}

// --- Entry point ---

/// Resolve the session id for `status`: an explicit `--session-id` wins, else
/// `$TRACEVAULT_SESSION_ID`; empty strings are ignored (fall through).
pub fn effective_session_id(arg: Option<String>, env: Option<String>) -> Option<String> {
    arg.filter(|s| !s.is_empty())
        .or_else(|| env.filter(|s| !s.is_empty()))
}

pub async fn run_status(project_root: &Path, cwd: &Path, session_id: Option<&str>) -> i32 {
    // Loaded here rather than reused from `project_checks` below, which runs
    // later and owns its own load. `try_load`, not `load`: `load` prints its own
    // stderr line on malformed TOML, which would pre-empt (and duplicate) the
    // structured parse error `project_checks` reports. A config that will not
    // parse simply contributes nothing here.
    let auth_config = TracevaultConfig::try_load(project_root).ok().flatten();
    let config_server_url = auth_config.as_ref().and_then(|c| c.server_url.as_deref());
    let auth = resolve_auth(
        config_server_url,
        auth_config.as_ref().and_then(|c| c.api_key.as_deref()),
    );

    // ~/.claude/settings.json — None when the home dir can't be resolved. Do
    // NOT fall back to a relative `.claude/settings.json`: that would read the
    // repo's own settings and could report a false "Global install".
    let global_settings: Option<PathBuf> =
        dirs::home_dir().map(|h| h.join(".claude").join("settings.json"));
    let sessions_dir = crate::session_state::sessions_dir();

    let global_check = match global_settings.as_deref() {
        Some(p) => global_hook_check_in(p),
        None => Check::skip("Global install", "cannot determine home directory"),
    };
    let has_global = global_check.level == Level::Ok;

    // Resolve the workspace binding ONCE: it drives both the section and the
    // `has_binding` mode signal used by project_checks.
    let (binding_check, binding) = match sessions_dir.as_deref() {
        Some(dir) => workspace_binding_check_in(dir, session_id),
        None => (
            Check::skip("Workspace repo", "cannot determine session state dir"),
            None,
        ),
    };
    // A binding only counts as a mode signal when the session was named
    // explicitly (--session-id / $TRACEVAULT_SESSION_ID). A *scanned* binding
    // (no session id) is cwd-agnostic and may belong to another repo, so it is
    // shown for information but must NOT downgrade "not set up" severity nor
    // drive the server-repo check.
    let explicit_session = session_id.is_some();
    let has_binding = binding.is_some() && explicit_session;
    let authoritative_binding = if explicit_session {
        binding.as_ref()
    } else {
        None
    };

    let auth_checks_v = auth_checks(&auth, config_server_url).await;
    let install_v = vec![global_check];
    let (proj_checks_v, config) = project_checks(
        project_root,
        global_settings.as_deref(),
        has_global,
        has_binding,
    );

    // The recording verdict: call the hook's own gate (stream.rs) via
    // `recording_attribution` — see that function's doc comment for why its
    // composition, including the worktree-key derivation, lives there rather
    // than inline here. Its worktree key is reused below for the project
    // display resolution, which needs the same value.
    let (attribution, worktree) =
        recording_attribution(sessions_dir.as_deref(), session_id, cwd, config.as_ref());

    // The project axis for DISPLAY needs its own full `SessionState` (more
    // than `workspace_binding_check_in`'s bare `RepoBinding`, e.g.
    // `active_project`/`subagent_projects`) — unlike the verdict above, which
    // lives entirely inside `recording_attribution`. Loaded separately
    // (rather than sharing `recording_attribution`'s internal session load)
    // so that function's session load stays self-contained.
    let project_session = match (sessions_dir.as_deref(), session_id) {
        (Some(dir), Some(id)) => crate::session_state::load_from(dir, id),
        _ => crate::session_state::SessionState::default(),
    };

    // --- The project axis for DISPLAY only, mirroring `commands::project`'s
    // `status` path (project.rs:228-334): read the worktree-relative
    // default_project + the user-level default, and run the full precedence
    // chain (including server-side deduction) if a client can be built from
    // the existing `auth`. Best-effort like that path: no credential/client
    // degrades to the pure local tiers via `effective_project`, which need no
    // network. Whatever this resolves to (including a deduction failure) is
    // shown to the user but — unlike the verdict above — can never affect the
    // exit code beyond a Warn; see `project_binding_check`.
    let config_default_name = config.as_ref().and_then(|c| c.default_project.clone());
    let user_default_project = crate::user_project_default::load();
    let project_git_url = git_remote_url(cwd);
    // Set when a configured `default_project` NAME fails to resolve (network
    // error, or no matching/case-matching project) — surfaced below rather
    // than silently dropped, mirroring `commands::project`'s `status`
    // (project.rs:284-289). `None` in EITHER arm: the offline arm never
    // attempts resolution at all (no client to call `list_projects` with),
    // which is a different, already-visible gap (no credential -> an Error
    // from the Authentication section).
    let (project_outcome, config_default_unresolved): (ProjectOutcome, Option<String>) =
        match (auth.credential.clone(), auth.server_url.as_ref()) {
            (Some(credential), Some(server_url)) => {
                let client = ApiClient::with_credential(server_url, Some(credential));
                online_project_outcome(
                    &client,
                    config_default_name.as_deref(),
                    &project_session,
                    &worktree,
                    user_default_project,
                    project_git_url.as_deref(),
                )
                .await
            }
            _ => {
                // Offline (no client): only the UUID form of
                // `TRACEVAULT_PROJECT` can be honoured — a name would need
                // `list_projects`, which needs a client this arm doesn't
                // have. Reuses `commands::stream`'s capture-time helper
                // rather than duplicating the same UUID-only parse a third
                // time (the `project.rs` inline version stays separate: it
                // must distinguish "not a UUID" from "absent" so it can fall
                // through to name resolution, which this `Option`-returning
                // helper can't express).
                let inputs = ProjectResolveInputs {
                    project_flag: None,
                    env_project: crate::commands::stream::env_project_binding(),
                    session: &project_session,
                    worktree_path: Some(&worktree),
                    config_default: None,
                };
                (offline_project_outcome(&inputs, user_default_project), None)
            }
        };

    let mut attribution_v = Vec::new();
    if let Some(c) = recording_check(session_id, attribution.as_ref()) {
        attribution_v.push(c);
    }
    attribution_v.push(binding_check);
    attribution_v.push(project_binding_check(&project_outcome));
    if let Some(name) = config_default_unresolved {
        attribution_v.push(unresolved_config_default_check(&name));
    }

    let server_checks_v =
        server_repo_checks(project_root, &auth, config.as_ref(), authoritative_binding).await;
    let session_checks_v = session_checks(project_root);

    let sections: Vec<(&str, Vec<Check>)> = vec![
        ("Authentication", auth_checks_v),
        ("Installation", install_v),
        ("Project", proj_checks_v),
        ("Attribution", attribution_v),
        ("Server repo", server_checks_v),
        ("Sessions", session_checks_v),
    ];
    for (title, checks) in &sections {
        print_section(title, checks);
    }
    let all: Vec<&Check> = sections.iter().flat_map(|(_, v)| v.iter()).collect();

    let errors = all.iter().filter(|c| c.level == Level::Error).count();
    let warns = all.iter().filter(|c| c.level == Level::Warn).count();

    match (errors, warns) {
        (0, 0) => println!("{ANSI_GREEN}All good.{ANSI_RESET}"),
        (0, w) => println!(
            "{ANSI_YELLOW}{w} warning{} — no blocking issues.{ANSI_RESET}",
            if w == 1 { "" } else { "s" }
        ),
        (e, 0) => println!(
            "{ANSI_RED}{e} problem{} found.{ANSI_RESET}",
            if e == 1 { "" } else { "s" }
        ),
        (e, w) => println!(
            "{ANSI_RED}{e} problem{}, {w} warning{}.{ANSI_RESET}",
            if e == 1 { "" } else { "s" },
            if w == 1 { "" } else { "s" }
        ),
    }

    exit_code_for(&all)
}

/// Only `Level::Error` checks force a non-zero exit; `Warn` and `Skip` are
/// surfaced to the user but don't fail the command. Pulled out as a pure
/// function so the severity→exit-code mapping is unit-testable independent
/// of the network-calling `run_status`.
fn exit_code_for(checks: &[&Check]) -> i32 {
    if checks.iter().any(|c| c.level == Level::Error) {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_remote_ssh_https_equal() {
        assert_eq!(
            normalize_remote("git@github.com:VirtusLab/visdom-ai-tracing.git"),
            normalize_remote("https://github.com/VirtusLab/visdom-ai-tracing")
        );
        assert_eq!(
            normalize_remote("https://github.com/VirtusLab/visdom-ai-tracing.git/"),
            "github.com/VirtusLab/visdom-ai-tracing"
        );
    }

    #[test]
    fn normalize_remote_preserves_non_github() {
        assert_eq!(
            normalize_remote("git@gitlab.com:foo/bar.git"),
            "git@gitlab.com:foo/bar"
        );
    }

    #[test]
    fn git_hook_check_missing_file_is_warning() {
        let dir = tempfile::tempdir().unwrap();
        let check = git_hook_check(dir.path(), "pre-push", "# tracevault:enforce");
        assert_eq!(check.level, Level::Warn);
    }

    #[test]
    fn git_hook_check_with_marker_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(
            hooks.join("pre-push"),
            "#!/bin/sh\n# tracevault:enforce\ntracevault check\n",
        )
        .unwrap();
        let check = git_hook_check(dir.path(), "pre-push", "# tracevault:enforce");
        assert_eq!(check.level, Level::Ok);
    }

    #[test]
    fn git_hook_check_without_marker_is_warning() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(hooks.join("pre-push"), "#!/bin/sh\necho hi\n").unwrap();
        let check = git_hook_check(dir.path(), "pre-push", "# tracevault:enforce");
        assert_eq!(check.level, Level::Warn);
    }

    #[test]
    fn project_checks_errors_without_tracevault_dir_and_no_global_or_binding() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let missing_global = dir.path().join("no-global.json");
        let (checks, cfg) =
            project_checks(dir.path(), Some(missing_global.as_path()), false, false);
        assert!(cfg.is_none());
        assert!(checks
            .iter()
            .any(|c| c.level == Level::Error && c.label == "TraceVault initialized"));
        // The Git-hooks Skip must NOT claim "global/workspace mode" when the
        // user is truly unconfigured (no global install, no binding).
        let git_hooks = checks
            .iter()
            .find(|c| c.label == "Git hooks")
            .expect("Git hooks check present");
        assert_eq!(git_hooks.level, Level::Skip);
        assert!(
            !git_hooks.detail.contains("global/workspace mode"),
            "unconfigured git-hooks detail must not claim global/workspace mode: {}",
            git_hooks.detail
        );
    }

    #[test]
    fn project_checks_no_tracevault_dir_is_not_error_in_global_mode() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let missing_global = dir.path().join("no-global.json");
        let (checks, _) = project_checks(dir.path(), Some(missing_global.as_path()), true, false);
        let refs: Vec<&Check> = checks.iter().collect();
        assert_eq!(
            exit_code_for(&refs),
            0,
            "global mode must not force exit 1 on missing .tracevault/"
        );
        assert!(checks
            .iter()
            .any(|c| c.label == "TraceVault initialized" && c.level == Level::Skip));
    }

    #[test]
    fn project_checks_warns_when_config_toml_missing() {
        // Pure workspace/detached-mode user: .tracevault/ exists (or not,
        // doesn't matter for this check) but there's no config.toml because
        // `tracevault init` was never run. This must be a Warn, not an
        // Error, and therefore must not force the process exit code to 1.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join(".tracevault")).unwrap();
        let missing_global = dir.path().join("no-global.json");
        let (checks, cfg) =
            project_checks(dir.path(), Some(missing_global.as_path()), false, false);
        assert!(cfg.is_none());
        let project_config_check = checks
            .iter()
            .find(|c| c.label == "Project config")
            .expect("Project config check present");
        assert_eq!(project_config_check.level, Level::Warn);

        let refs: Vec<&Check> = checks.iter().collect();
        assert_eq!(
            exit_code_for(&refs),
            0,
            "a missing config.toml alone must not force a non-zero exit code"
        );
    }

    #[test]
    fn project_checks_errors_on_malformed_config_toml() {
        // A config.toml that exists but fails to parse is a genuine error,
        // distinct from "no config.toml at all" — it should stay Check::err
        // and keep forcing a non-zero exit code.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join(".tracevault")).unwrap();
        std::fs::write(
            dir.path().join(".tracevault/config.toml"),
            "not valid toml {{{",
        )
        .unwrap();
        let missing_global = dir.path().join("no-global.json");
        let (checks, cfg) =
            project_checks(dir.path(), Some(missing_global.as_path()), false, false);
        assert!(cfg.is_none());
        let project_config_check = checks
            .iter()
            .find(|c| c.label == "Project config")
            .expect("Project config check present");
        assert_eq!(project_config_check.level, Level::Error);

        let refs: Vec<&Check> = checks.iter().collect();
        assert_eq!(exit_code_for(&refs), 1);
    }

    #[test]
    fn exit_code_for_warn_only_is_zero() {
        let checks = [
            Check::warn("a", ""),
            Check::ok("b", ""),
            Check::skip("c", ""),
        ];
        let refs: Vec<&Check> = checks.iter().collect();
        assert_eq!(exit_code_for(&refs), 0);
    }

    #[test]
    fn exit_code_for_any_error_is_one() {
        let checks = [
            Check::ok("a", ""),
            Check::err("b", ""),
            Check::warn("c", ""),
        ];
        let refs: Vec<&Check> = checks.iter().collect();
        assert_eq!(exit_code_for(&refs), 1);
    }

    #[test]
    fn count_pending_events_sums_across_per_repo_queues() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pending-a.jsonl"), "line1\nline2\n").unwrap();
        std::fs::write(dir.path().join("pending-b.jsonl"), "line1\n").unwrap();
        std::fs::write(dir.path().join("pending-c.jsonl"), "").unwrap();
        assert_eq!(count_pending_events(dir.path(), false), 3);
        assert_eq!(count_pending_events(dir.path(), true), 3);
    }

    #[test]
    fn count_pending_events_ignores_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pending-a.jsonl"), "line1\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "line1\nline2\n").unwrap();
        assert_eq!(count_pending_events(dir.path(), false), 1);
    }

    #[test]
    fn count_pending_events_zero_for_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(count_pending_events(dir.path(), false), 0);
        assert_eq!(count_pending_events(dir.path(), true), 0);
    }

    #[test]
    fn is_pending_queue_filename_matches_per_repo_only() {
        assert!(!is_pending_queue_filename("pending.jsonl"));
        // Repo-less queues must be counted too. This holds today only
        // because `repo_id_from_pending_filename` matches the shared
        // `pending-` prefix and `status` never validates the id — i.e. by
        // accident. Pin it, so tightening that helper to reject the project
        // shape (which its name invites) cannot silently make `status` report
        // "0 pending" over a non-empty queue that `flush` still drains.
        assert!(is_pending_queue_filename(
            "pending-project-018f0000-0000-7000-8000-000000000abc.jsonl"
        ));
        assert!(is_pending_queue_filename("pending-a.jsonl"));
    }

    #[test]
    fn is_pending_queue_filename_rejects_lookalikes() {
        assert!(!is_pending_queue_filename("pendingfoo.jsonl"));
        assert!(!is_pending_queue_filename("pending-.jsonl"));
    }

    #[test]
    fn count_pending_events_legacy_pending_jsonl_requires_include_legacy() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pending.jsonl"), "line1\nline2\n").unwrap();
        assert_eq!(
            count_pending_events(dir.path(), false),
            0,
            "legacy pending.jsonl must not be counted when there's no bound repo_id"
        );
        assert_eq!(
            count_pending_events(dir.path(), true),
            2,
            "legacy pending.jsonl counts once a repo_id is bound"
        );
    }

    #[test]
    fn count_pending_events_legacy_and_per_repo_both_count_when_included() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pending.jsonl"), "line1\n").unwrap();
        std::fs::write(dir.path().join("pending-a.jsonl"), "line1\nline2\n").unwrap();
        assert_eq!(count_pending_events(dir.path(), false), 2);
        assert_eq!(count_pending_events(dir.path(), true), 3);
    }

    #[test]
    fn count_pending_events_ignores_lookalike_filenames() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pendingfoo.jsonl"), "line1\n").unwrap();
        std::fs::write(dir.path().join("pending-.jsonl"), "line1\n").unwrap();
        assert_eq!(count_pending_events(dir.path(), false), 0);
        assert_eq!(count_pending_events(dir.path(), true), 0);
    }

    #[test]
    fn settings_has_tracevault_hooks_detects_commands() {
        assert!(settings_has_tracevault_hooks(
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"command":"tracevault stream --event pre"}]}]}}"#
        ));
        assert!(settings_has_tracevault_hooks(
            r#"{"command":"tracevault session-start"}"#
        ));
        assert!(settings_has_tracevault_hooks(
            r#"{"command":"tracevault user-prompt"}"#
        ));
        assert!(!settings_has_tracevault_hooks(
            r#"{"hooks":{"PreToolUse":[]}}"#
        ));
        assert!(!settings_has_tracevault_hooks("{}"));
    }

    #[test]
    fn global_hook_check_ok_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(
            &p,
            r#"{"hooks":{"SessionStart":[{"hooks":[{"command":"tracevault session-start"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(global_hook_check_in(&p).level, Level::Ok);
    }

    #[test]
    fn global_hook_check_skip_when_absent_or_no_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("settings.json");
        assert_eq!(global_hook_check_in(&missing).level, Level::Skip);
        let empty = dir.path().join("empty.json");
        std::fs::write(&empty, "{}").unwrap();
        assert_eq!(global_hook_check_in(&empty).level, Level::Skip);
    }

    #[test]
    fn global_hook_check_warns_when_unreadable() {
        // A path that exists but isn't a readable file (a directory) yields a
        // non-NotFound read error → Warn, not a false "no settings.json" Skip.
        let dir = tempfile::tempdir().unwrap();
        let as_dir = dir.path().join("settings.json");
        std::fs::create_dir(&as_dir).unwrap();
        let check = global_hook_check_in(&as_dir);
        assert_eq!(check.level, Level::Warn);
        assert!(
            check.detail.contains("cannot read"),
            "detail: {}",
            check.detail
        );
    }

    #[test]
    fn claude_hook_check_ok_from_global_only() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo-settings.json"); // does not exist
        let global = dir.path().join("global-settings.json");
        std::fs::write(&global, r#"{"command":"tracevault stream"}"#).unwrap();
        let c = claude_hook_check_in(&repo, Some(global.as_path()));
        assert_eq!(c.level, Level::Ok);
        assert!(
            c.detail.to_lowercase().contains("global") || c.detail.contains("~/.claude"),
            "detail: {}",
            c.detail
        );
    }

    #[test]
    fn claude_hook_check_ok_from_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo-settings.json");
        std::fs::write(&repo, r#"{"command":"tracevault stream"}"#).unwrap();
        let global = dir.path().join("global-settings.json"); // does not exist
        assert_eq!(
            claude_hook_check_in(&repo, Some(global.as_path())).level,
            Level::Ok
        );
    }

    #[test]
    fn claude_hook_check_no_global_path_uses_repo_only() {
        // When the home dir can't be resolved (global_settings = None), only the
        // repo settings are consulted — never a relative fallback.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo-settings.json");
        std::fs::write(&repo, r#"{"command":"tracevault stream"}"#).unwrap();
        assert_eq!(claude_hook_check_in(&repo, None).level, Level::Ok);

        let missing = dir.path().join("nope.json");
        assert_eq!(claude_hook_check_in(&missing, None).level, Level::Warn);
    }

    #[test]
    fn claude_hook_check_warn_when_neither() {
        let dir = tempfile::tempdir().unwrap();
        let c = claude_hook_check_in(
            &dir.path().join("a.json"),
            Some(dir.path().join("b.json").as_path()),
        );
        assert_eq!(c.level, Level::Warn);
    }

    #[test]
    fn claude_hook_check_warns_when_repo_settings_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo-settings.json");
        std::fs::create_dir(&repo).unwrap(); // directory-as-file → non-NotFound read error
        let global = dir.path().join("global.json"); // missing
        let c = claude_hook_check_in(&repo, Some(global.as_path()));
        assert_eq!(c.level, Level::Warn);
        assert!(c.detail.contains("cannot read"), "detail: {}", c.detail);
    }

    #[test]
    fn tracevault_init_check_severity_matrix() {
        assert_eq!(tracevault_init_check(true, false, false).level, Level::Ok); // present → Ok
        assert_eq!(tracevault_init_check(false, true, false).level, Level::Skip); // absent + global → Skip
        assert_eq!(tracevault_init_check(false, false, true).level, Level::Skip); // absent + binding → Skip
        assert_eq!(tracevault_init_check(false, true, true).level, Level::Skip);
        assert_eq!(
            tracevault_init_check(false, false, false).level,
            Level::Error
        ); // nothing → Error
    }

    #[test]
    fn workspace_binding_explicit_id_with_binding_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let st = crate::session_state::SessionState {
            active: Some(crate::session_state::RepoBinding {
                repo_id: "11111111-1111-4111-8111-111111111111".into(),
                git_url: None,
                remote_id: None,
                codebase_name: None,
                updated_at: "t".into(),
            }),
            ..Default::default()
        };
        std::fs::write(
            dir.path().join("sess-9.toml"),
            toml::to_string(&st).unwrap(),
        )
        .unwrap();
        let (check, binding) = workspace_binding_check_in(dir.path(), Some("sess-9"));
        assert_eq!(check.level, Level::Ok);
        assert!(
            check
                .detail
                .contains("11111111-1111-4111-8111-111111111111"),
            "detail: {}",
            check.detail
        );
        assert_eq!(
            binding.unwrap().repo_id,
            "11111111-1111-4111-8111-111111111111"
        );
    }

    #[test]
    fn workspace_binding_explicit_id_without_binding_is_skip() {
        // Severity moved to `recording_check`, which is the only place that
        // sees both attribution axes — this check alone can't tell a
        // genuinely unbound machine from one attributed via the project axis.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("sess-empty.toml"),
            toml::to_string(&crate::session_state::SessionState::default()).unwrap(),
        )
        .unwrap();
        let (check, binding) = workspace_binding_check_in(dir.path(), Some("sess-empty"));
        assert_eq!(check.level, Level::Skip);
        assert!(binding.is_none());
    }

    #[test]
    fn workspace_binding_no_id_empty_dir_is_skip() {
        let dir = tempfile::tempdir().unwrap();
        let (check, binding) = workspace_binding_check_in(dir.path(), None);
        assert_eq!(check.level, Level::Skip);
        assert!(binding.is_none());
    }

    #[test]
    fn workspace_binding_no_id_scans_latest() {
        let dir = tempfile::tempdir().unwrap();
        // Two sessions; the one written LAST should be picked by the mtime scan.
        let older = crate::session_state::SessionState {
            active: Some(crate::session_state::RepoBinding {
                repo_id: "aaaaaaaa-1111-4111-8111-111111111111".into(),
                git_url: None,
                remote_id: None,
                codebase_name: None,
                updated_at: "t".into(),
            }),
            ..Default::default()
        };
        std::fs::write(
            dir.path().join("old.toml"),
            toml::to_string(&older).unwrap(),
        )
        .unwrap();
        // Ensure a distinct, later mtime for the second file.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let newer = crate::session_state::SessionState {
            active: Some(crate::session_state::RepoBinding {
                repo_id: "bbbbbbbb-2222-4222-8222-222222222222".into(),
                git_url: None,
                remote_id: None,
                codebase_name: None,
                updated_at: "t".into(),
            }),
            ..Default::default()
        };
        std::fs::write(
            dir.path().join("new.toml"),
            toml::to_string(&newer).unwrap(),
        )
        .unwrap();
        let (check, binding) = workspace_binding_check_in(dir.path(), None);
        assert_eq!(check.level, Level::Ok);
        assert_eq!(
            binding.unwrap().repo_id,
            "bbbbbbbb-2222-4222-8222-222222222222",
            "must pick the most-recently-modified session"
        );
    }

    #[test]
    fn workspace_binding_scan_skips_newest_without_binding() {
        // The most-recently-touched session has NO active binding, but an older
        // one does — the scan must find the older binding, not stop at the newest.
        let dir = tempfile::tempdir().unwrap();
        let with_binding = crate::session_state::SessionState {
            active: Some(crate::session_state::RepoBinding {
                repo_id: "aaaaaaaa-1111-4111-8111-111111111111".into(),
                git_url: None,
                remote_id: None,
                codebase_name: None,
                updated_at: "t".into(),
            }),
            ..Default::default()
        };
        std::fs::write(
            dir.path().join("older.toml"),
            toml::to_string(&with_binding).unwrap(),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // Newest file: no active binding.
        std::fs::write(
            dir.path().join("newest.toml"),
            toml::to_string(&crate::session_state::SessionState::default()).unwrap(),
        )
        .unwrap();
        let (check, binding) = workspace_binding_check_in(dir.path(), None);
        assert_eq!(check.level, Level::Ok);
        assert_eq!(
            binding.unwrap().repo_id,
            "aaaaaaaa-1111-4111-8111-111111111111"
        );
    }

    #[test]
    fn workspace_binding_scan_no_active_binding_is_skip_not_warn() {
        // A session file exists but has no active binding (e.g. a bound-mode
        // user with a stray session). Scan mode must Skip, not Warn.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("stray.toml"),
            toml::to_string(&crate::session_state::SessionState::default()).unwrap(),
        )
        .unwrap();
        let (check, binding) = workspace_binding_check_in(dir.path(), None);
        assert_eq!(check.level, Level::Skip);
        assert!(binding.is_none());
    }

    #[test]
    fn workspace_binding_explicit_id_malformed_repo_id_is_warn_not_ok() {
        // `attribution_for` filters a resolved binding through
        // `binding_repo_id_is_valid` and drops it if the check fails; a
        // corrupted session-state file with a non-UUID `repo_id` must not
        // read Ok here either.
        let dir = tempfile::tempdir().unwrap();
        let st = crate::session_state::SessionState {
            active: Some(crate::session_state::RepoBinding {
                repo_id: "not-a-uuid".into(),
                git_url: None,
                remote_id: None,
                codebase_name: None,
                updated_at: "t".into(),
            }),
            ..Default::default()
        };
        std::fs::write(
            dir.path().join("sess-bad.toml"),
            toml::to_string(&st).unwrap(),
        )
        .unwrap();
        let (check, binding) = workspace_binding_check_in(dir.path(), Some("sess-bad"));
        assert_eq!(check.level, Level::Warn);
        assert!(
            check.detail.contains("not a valid id"),
            "detail: {}",
            check.detail
        );
        // The raw binding is still returned (unchanged mode signal for
        // callers) — only this check's OWN severity changes.
        assert_eq!(binding.unwrap().repo_id, "not-a-uuid");
    }

    #[test]
    fn workspace_binding_scan_malformed_repo_id_is_warn_not_ok() {
        // Same rule, scan arm: `latest_active_binding_in` doesn't validate
        // either, so the same corrupted binding could surface via a scan.
        let dir = tempfile::tempdir().unwrap();
        let st = crate::session_state::SessionState {
            active: Some(crate::session_state::RepoBinding {
                repo_id: "not-a-uuid".into(),
                git_url: None,
                remote_id: None,
                codebase_name: None,
                updated_at: "t".into(),
            }),
            ..Default::default()
        };
        std::fs::write(
            dir.path().join("sess-bad.toml"),
            toml::to_string(&st).unwrap(),
        )
        .unwrap();
        let (check, binding) = workspace_binding_check_in(dir.path(), None);
        assert_eq!(check.level, Level::Warn);
        assert!(
            check.detail.contains("not a valid id"),
            "detail: {}",
            check.detail
        );
        assert_eq!(binding.unwrap().repo_id, "not-a-uuid");
    }

    fn repo_item(id: &str, name: &str) -> crate::api_client::RepoListItem {
        crate::api_client::RepoListItem {
            id: id.parse().unwrap(),
            name: name.to_string(),
            github_url: None,
            clone_status: None,
        }
    }

    #[test]
    fn find_repo_with_fallback_direct_name_hit_no_fallback_needed() {
        let repos = vec![repo_item("11111111-1111-4111-8111-111111111111", "myrepo")];
        let want = RepoMatch::ByName("myrepo".to_string());
        let (r, matched_via) = find_repo_with_fallback(&repos, &want, None).unwrap();
        assert_eq!(r.name, "myrepo");
        assert_eq!(matched_via, want);
    }

    #[test]
    fn find_repo_with_fallback_falls_back_to_binding_id_on_name_miss() {
        // Local checkout directory was renamed after registration: the
        // git-derived name ("renamed") no longer matches the server's
        // registered name ("myrepo"), but the workspace binding still carries
        // the correct repo_id.
        let id = "11111111-1111-4111-8111-111111111111";
        let repos = vec![repo_item(id, "myrepo")];
        let binding = crate::session_state::RepoBinding {
            repo_id: id.to_string(),
            git_url: None,
            remote_id: None,
            codebase_name: None,
            updated_at: "t".into(),
        };
        let want = RepoMatch::ByName("renamed".to_string());
        let (r, matched_via) = find_repo_with_fallback(&repos, &want, Some(&binding))
            .expect("fallback by binding id must find the repo");
        assert_eq!(r.name, "myrepo");
        assert_eq!(matched_via, RepoMatch::ById(id.to_string()));
    }

    #[test]
    fn find_repo_with_fallback_none_when_name_misses_and_no_binding() {
        let repos = vec![repo_item("11111111-1111-4111-8111-111111111111", "myrepo")];
        let want = RepoMatch::ByName("renamed".to_string());
        assert!(find_repo_with_fallback(&repos, &want, None).is_none());
    }

    #[test]
    fn find_repo_with_fallback_none_when_name_and_binding_both_miss() {
        let repos = vec![repo_item("11111111-1111-4111-8111-111111111111", "myrepo")];
        let binding = crate::session_state::RepoBinding {
            repo_id: "22222222-2222-4222-8222-222222222222".to_string(),
            git_url: None,
            remote_id: None,
            codebase_name: None,
            updated_at: "t".into(),
        };
        let want = RepoMatch::ByName("renamed".to_string());
        assert!(find_repo_with_fallback(&repos, &want, Some(&binding)).is_none());
    }

    #[test]
    fn find_repo_with_fallback_by_id_miss_is_never_retried() {
        // An ById lookup has already tried the only id available — a miss
        // stays a miss even with a (necessarily-matching-or-irrelevant)
        // binding present.
        let repos = vec![repo_item("11111111-1111-4111-8111-111111111111", "myrepo")];
        let binding = crate::session_state::RepoBinding {
            repo_id: "99999999-9999-4999-8999-999999999999".to_string(),
            git_url: None,
            remote_id: None,
            codebase_name: None,
            updated_at: "t".into(),
        };
        let want = RepoMatch::ById("99999999-9999-4999-8999-999999999999".to_string());
        assert!(find_repo_with_fallback(&repos, &want, Some(&binding)).is_none());
    }

    #[test]
    fn server_repo_lookup_prefers_config_by_name() {
        let m = server_repo_lookup(true, "myrepo", None).unwrap();
        assert_eq!(m, RepoMatch::ByName("myrepo".to_string()));
    }

    #[test]
    fn server_repo_lookup_falls_back_to_binding_by_id() {
        let b = crate::session_state::RepoBinding {
            repo_id: "rid-1".into(),
            git_url: None,
            remote_id: None,
            codebase_name: None,
            updated_at: "t".into(),
        };
        let m = server_repo_lookup(false, "ignored", Some(&b)).unwrap();
        assert_eq!(m, RepoMatch::ById("rid-1".to_string()));
    }

    #[test]
    fn server_repo_lookup_none_when_neither() {
        assert!(server_repo_lookup(false, "x", None).is_none());
    }

    #[test]
    fn effective_session_id_arg_wins_and_filters_empty() {
        assert_eq!(
            effective_session_id(Some("a".into()), Some("b".into())),
            Some("a".into())
        );
        assert_eq!(
            effective_session_id(Some("".into()), Some("b".into())),
            Some("b".into())
        );
        assert_eq!(
            effective_session_id(Some("   ".into()), None).as_deref(),
            Some("   ")
        ); // non-empty whitespace is a real (if odd) id — NOT filtered
        assert_eq!(effective_session_id(None, Some("".into())), None);
        assert_eq!(effective_session_id(None, None), None);
    }

    fn project_binding(id: &str, name: &str) -> crate::session_state::ProjectBinding {
        crate::session_state::ProjectBinding {
            project_id: id.into(),
            project_name: name.into(),
            updated_at: "t".into(),
            forced_until: None,
        }
    }

    /// `id` must be a real UUID string — `attribution_for` only accepts a
    /// repo binding whose `repo_id` parses as one (`binding_repo_id_is_valid`
    /// in stream.rs); a non-UUID id degrades to `ProjectOnly`/`None` instead.
    fn repo_binding_for_attribution(id: &str) -> crate::session_state::RepoBinding {
        crate::session_state::RepoBinding {
            repo_id: id.into(),
            git_url: None,
            remote_id: None,
            codebase_name: None,
            updated_at: "t".into(),
        }
    }

    #[test]
    fn recording_check_truth_table() {
        // Literal expected outcomes, NOT a restatement of `recording_check`'s
        // own condition (a computed `expect_error` formula could pass even if
        // the implementation's condition drifted, since both would drift
        // together).
        let repo_attr = crate::commands::stream::Attribution::Repo {
            repo_id: "11111111-1111-4111-8111-111111111111".into(),
            project: None,
        };
        let project_attr = crate::commands::stream::Attribution::ProjectOnly {
            project_id: uuid::Uuid::from_u128(1),
        };

        // session_id: None (no explicit session — a bare scan): never fires,
        // no matter what attribution is.
        assert!(recording_check(None, None).is_none());
        assert!(recording_check(None, Some(&repo_attr)).is_none());
        assert!(recording_check(None, Some(&project_attr)).is_none());

        // session_id: Some, something resolved (either shape): no check.
        assert!(recording_check(Some("s"), Some(&repo_attr)).is_none());
        assert!(recording_check(Some("s"), Some(&project_attr)).is_none());

        // session_id: Some, nothing resolved: the ONLY firing cell.
        assert_eq!(
            recording_check(Some("s"), None).map(|c| c.level),
            Some(Level::Error)
        );
    }

    #[test]
    fn recording_check_error_names_project_switch_and_session() {
        // Defect (2): the stale `repo switch <name>` hint has to be gone and
        // the repo-less remediation (`project switch --user`) has to be
        // present, or a regression to the old text wouldn't be caught by a
        // plain level assertion.
        let check = recording_check(Some("3f27a336-abcd"), None).expect("expected an Error check");
        assert_eq!(check.level, Level::Error);
        assert!(
            check.detail.contains("project switch"),
            "detail: {}",
            check.detail
        );
        assert!(
            check.detail.contains("3f27a336-abcd"),
            "detail: {}",
            check.detail
        );
    }

    #[test]
    fn project_binding_check_tiers_capture_project_honors_are_ok() {
        // Only the tiers `capture_project` itself resolves at capture time
        // read Ok: a `--project`-equivalent flag, the subagent/session-active
        // overrides, `TRACEVAULT_PROJECT` (`Env`, since A2 wired
        // `env_project_binding()` into `capture_project`'s inputs), and the
        // user-level default. `ConfigDefault` and `Deduced` are covered
        // separately — they must NOT appear here. Uses a well-formed UUID
        // id: an honored tier still needs a parseable `project_id` to read
        // Ok (see the malformed-id tests below).
        use crate::resolution::ProjectSource::*;
        for source in [ProjectFlag, Subagent, Env, SessionActive, UserDefault] {
            let outcome: ProjectOutcome = Ok(Some((
                project_binding("33333333-3333-4333-8333-333333333333", "My Project"),
                source,
            )));
            let check = project_binding_check(&outcome);
            assert_eq!(check.level, Level::Ok, "source={source} should read Ok");
            assert!(
                check.detail.contains("My Project"),
                "source={source} detail: {}",
                check.detail
            );
        }
    }

    #[test]
    fn project_binding_check_config_default_is_warn_not_ok() {
        // `capture_project` deliberately excludes the config `default_project`
        // tier, so displaying it as Ok would print a green Project line
        // directly above a red Recording line for the same session.
        let outcome: ProjectOutcome = Ok(Some((
            project_binding("pid-3", "My Project"),
            crate::resolution::ProjectSource::ConfigDefault,
        )));
        let check = project_binding_check(&outcome);
        assert_eq!(check.level, Level::Warn);
        assert!(
            check
                .detail
                .contains("not used for attribution at capture time"),
            "detail: {}",
            check.detail
        );
    }

    #[test]
    fn project_binding_check_deduced_is_warn() {
        // `capture_project` never deduces server-side, so this tier can't
        // read Ok either — same reasoning as `ConfigDefault` above.
        let outcome: ProjectOutcome = Ok(Some((
            project_binding("pid-2", ""),
            crate::resolution::ProjectSource::Deduced,
        )));
        let check = project_binding_check(&outcome);
        assert_eq!(check.level, Level::Warn);
        assert!(
            check
                .detail
                .contains("not used for attribution at capture time"),
            "detail: {}",
            check.detail
        );
    }

    #[test]
    fn project_binding_check_malformed_project_id_is_warn_not_ok() {
        // Aikido's finding: `capture_project` ends with
        // `.parse::<uuid::Uuid>().ok()?`, so a saved binding with a
        // non-UUID `project_id` (a hand-edited or corrupted
        // `user_project.toml`, or a corrupted session-state file) is
        // silently dropped at capture time. An honored tier with such an id
        // must not read Ok. Covers SessionActive and UserDefault.
        use crate::resolution::ProjectSource::{SessionActive, UserDefault};
        for source in [SessionActive, UserDefault] {
            let outcome: ProjectOutcome =
                Ok(Some((project_binding("not-a-uuid", "My Project"), source)));
            let check = project_binding_check(&outcome);
            assert_eq!(check.level, Level::Warn, "source={source}");
            assert!(
                check.detail.contains("not a valid id"),
                "source={source} detail: {}",
                check.detail
            );
        }
    }

    #[test]
    fn project_binding_check_ambiguous_err_is_warn_not_error() {
        // This axis is display-only — the verdict comes from
        // `recording_check`'s real gate, which never deduces — so ANY failure
        // of the deduction call (genuinely ambiguous, or a transport/5xx
        // error propagated the same way) must read as Warn, never Error: a
        // network blip on this axis must not be able to redden the exit code.
        let outcome: ProjectOutcome =
            Err("this repo belongs to multiple projects; select one with `tracevault project switch <name>`".into());
        let check = project_binding_check(&outcome);
        assert_eq!(check.level, Level::Warn);
        assert!(
            check.detail.contains("multiple projects"),
            "detail: {}",
            check.detail
        );
    }

    #[test]
    fn project_binding_check_none_is_skip() {
        let outcome: ProjectOutcome = Ok(None);
        let check = project_binding_check(&outcome);
        assert_eq!(check.level, Level::Skip);
    }

    // --- Verdict composition: every test below drives `recording_attribution`
    // directly (rather than re-deriving its session-load/worktree-key/bound-
    // config composition by hand), so a regression in that composition —
    // e.g. the worktree key coming from the wrong path — fails a test instead
    // of only a full end-to-end run (see `recording_attribution_finds_
    // subagent_override_via_linked_worktree_cwd` below).
    //
    // ISOLATION: `recording_attribution` -> `capture_project` always
    // consults `user_project_default::load()` once its local tiers miss, and
    // `recording_attribution` itself calls `user_default::load()`
    // unconditionally — both are backed by `dirs::config_dir()`/
    // `dirs::home_dir()`, which read the REAL developer config on the machine
    // running the test unless overridden. EVERY test below therefore
    // redirects both `XDG_CONFIG_HOME` (the path on Linux) and `HOME` (the
    // path `dirs` actually uses on macOS, and `home_dir()`'s own first
    // choice) to the same empty temp dir — even tests that don't care about
    // either default, since a developer machine that has ever run
    // `project switch --user` or `repo switch --user` would otherwise inject
    // a real, non-deterministic tier and flip the test's outcome. Three of
    // these tests call the public `save()` (not just `load()`), so getting
    // this isolation wrong doesn't just make a test flaky — it overwrites
    // that developer's real config file.

    // ---- resolve_auth: `.tracevault/config.toml` is the lowest credential
    // ---- rung in `resolve_credentials`, so the inspector must know it too.

    /// Isolate the env and the credentials file so nothing on the developer's
    /// machine can supply a credential these tests are asserting the absence of.
    fn auth_fixture() -> (
        tempfile::TempDir,
        tokio::sync::MutexGuard<'static, ()>,
        crate::test_helpers::EnvVarGuard,
    ) {
        let env_lock = crate::test_helpers::lock_env_mutation_sync();
        let dir = tempfile::tempdir().unwrap();
        let mut guard = crate::test_helpers::EnvVarGuard::new();
        guard.set("XDG_CONFIG_HOME", dir.path());
        guard.set("HOME", dir.path());
        guard.remove("TRACEVAULT_API_KEY");
        guard.remove("TRACEVAULT_SERVER_URL");
        (dir, env_lock, guard)
    }

    /// The reported bug: a repo authenticated purely through `config.toml`
    /// works for every command, but `status` called it "no credentials found"
    /// and exited non-zero.
    #[test]
    fn a_config_only_setup_reads_as_logged_in() {
        let (_dir, _lock, _guard) = auth_fixture();

        let auth = resolve_auth(Some("http://localhost:8080"), Some("tvk_from_config"));

        assert_eq!(auth.server_url.as_deref(), Some("http://localhost:8080"));
        assert!(
            matches!(auth.credential, Some(Credential::ApiKey(ref k)) if k == "tvk_from_config"),
            "the config's key is the credential every command resolves"
        );
        assert_eq!(auth.source, "project config (.tracevault/config.toml)");
    }

    /// `resolve_credentials` resolves the URL and the credential independently,
    /// so an env key with the URL pinned only in the config is a working setup
    /// — it used to report "token found but no server URL".
    #[test]
    fn an_env_key_takes_its_url_from_the_config_when_the_env_has_none() {
        let (_dir, _lock, mut guard) = auth_fixture();
        guard.set("TRACEVAULT_API_KEY", "tvk_from_env");

        let auth = resolve_auth(Some("http://localhost:8080"), None);

        assert_eq!(auth.server_url.as_deref(), Some("http://localhost:8080"));
        assert!(matches!(auth.credential, Some(Credential::ApiKey(ref k)) if k == "tvk_from_env"));
        assert_eq!(auth.source, "env (TRACEVAULT_API_KEY)");
    }

    /// The config is the LOWEST rung: a credentials file still outranks it, and
    /// its URL still wins, which is what the conflict warning then reports on.
    #[test]
    fn the_credentials_file_still_outranks_the_config() {
        let (dir, _lock, _guard) = auth_fixture();
        let creds_dir = dir.path().join("tracevault");
        std::fs::create_dir_all(&creds_dir).unwrap();
        std::fs::write(
            creds_dir.join("credentials.json"),
            r#"{"server_url":"https://dev.example.com","token":"tvk_from_file","email":"a@b.com"}"#,
        )
        .unwrap();

        let auth = resolve_auth(Some("http://localhost:8080"), Some("tvk_from_config"));

        assert_eq!(auth.server_url.as_deref(), Some("https://dev.example.com"));
        assert!(matches!(auth.credential, Some(Credential::ApiKey(ref k)) if k == "tvk_from_file"));
        assert_eq!(auth.source, "credentials file (API key)");
    }

    /// Nothing anywhere is still "none" — the fallback must not start claiming
    /// a credential just because a config exists without one.
    #[test]
    fn a_config_without_a_key_is_still_not_logged_in() {
        let (_dir, _lock, _guard) = auth_fixture();

        let auth = resolve_auth(Some("http://localhost:8080"), None);

        assert!(auth.credential.is_none());
        assert_eq!(auth.source, "none");
        assert_eq!(
            auth.server_url.as_deref(),
            Some("http://localhost:8080"),
            "the pin is still worth reporting"
        );
    }

    #[test]
    fn verdict_repo_user_default_alone_is_not_an_error() {
        // `repo switch --user` is `resolve_stream_binding`'s lowest tier.
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", cfg_tmp.path());
        _guard.set("HOME", cfg_tmp.path()); // see isolation note above

        crate::user_default::save(&repo_binding_for_attribution(
            "11111111-1111-4111-8111-111111111111",
        ))
        .unwrap();

        let cwd = tempfile::tempdir().unwrap();
        let (attribution, _worktree) = recording_attribution(None, None, cwd.path(), None);
        assert_eq!(
            attribution,
            Some(crate::commands::stream::Attribution::Repo {
                repo_id: "11111111-1111-4111-8111-111111111111".into(),
                project: None,
            })
        );
        assert!(recording_check(Some("s"), attribution.as_ref()).is_none());
    }

    #[test]
    fn verdict_project_user_default_alone_is_not_an_error() {
        // `project switch --user` with no credential configured: the display
        // half of this same fallback is pinned separately by
        // `offline_project_outcome_falls_back_to_saved_user_default`.
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", cfg_tmp.path());
        _guard.set("HOME", cfg_tmp.path()); // see isolation note above

        let pb = crate::session_state::ProjectBinding {
            project_id: uuid::Uuid::from_u128(7).to_string(),
            project_name: "p".into(),
            updated_at: "".into(),
            forced_until: None,
        };
        crate::user_project_default::save(&pb).unwrap();

        let cwd = tempfile::tempdir().unwrap();
        let (attribution, _worktree) = recording_attribution(None, None, cwd.path(), None);
        assert_eq!(
            attribution,
            Some(crate::commands::stream::Attribution::ProjectOnly {
                project_id: uuid::Uuid::from_u128(7)
            })
        );
        assert!(recording_check(Some("s"), attribution.as_ref()).is_none());
    }

    #[test]
    fn verdict_config_without_repo_id_and_nothing_else_is_error() {
        // A loaded `config.toml` with no `repo_id` must not count as bound —
        // `binding_from_config` returns `None` for it.
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", cfg_tmp.path());
        _guard.set("HOME", cfg_tmp.path()); // see isolation note above

        let config = crate::config::TracevaultConfig::default();
        assert!(
            crate::resolution::binding_from_config(&config).is_none(),
            "a repo_id-less config must not bind"
        );

        let cwd = tempfile::tempdir().unwrap();
        let (attribution, _worktree) = recording_attribution(None, None, cwd.path(), Some(&config));
        assert_eq!(
            recording_check(Some("s"), attribution.as_ref()).map(|c| c.level),
            Some(Level::Error)
        );
    }

    #[test]
    fn verdict_nothing_anywhere_is_error() {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", cfg_tmp.path());
        _guard.set("HOME", cfg_tmp.path()); // see isolation note above

        let cwd = tempfile::tempdir().unwrap();
        let (attribution, _worktree) = recording_attribution(None, None, cwd.path(), None);
        assert_eq!(
            recording_check(Some("s"), attribution.as_ref()).map(|c| c.level),
            Some(Level::Error)
        );
    }

    #[test]
    fn verdict_deduced_project_display_does_not_suppress_recording_error() {
        // A `Deduced` DISPLAY value must not make the project axis count as
        // bound for the verdict — `capture_project` never deduces.
        let outcome: ProjectOutcome = Ok(Some((
            project_binding("pid", ""),
            crate::resolution::ProjectSource::Deduced,
        )));
        assert_eq!(project_binding_check(&outcome).level, Level::Warn);

        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", cfg_tmp.path());
        _guard.set("HOME", cfg_tmp.path()); // see isolation note above

        let cwd = tempfile::tempdir().unwrap();
        let (attribution, _worktree) = recording_attribution(None, None, cwd.path(), None);
        assert_eq!(
            recording_check(Some("s"), attribution.as_ref()).map(|c| c.level),
            Some(Level::Error),
            "a Deduced display value must not suppress the Recording error"
        );
    }

    #[test]
    fn verdict_deduction_failure_is_warn_and_does_not_affect_recording() {
        // A deduction failure (network blip, 5xx, or genuinely Ambiguous)
        // must render at most Warn, and — since `recording_check` never even
        // sees `project_outcome` — cannot suppress or trigger the Recording
        // verdict either way.
        let outcome: ProjectOutcome = Err("this repo belongs to multiple projects".into());
        assert_eq!(project_binding_check(&outcome).level, Level::Warn);

        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", cfg_tmp.path());
        _guard.set("HOME", cfg_tmp.path()); // see isolation note above

        let config = crate::config::TracevaultConfig {
            repo_id: Some("22222222-2222-4222-8222-222222222222".into()),
            ..Default::default()
        };
        let cwd = tempfile::tempdir().unwrap();
        let (attribution, _worktree) = recording_attribution(None, None, cwd.path(), Some(&config));
        // Repo bound (via config) independently of the deduction failure
        // above -> no error, and nothing about the `Err` outcome forced one.
        assert!(recording_check(Some("s"), attribution.as_ref()).is_none());
    }

    #[test]
    fn offline_project_outcome_falls_back_to_saved_user_default() {
        // A saved `project switch --user` default must surface as Ok, not
        // Skip, in the credential-less path — not just the pure local tiers,
        // which `effective_project` alone covers.
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", cfg_tmp.path());
        _guard.set("HOME", cfg_tmp.path()); // see isolation note above

        let pb = crate::session_state::ProjectBinding {
            project_id: uuid::Uuid::from_u128(9).to_string(),
            project_name: "My Project".into(),
            updated_at: "".into(),
            forced_until: None,
        };
        crate::user_project_default::save(&pb).unwrap();

        let session = crate::session_state::SessionState::default();
        let inputs = ProjectResolveInputs {
            project_flag: None,
            env_project: None,
            session: &session,
            worktree_path: None,
            config_default: None,
        };
        // Same call `run_status`'s credential-less arm makes.
        let user_default_project = crate::user_project_default::load();
        let outcome = offline_project_outcome(&inputs, user_default_project);
        let check = project_binding_check(&outcome);
        assert_eq!(check.level, Level::Ok, "detail: {}", check.detail);
        assert!(
            check.detail.contains("My Project"),
            "detail: {}",
            check.detail
        );
    }

    /// VIS-305 Part A fix: `run_status`'s client-backed arm must resolve
    /// `TRACEVAULT_PROJECT`'s UUID form at the `Env` rung, and the resulting
    /// `Check` must read Ok (not the "not used for attribution at capture
    /// time" Warn) — `capture_project` genuinely honours this tier as of the
    /// A2 fix, so the diagnostic disagreeing with real behaviour would be
    /// exactly the bug this batch closes.
    #[tokio::test]
    async fn online_project_outcome_resolves_env_uuid_and_reads_ok() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        let uuid = "55555555-5555-4555-8555-555555555555";
        _guard.set("TRACEVAULT_PROJECT", uuid);

        // No `list_projects` mock: the UUID form must resolve without one.
        let client = ApiClient::new("http://127.0.0.1:0", Some("tok"));
        let session = crate::session_state::SessionState::default();
        let (outcome, unresolved) =
            online_project_outcome(&client, None, &session, "/wt", None, None).await;
        assert!(unresolved.is_none());
        let (b, source) = outcome
            .unwrap()
            .expect("TRACEVAULT_PROJECT must resolve to a binding");
        assert_eq!(source, ProjectSource::Env);
        assert_eq!(b.project_id, uuid);

        let check = project_binding_check(&Ok(Some((b, source))));
        assert_eq!(
            check.level,
            Level::Ok,
            "an Env-sourced binding is honoured by capture_project and must read Ok: {}",
            check.detail
        );
    }

    /// VIS-305 Part A fix, name form: with a client in scope,
    /// `TRACEVAULT_PROJECT` set to a NAME must resolve via `list_projects`,
    /// mirroring `commands::project`'s `status`.
    #[tokio::test]
    async fn online_project_outcome_resolves_env_name_via_the_client() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("TRACEVAULT_PROJECT", "payments");

        let list =
            r#"[{"id":"11111111-1111-4111-8111-111111111111","name":"payments"}]"#.to_string();
        let (base, _rx) =
            crate::test_helpers::spawn_seq(vec![crate::test_helpers::http_json("200 OK", &list)]);
        let client = ApiClient::new(&base, Some("tok"));
        let session = crate::session_state::SessionState::default();
        let (outcome, _unresolved) =
            online_project_outcome(&client, None, &session, "/wt", None, None).await;
        let (b, source) = outcome
            .unwrap()
            .expect("a name should resolve via list_projects");
        assert_eq!(source, ProjectSource::Env);
        assert_eq!(b.project_name, "payments");
        assert_eq!(b.project_id, "11111111-1111-4111-8111-111111111111");
    }

    #[test]
    fn recording_attribution_finds_subagent_override_via_linked_worktree_cwd() {
        // The subagent override is keyed by the LINKED worktree's own
        // toplevel, not the primary repo's — resolving from the wrong one
        // must miss it.
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", cfg_tmp.path());
        _guard.set("HOME", cfg_tmp.path()); // see isolation note above

        let base = tempfile::tempdir().unwrap();
        let repo_dir = base.path().join("repo");
        let wt_dir = base.path().join("wt");
        std::fs::create_dir_all(&repo_dir).unwrap();
        crate::test_helpers::init_git_repo(&repo_dir);
        crate::test_helpers::add_worktree(&repo_dir, &wt_dir);

        let wt_key = crate::paths::worktree_toplevel(&wt_dir);
        let project_id = uuid::Uuid::from_u128(42);
        let mut subagent_projects = std::collections::HashMap::new();
        subagent_projects.insert(
            wt_key,
            crate::session_state::ProjectBinding {
                project_id: project_id.to_string(),
                project_name: "linked".into(),
                updated_at: "t".into(),
                forced_until: None,
            },
        );
        let state = crate::session_state::SessionState {
            subagent_projects,
            ..Default::default()
        };
        let sessions_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            sessions_dir.path().join("sess-linked.toml"),
            toml::to_string(&state).unwrap(),
        )
        .unwrap();

        // From the LINKED worktree cwd, the subagent override resolves.
        let (found, _worktree) = recording_attribution(
            Some(sessions_dir.path()),
            Some("sess-linked"),
            &wt_dir,
            None,
        );
        assert_eq!(
            found,
            Some(crate::commands::stream::Attribution::ProjectOnly { project_id })
        );

        // From the PRIMARY worktree root instead, the key doesn't match, so
        // nothing resolves.
        let (missed, _worktree) = recording_attribution(
            Some(sessions_dir.path()),
            Some("sess-linked"),
            &repo_dir,
            None,
        );
        assert_eq!(missed, None);
    }

    #[test]
    fn verdict_malformed_project_id_leaves_project_axis_unbound() {
        // Pins that the display fix (`project_binding_check`) and the real
        // verdict (`recording_attribution` -> `capture_project`) cannot
        // diverge: a malformed `project_id` must be dropped by both.
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", cfg_tmp.path());
        _guard.set("HOME", cfg_tmp.path()); // see isolation note above

        let state = crate::session_state::SessionState {
            active_project: Some(project_binding("not-a-uuid", "My Project")),
            ..Default::default()
        };
        let sessions_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            sessions_dir.path().join("sess-bad-project.toml"),
            toml::to_string(&state).unwrap(),
        )
        .unwrap();

        let cwd = tempfile::tempdir().unwrap();
        let (attribution, _worktree) = recording_attribution(
            Some(sessions_dir.path()),
            Some("sess-bad-project"),
            cwd.path(),
            None,
        );
        assert_eq!(
            attribution, None,
            "a malformed project_id must not resolve an attribution"
        );
        assert_eq!(
            recording_check(Some("s"), attribution.as_ref()).map(|c| c.level),
            Some(Level::Error)
        );
    }

    #[test]
    fn verdict_malformed_repo_id_with_nothing_else_bound_is_error() {
        // Same principle, repo axis: `attribution_for` drops a binding whose
        // `repo_id` fails `binding_repo_id_is_valid`, so nothing else bound
        // must still error, matching `workspace_binding_*_malformed_repo_id_*`.
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", cfg_tmp.path());
        _guard.set("HOME", cfg_tmp.path()); // see isolation note above

        let state = crate::session_state::SessionState {
            active: Some(crate::session_state::RepoBinding {
                repo_id: "not-a-uuid".into(),
                git_url: None,
                remote_id: None,
                codebase_name: None,
                updated_at: "t".into(),
            }),
            ..Default::default()
        };
        let sessions_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            sessions_dir.path().join("sess-bad-repo.toml"),
            toml::to_string(&state).unwrap(),
        )
        .unwrap();

        let cwd = tempfile::tempdir().unwrap();
        let (attribution, _worktree) = recording_attribution(
            Some(sessions_dir.path()),
            Some("sess-bad-repo"),
            cwd.path(),
            None,
        );
        assert_eq!(
            attribution, None,
            "a malformed repo_id must not resolve an attribution"
        );
        assert_eq!(
            recording_check(Some("s"), attribution.as_ref()).map(|c| c.level),
            Some(Level::Error)
        );
    }

    #[test]
    fn unresolved_config_default_check_names_it_and_is_warn() {
        let check = unresolved_config_default_check("payments-platform");
        assert_eq!(check.level, Level::Warn);
        assert!(
            check.detail.contains("payments-platform"),
            "detail: {}",
            check.detail
        );
        assert!(
            check
                .detail
                .contains("not used for attribution at capture time"),
            "detail should not imply resolving it would make capture-time \
             attribution work: {}",
            check.detail
        );
    }
}
