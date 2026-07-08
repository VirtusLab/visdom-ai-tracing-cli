//! `tracevault status` — surface every piece of state a user might need to
//! debug "why doesn't it work". The command runs read-only checks across
//! credentials, the project tree, and the server, and prints a grouped
//! report with ✓ / ✗ / ! markers. Exits non-zero if anything actionable is
//! broken.

use crate::api_client::{ApiClient, GetMeError};
use crate::config::TracevaultConfig;
use crate::credentials::Credentials;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    token: Option<String>,
    source: &'static str, // "env", "credentials", or "none"
    email_from_creds: Option<String>,
}

fn resolve_auth() -> AuthContext {
    // Env var wins. Match the server-side resolution order in
    // resolve_credentials (env > creds), except we treat the env var as
    // authoritative without looking at the credentials file email.
    let env_key = std::env::var("TRACEVAULT_API_KEY").ok();
    let env_url = std::env::var("TRACEVAULT_SERVER_URL").ok();

    if let Some(token) = env_key {
        return AuthContext {
            server_url: env_url,
            token: Some(token),
            source: "env (TRACEVAULT_API_KEY)",
            email_from_creds: None,
        };
    }

    let creds = Credentials::load();
    if let Some(c) = creds {
        return AuthContext {
            server_url: Some(c.server_url),
            token: Some(c.token),
            source: "credentials file",
            email_from_creds: Some(c.email),
        };
    }

    AuthContext {
        server_url: env_url,
        token: None,
        source: "none",
        email_from_creds: None,
    }
}

