//! `tracevault status` — surface every piece of state a user might need to
//! debug "why doesn't it work". The command runs read-only checks across
//! credentials, the project tree, and the server, and prints a grouped
//! report with ✓ / ✗ / ! markers. Exits non-zero if anything actionable is
//! broken.

use crate::api_client::{ApiClient, GetMeError};
use crate::config::TracevaultConfig;
use crate::credentials::Credentials;
use std::fs;
use std::path::Path;
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
fn project_checks(project_root: &Path) -> (Vec<Check>, Option<TracevaultConfig>) {
    let mut out = Vec::new();

    let is_git = project_root.join(".git").exists();
    if is_git {
        out.push(Check::ok(
            "Git repository",
            project_root.display().to_string(),
        ));
    } else {
        out.push(Check::err(
            "Git repository",
            "current directory is not a git repo",
        ));
    }

    let tv_dir = project_root.join(".tracevault");
    if !tv_dir.exists() {
        out.push(Check::err(
            "TraceVault initialized",
            "no .tracevault/ directory. Run `tracevault init`.",
        ));
        return (out, None);
    }
    out.push(Check::ok("TraceVault initialized", ".tracevault/ present"));

    let config = TracevaultConfig::load(project_root);
    match &config {
        Some(c) => {
            let slug = c.org_slug.as_deref().unwrap_or("<unset>");
            let url = c.server_url.as_deref().unwrap_or("<unset>");
            out.push(Check::ok(
                "Project config",
                format!("org={slug}, server={url}"),
            ));
        }
        None => {
            out.push(Check::err(
                "Project config",
                ".tracevault/config.toml missing or unreadable",
            ));
        }
    }

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
    out.push(claude_hook_check(project_root));

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

fn claude_hook_check(project_root: &Path) -> Check {
    let settings = project_root.join(".claude/settings.json");
    if !settings.exists() {
        // Not installed is a warning, not an error: tracevault still works
        // without Claude Code hooks (just with less rich capture).
        return Check::warn(
            "Claude Code hooks",
            ".claude/settings.json missing (capture will miss some events)",
        );
    }
    match fs::read_to_string(&settings) {
        Ok(s) if s.contains("tracevault stream") => {
            Check::ok("Claude Code hooks", "registered in .claude/settings.json")
        }
        Ok(_) => Check::warn(
            "Claude Code hooks",
            "settings.json has no tracevault stream commands",
        ),
        Err(e) => Check::warn(
            "Claude Code hooks",
            format!("cannot read settings.json: {e}"),
        ),
    }
}

// --- Server repo ---

async fn server_repo_checks(
    project_root: &Path,
    auth: &AuthContext,
    config: Option<&TracevaultConfig>,
) -> Vec<Check> {
    let mut out = Vec::new();

    let (Some(token), Some(server_url)) = (auth.token.as_ref(), auth.server_url.as_ref()) else {
        out.push(Check::skip(
            "Repo registered on server",
            "not authenticated",
        ));
        return out;
    };
    let Some(slug) = config.and_then(|c| c.org_slug.as_deref()) else {
        out.push(Check::skip(
            "Repo registered on server",
            "no org_slug in config",
        ));
        return out;
    };

    let client = ApiClient::new(server_url, Some(token));
    let repos = match client.list_repos(slug).await {
        Ok(r) => r,
        Err(e) => {
            out.push(Check::warn(
                "Repo registered on server",
                format!("failed to list repos: {e}"),
            ));
            return out;
        }
    };

    let repo_name = git_repo_name(project_root);
    let found = repos.iter().find(|r| r.name == repo_name);

    match found {
        None => {
            out.push(Check::err(
                "Repo registered on server",
                format!("'{repo_name}' not found in org '{slug}'. Run `tracevault init` while logged in, or `tracevault sync`."),
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
    }

    out
}

// --- Sessions ---

/// True for the legacy `pending.jsonl` or a per-repo `pending-<id>.jsonl`
/// (nonempty id) queue file — tighter than a bare `starts_with("pending") &&
/// ends_with(".jsonl")` check, which would also match e.g. `pendingfoo.jsonl`
/// or `pending-.jsonl`.
fn is_pending_queue_filename(name: &str) -> bool {
    name == "pending.jsonl" || crate::commands::flush::repo_id_from_pending_filename(name).is_some()
}

/// Sum the non-empty lines across every per-repo (and legacy) pending queue
/// file in a session directory: `pending.jsonl` and `pending-<repo_id>.jsonl`.
fn count_pending_events(session_dir: &Path) -> usize {
    let Ok(read) = fs::read_dir(session_dir) else {
        return 0;
    };

    read.flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .map(is_pending_queue_filename)
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
            "no .tracevault/sessions/ yet (no captures recorded)",
        )];
    }

    let mut total_sessions = 0usize;
    let mut sessions_with_pending = 0usize;
    let mut pending_event_count = 0usize;

    if let Ok(read) = fs::read_dir(&sessions_dir) {
        for entry in read.flatten() {
            if !entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                continue;
            }
            total_sessions += 1;
            let count = count_pending_events(&entry.path());
            if count > 0 {
                sessions_with_pending += 1;
                pending_event_count += count;
            }
        }
    }

    vec![if sessions_with_pending == 0 {
        Check::ok(
            "Pending events",
            format!("{total_sessions} session(s), all synced"),
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

pub async fn run_status(project_root: &Path) -> i32 {
    let auth = resolve_auth();

    let auth_checks_v = auth_checks(&auth).await;
    let (proj_checks_v, config) = project_checks(project_root);
    let server_checks_v = server_repo_checks(project_root, &auth, config.as_ref()).await;
    let session_checks_v = session_checks(project_root);

    print_section("Authentication", &auth_checks_v);
    print_section("Project", &proj_checks_v);
    print_section("Server repo", &server_checks_v);
    print_section("Sessions", &session_checks_v);

    let all: Vec<&Check> = auth_checks_v
        .iter()
        .chain(proj_checks_v.iter())
        .chain(server_checks_v.iter())
        .chain(session_checks_v.iter())
        .collect();

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

    if errors > 0 {
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
    fn project_checks_errors_without_tracevault_dir() {
        let dir = tempfile::tempdir().unwrap();
        // Create .git so the git-repo check passes, isolate the assertion.
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let (checks, cfg) = project_checks(dir.path());
        assert!(cfg.is_none());
        assert!(checks
            .iter()
            .any(|c| c.level == Level::Error && c.label == "TraceVault initialized"));
    }

    #[test]
    fn count_pending_events_sums_across_per_repo_queues() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pending-a.jsonl"), "line1\nline2\n").unwrap();
        std::fs::write(dir.path().join("pending-b.jsonl"), "line1\n").unwrap();
        std::fs::write(dir.path().join("pending-c.jsonl"), "").unwrap();
        assert_eq!(count_pending_events(dir.path()), 3);
    }

    #[test]
    fn count_pending_events_ignores_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pending-a.jsonl"), "line1\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "line1\nline2\n").unwrap();
        assert_eq!(count_pending_events(dir.path()), 1);
    }

    #[test]
    fn count_pending_events_zero_for_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(count_pending_events(dir.path()), 0);
    }

    #[test]
    fn is_pending_queue_filename_matches_legacy_and_per_repo() {
        assert!(is_pending_queue_filename("pending.jsonl"));
        assert!(is_pending_queue_filename("pending-a.jsonl"));
    }

    #[test]
    fn is_pending_queue_filename_rejects_lookalikes() {
        assert!(!is_pending_queue_filename("pendingfoo.jsonl"));
        assert!(!is_pending_queue_filename("pending-.jsonl"));
    }

    #[test]
    fn count_pending_events_counts_legacy_pending_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pending.jsonl"), "line1\nline2\n").unwrap();
        assert_eq!(count_pending_events(dir.path()), 2);
    }

    #[test]
    fn count_pending_events_ignores_lookalike_filenames() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pendingfoo.jsonl"), "line1\n").unwrap();
        std::fs::write(dir.path().join("pending-.jsonl"), "line1\n").unwrap();
        assert_eq!(count_pending_events(dir.path()), 0);
    }
}