async fn auth_checks(auth: &AuthContext) -> Vec<Check> {
    let mut out = Vec::new();

    match (auth.token.as_ref(), auth.server_url.as_ref()) {
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

    let server_url = auth.server_url.as_ref().unwrap();
    let token = auth.token.as_ref().unwrap();
    let client = ApiClient::new(server_url, Some(token));
    match client.get_me().await {
        Ok(me) => {
            let who = me.name.unwrap_or_else(|| me.email.clone());
            out.push(Check::ok("Token valid", format!("{who} <{}>", me.email)));

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
    global_settings: &Path,
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
            let slug = c.org_slug.as_deref().unwrap_or("<unset>");
            let url = c.server_url.as_deref().unwrap_or("<unset>");
            out.push(Check::ok(
                "Project config",
                format!("org={slug}, server={url}"),
            ));
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
fn claude_hook_check_in(repo_settings: &Path, global_settings: &Path) -> Check {
    let repo = read_settings_hooks(repo_settings);
    if matches!(repo, SettingsHooks::Present) {
        return Check::ok("Claude Code hooks", "registered in .claude/settings.json");
    }
    let global = read_settings_hooks(global_settings);
    if matches!(global, SettingsHooks::Present) {
        return Check::ok("Claude Code hooks", "via global install (see Installation)");
    }
    if let SettingsHooks::Unreadable(e) = &repo {
        return Check::warn(
            "Claude Code hooks",
            format!("cannot read <repo>/.claude/settings.json: {e}"),
        );
    }
    if let SettingsHooks::Unreadable(e) = &global {
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

/// Resolve the workspace binding for `status`. With an explicit session id the
/// user named a target, so a missing binding is a Warn. Without one, best-effort
/// scan across all sessions (state is global): show the most recent that HAS a
/// binding, and stay quiet (Skip) when none does — so a bound-mode user with a
/// stray empty session file isn't warned. Returns the Check AND the resolved
/// binding (reused by the caller as the `has_binding` mode signal).
fn workspace_binding_check_in(
    sessions_dir: &Path,
    explicit_session_id: Option<&str>,
) -> (Check, Option<crate::session_state::RepoBinding>) {
    match explicit_session_id {
        Some(id) => {
            let state = crate::session_state::load_from(sessions_dir, id);
            match state.active {
                Some(b) => (
                    Check::ok(
                        "Workspace binding",
                        format!(
                            "repo {} (org {}) via repo switch (session {id})",
                            b.repo_id, b.org_slug
                        ),
                    ),
                    Some(b),
                ),
                None => (
                    Check::warn(
                        "Workspace binding",
                        format!(
                            "session {id} has no active binding — run `tracevault repo switch …`"
                        ),
                    ),
                    None,
                ),
            }
        }
        None => match latest_active_binding_in(sessions_dir) {
            Some((id, b)) => (
                Check::ok(
                    "Workspace binding",
                    format!(
                        "repo {} (org {}) via repo switch — most recent session with a binding: {id} (may be another repo; pass --session-id to target one)",
                        b.repo_id, b.org_slug
                    ),
                ),
                Some(b),
            ),
            None => (
                Check::skip("Workspace binding", "no active workspace binding found"),
                None,
            ),
        },
    }
}

// --- Server repo ---

/// How `status` should locate the repo on the server.
#[derive(Debug, PartialEq)]
enum RepoMatch {
    ByName(String),
    ById(String),
}

/// Bound mode (config `org_slug`, match by git repo NAME) takes precedence;
/// otherwise workspace mode (the authoritative binding's `org_slug`, match by
/// `repo_id`). `None` when neither is available. Pure — `repo_name` is passed
/// in so this is unit-testable without invoking git.
fn server_repo_lookup(
    config_org_slug: Option<&str>,
    repo_name: &str,
    binding: Option<&crate::session_state::RepoBinding>,
) -> Option<(String, RepoMatch)> {
    if let Some(slug) = config_org_slug {
        return Some((slug.to_string(), RepoMatch::ByName(repo_name.to_string())));
    }
    if let Some(b) = binding {
        return Some((b.org_slug.clone(), RepoMatch::ById(b.repo_id.clone())));
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

    let (Some(token), Some(server_url)) = (auth.token.as_ref(), auth.server_url.as_ref()) else {
        out.push(Check::skip(
            "Repo registered on server",
            "not authenticated",
        ));
        return out;
    };

    let repo_name = git_repo_name(project_root);
    let Some((slug, want)) = server_repo_lookup(
        config.and_then(|c| c.org_slug.as_deref()),
        &repo_name,
        binding,
    ) else {
        out.push(Check::skip(
            "Repo registered on server",
            "no org (need a bound config.toml or a --session-id workspace binding)",
        ));
        return out;
    };

    let client = ApiClient::new(server_url, Some(token));
    let repos = match client.list_repos(&slug).await {
        Ok(r) => r,
        Err(e) => {
            out.push(Check::warn(
                "Repo registered on server",
                format!("failed to list repos: {e}"),
            ));
            return out;
        }
    };

    let found = repos.iter().find(|r| match &want {
        RepoMatch::ByName(name) => &r.name == name,
        RepoMatch::ById(id) => &r.id.to_string() == id,
    });

    match found {
        None => {
            let what = match &want {
                RepoMatch::ByName(n) => format!("'{n}'"),
                RepoMatch::ById(id) => format!("repo id {id}"),
            };
            out.push(Check::err(
                "Repo registered on server",
                format!("{what} not found in org '{slug}'. Run `tracevault init` while logged in, or `tracevault sync`."),
            ));
            return out;
        }
        Some(r) => {
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

            match &want {
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

fn git_repo_name(project_root: &Path) -> String {
    Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .as_deref()
        .and_then(|p| p.rsplit('/').next())
        .map(String::from)
        .unwrap_or_else(|| "unknown".into())
}

fn git_remote_url(project_root: &Path) -> Option<String> {
    Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

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

pub async fn run_status(project_root: &Path, session_id: Option<&str>) -> i32 {
    let auth = resolve_auth();

    // Production locations for the global install + workspace session state.
    let global_settings = dirs::home_dir()
        .map(|h| h.join(".claude").join("settings.json"))
        .unwrap_or_else(|| PathBuf::from(".claude/settings.json"));
    let sessions_dir = crate::session_state::sessions_dir();

    let global_check = global_hook_check_in(&global_settings);
    let has_global = global_check.level == Level::Ok;

    // Resolve the workspace binding ONCE: it drives both the section and the
    // `has_binding` mode signal used by project_checks.
    let (binding_check, binding) = match sessions_dir.as_deref() {
        Some(dir) => workspace_binding_check_in(dir, session_id),
        None => (
            Check::skip("Workspace binding", "cannot determine session state dir"),
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

    let auth_checks_v = auth_checks(&auth).await;
    let install_v = vec![global_check];
    let (proj_checks_v, config) =
        project_checks(project_root, &global_settings, has_global, has_binding);
    let binding_v = vec![binding_check];
    let server_checks_v =
        server_repo_checks(project_root, &auth, config.as_ref(), authoritative_binding).await;
    let session_checks_v = session_checks(project_root);

    let sections: Vec<(&str, Vec<Check>)> = vec![
        ("Authentication", auth_checks_v),
        ("Installation", install_v),
        ("Project", proj_checks_v),
        ("Workspace binding", binding_v),
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
        let (checks, cfg) = project_checks(dir.path(), &missing_global, false, false);
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
        let (checks, _) = project_checks(dir.path(), &missing_global, true, false);
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
        let (checks, cfg) = project_checks(dir.path(), &missing_global, false, false);
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
        let (checks, cfg) = project_checks(dir.path(), &missing_global, false, false);
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
        let c = claude_hook_check_in(&repo, &global);
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
        assert_eq!(claude_hook_check_in(&repo, &global).level, Level::Ok);
    }

    #[test]
    fn claude_hook_check_warn_when_neither() {
        let dir = tempfile::tempdir().unwrap();
        let c = claude_hook_check_in(&dir.path().join("a.json"), &dir.path().join("b.json"));
        assert_eq!(c.level, Level::Warn);
    }

    #[test]
    fn claude_hook_check_warns_when_repo_settings_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo-settings.json");
        std::fs::create_dir(&repo).unwrap(); // directory-as-file → non-NotFound read error
        let global = dir.path().join("global.json"); // missing
        let c = claude_hook_check_in(&repo, &global);
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
                org_slug: "acme".into(),
                repo_id: "11111111-1111-4111-8111-111111111111".into(),
                git_url: None,
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
        assert!(check.detail.contains("acme"), "detail: {}", check.detail);
        assert_eq!(
            binding.unwrap().repo_id,
            "11111111-1111-4111-8111-111111111111"
        );
    }

    #[test]
    fn workspace_binding_explicit_id_without_binding_is_warn() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("sess-empty.toml"),
            toml::to_string(&crate::session_state::SessionState::default()).unwrap(),
        )
        .unwrap();
        let (check, binding) = workspace_binding_check_in(dir.path(), Some("sess-empty"));
        assert_eq!(check.level, Level::Warn);
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
                org_slug: "o1".into(),
                repo_id: "aaaaaaaa-1111-4111-8111-111111111111".into(),
                git_url: None,
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
                org_slug: "o2".into(),
                repo_id: "bbbbbbbb-2222-4222-8222-222222222222".into(),
                git_url: None,
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
            binding.unwrap().org_slug,
            "o2",
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
                org_slug: "has-it".into(),
                repo_id: "aaaaaaaa-1111-4111-8111-111111111111".into(),
                git_url: None,
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
        assert_eq!(binding.unwrap().org_slug, "has-it");
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
    fn server_repo_lookup_prefers_config_by_name() {
        let m = server_repo_lookup(Some("acme"), "myrepo", None).unwrap();
        assert_eq!(
            m,
            ("acme".to_string(), RepoMatch::ByName("myrepo".to_string()))
        );
    }

    #[test]
    fn server_repo_lookup_falls_back_to_binding_by_id() {
        let b = crate::session_state::RepoBinding {
            org_slug: "visdom".into(),
            repo_id: "rid-1".into(),
            git_url: None,
            updated_at: "t".into(),
        };
        let m = server_repo_lookup(None, "ignored", Some(&b)).unwrap();
        assert_eq!(
            m,
            ("visdom".to_string(), RepoMatch::ById("rid-1".to_string()))
        );
    }

    #[test]
    fn server_repo_lookup_none_when_neither() {
        assert!(server_repo_lookup(None, "x", None).is_none());
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
}
