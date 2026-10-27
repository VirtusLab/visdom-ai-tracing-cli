use crate::api_client::ApiClient;
use crate::config::{user_config_path_in, TracevaultConfig, UserContext};
use crate::context::Context;
use crate::resolution::{git_remote_url, git_repo_name, RegistrationPlan};
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

/// Which Claude Code settings file to install hooks into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ClaudeSettingsTarget {
    /// .claude/settings.json — typically committed/shared with the team.
    Shared,
    /// .claude/settings.local.json — personal, conventionally git-ignored.
    Local,
}

impl ClaudeSettingsTarget {
    pub fn filename(self) -> &'static str {
        match self {
            ClaudeSettingsTarget::Shared => "settings.json",
            ClaudeSettingsTarget::Local => "settings.local.json",
        }
    }

    pub fn gitignore_entry(self) -> &'static str {
        match self {
            ClaudeSettingsTarget::Shared => ".claude/settings.json",
            ClaudeSettingsTarget::Local => ".claude/settings.local.json",
        }
    }
}

/// Resolve which settings file to use. If the caller passed an explicit
/// choice, honor it. Otherwise prompt interactively when stdin is a TTY,
/// or fall back to Shared for non-interactive callers (CI, scripts, tests).
fn resolve_claude_target(
    explicit: Option<ClaudeSettingsTarget>,
) -> io::Result<ClaudeSettingsTarget> {
    if let Some(target) = explicit {
        return Ok(target);
    }
    if !io::stdin().is_terminal() {
        return Ok(ClaudeSettingsTarget::Shared);
    }

    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write!(
        stdout,
        "Install Claude Code hooks into [s]hared (.claude/settings.json) or [l]ocal (.claude/settings.local.json)? [s]: "
    )?;
    stdout.flush()?;

    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let answer = line.trim().to_lowercase();
    Ok(match answer.as_str() {
        "l" | "local" => ClaudeSettingsTarget::Local,
        _ => ClaudeSettingsTarget::Shared,
    })
}

/// If `dir` is inside a LINKED git worktree (not the primary checkout), return
/// the primary worktree root. Returns `None` for the primary checkout or a
/// non-git directory.
///
/// A linked worktree has `git --git-dir` (e.g. `<primary>/.git/worktrees/<name>`)
/// distinct from `git --git-common-dir` (`<primary>/.git`); in the primary
/// checkout the two are equal. The primary root is the parent of the common dir.
fn linked_worktree_primary(dir: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--git-dir", "--git-common-dir"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let mut lines = stdout.lines();
    let git_dir = lines.next()?.trim();
    let git_common_dir = lines.next()?.trim();

    // git may return paths relative to `dir`; join (absolute args replace the
    // base) then canonicalize so the comparison is robust.
    let git_dir = dir.join(git_dir).canonicalize().ok()?;
    let git_common_dir = dir.join(git_common_dir).canonicalize().ok()?;

    if git_dir == git_common_dir {
        None // primary checkout
    } else {
        git_common_dir.parent().map(Path::to_path_buf)
    }
}

/// Validate init flag combinations against the selected agent. `--claude-settings`
/// selects between `.claude/settings.json` and `settings.local.json`, so it only
/// applies to the Claude Code agent — `--agent codex` always writes
/// `.codex/hooks.json`. Returns a human-readable error for an incompatible combo
/// so the CLI rejects it instead of silently ignoring the flag.
pub fn validate_init_flags(
    agent: crate::agent::Agent,
    claude_settings_set: bool,
) -> Result<(), String> {
    if matches!(
        agent,
        crate::agent::Agent::Codex | crate::agent::Agent::Gsd | crate::agent::Agent::OpenCode
    ) && claude_settings_set
    {
        return Err("--claude-settings only applies to --agent claude-code".to_string());
    }
    Ok(())
}

pub async fn init_in_directory(
    project_root: &Path,
    server_url: Option<&str>,
    claude_settings: Option<ClaudeSettingsTarget>,
    no_gitignore: bool,
    user_context: UserContext,
    agent: crate::agent::Agent,
) -> Result<String, io::Error> {
    // Check for git repository
    if !project_root.join(".git").exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Not a git repository. Run 'git init' first.",
        ));
    }

    // Refuse to initialize from a linked worktree: TraceVault's `.tracevault/`,
    // git hooks, and Claude settings all belong in the primary checkout (which
    // every worktree shares). Initializing here would create a stray
    // `.tracevault/` in this worktree instead. (In a linked worktree `.git` is a
    // file, so the check above passes — this guard is what catches it.)
    if let Some(primary) = linked_worktree_primary(project_root) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "Run `tracevault init` from the primary checkout ({}), not a linked worktree. \
                 cd there and run it again — all worktrees share the primary's .tracevault/.",
                primary.display()
            ),
        ));
    }

    // Credentials are resolved before anything is written. Without one the
    // repo is never registered, no `repo_id` is persisted, and the hooks
    // installed below would drop every event silently — so `init` refuses
    // instead of leaving a setup that looks installed but records nothing
    // (VIS-1045). A credential/URL mismatch aborts `init` too: it would
    // otherwise register the repo against whichever instance the env var
    // names while holding another instance's session. Resolved once, here:
    // the config rewrite below drops a hand-written `api_key`.
    // `io::Error::other` because this function's error type is `io::Error`.
    let (resolved_url, resolved_credential) =
        crate::credentials::resolve_credentials_quiet(project_root)
            .map_err(|e| io::Error::other(e.to_string()))?;
    let effective_url = server_url.map(String::from).or(resolved_url);
    if resolved_credential.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            not_logged_in_message(effective_url.as_deref()),
        ));
    }

    // Resolve the hook-file gitignore entry per agent. Claude has a
    // shared/local settings choice; Codex has a single .codex/hooks.json.
    let (claude_target, hook_gitignore_entry): (Option<ClaudeSettingsTarget>, String) = match agent
    {
        crate::agent::Agent::ClaudeCode => {
            let target = resolve_claude_target(claude_settings)?;
            (Some(target), target.gitignore_entry().to_string())
        }
        crate::agent::Agent::Codex => (None, ".codex/hooks.json".to_string()),
        // GSD (pi) itself creates `.gsd/settings.json` in the repo when
        // `install_gsd_extension` shells out to `gsd install <dir> -l` (a
        // project-local package registration, analogous to Claude's
        // `.claude/settings.local.json`) — that's GSD's own local install
        // state, not something the user should commit.
        crate::agent::Agent::Gsd => (None, ".gsd/".to_string()),
        // The OpenCode plugin is installed project-locally as
        // `.opencode/plugins/tracevault.ts` — ignore the whole `.opencode/`
        // directory rather than just the plugin file, in case OpenCode
        // itself later writes other local state there.
        crate::agent::Agent::OpenCode => (None, ".opencode/".to_string()),
    };

    // Create .tracevault/ directory
    let config_dir = TracevaultConfig::config_dir(project_root);
    fs::create_dir_all(&config_dir)?;
    fs::create_dir_all(config_dir.join("sessions"))?;
    fs::create_dir_all(config_dir.join("cache"))?;
    // Self-contained ignore for runtime artifacts, so this `.tracevault/` is
    // safe even if the root .gitignore doesn't cover it.
    TracevaultConfig::ensure_gitignore(&config_dir)?;

    // Keep tracevault's files local — update root .gitignore (unless opted out)
    if !no_gitignore {
        update_root_gitignore(project_root, &hook_gitignore_entry)?;
    }

    // Register repo on server if authenticated, server URL known, and git remote available
    let remote_url = git_remote_url(project_root);
    if remote_url.is_none() {
        eprintln!("Warning: no git remote 'origin' configured. Skipping server registration.");
        eprintln!("Run 'git remote add origin <url>' then 'tracevault sync' to register.");
    }

    // The repo a previous `init` bound, read before the config is rewritten:
    // re-running `init` keeps it if its codebase still links it.
    let previous_repo_id = TracevaultConfig::load(project_root)
        .and_then(|c| c.repo_id)
        .and_then(|id| id.parse::<uuid::Uuid>().ok());

    // Write config (include server_url if available)
    let mut config = TracevaultConfig::default();
    if let Some(url) = server_url {
        config.server_url = Some(url.to_string());
    }
    config.user_context = Some(user_context);
    fs::write(
        TracevaultConfig::config_path(project_root),
        config.to_toml(),
    )?;

    // Install the selected agent's hooks.
    match agent {
        crate::agent::Agent::ClaudeCode => {
            // `claude_target` is `Some` for the Claude agent by construction above.
            let target =
                claude_target.expect("claude settings target is resolved for the Claude agent");
            install_claude_hooks(project_root, target)?;
        }
        crate::agent::Agent::Codex => {
            install_codex_hooks(project_root)?;
            println!("{}", codex_hook_trust_notice());
        }
        crate::agent::Agent::Gsd => {
            // GSD (pi) loads extensions from the user-global ~/.gsd/, not from
            // anything in the repo, so even the repo-local `init` path installs
            // there — the repo itself only needs the .tracevault/ binding + git
            // hooks set up above, which the extension shells out to `tracevault`
            // to use. `Some(project_root)` makes `install_gsd_extension` run
            // `gsd install -l` from this repo, registering the extension as a
            // project-local install.
            install_gsd_extension(Some(project_root))?;
        }
        crate::agent::Agent::OpenCode => {
            install_opencode_plugin(Some(project_root))?;
        }
    }

    // Install git hooks
    install_git_hook(project_root)?;
    install_post_commit_hook(project_root)?;

    // Detect AI tools in the project
    let detected = crate::hooks::detect_tools(project_root);
    for tool in &detected {
        println!("  Detected: {}", tool.name());
    }

    if let (Some(url), Some(remote)) = (effective_url, remote_url) {
        let client = ApiClient::with_credential(&url, resolved_credential);
        let repo_name = git_repo_name(project_root);
        // Captured before `remote` is moved into the request below, so the
        // codebase-resolve step doesn't need to shell out to git again.
        let origin_url = remote.clone();

        // A remote whose codebase already has a repo record binds to it:
        // registering would upsert by THIS checkout's directory name, i.e. a
        // second record for the same codebase whenever the names differ.
        match crate::resolution::plan_registration(
            &client,
            &origin_url,
            &repo_name,
            previous_repo_id,
        )
        .await
        {
            Ok(RegistrationPlan::Existing(codebase, repo_id)) => {
                println!(
                    "Bound to existing repo {} (id: {repo_id}); not registering another record \
                     for this codebase",
                    codebase
                        .repos
                        .iter()
                        .find(|r| r.id == repo_id)
                        .map_or("", |r| r.name.as_str())
                );
                println!(
                    "{}",
                    crate::resolution::codebase_line(
                        codebase.remote.name.as_deref(),
                        &codebase.remote.normalized_url,
                        &codebase.remote.clone_status
                    )
                );
                persist_repo_binding(project_root, repo_id, Some(&codebase.remote));
            }
            Ok(RegistrationPlan::Ambiguous(codebase)) => {
                eprintln!(
                    "Warning: codebase {} already has {} repo records ({}) and none is named \
                     '{repo_name}'; not registering another. Set `repo_id = \"<id>\"` in \
                     .tracevault/config.toml to the one this checkout belongs to.",
                    codebase.display_name(),
                    codebase.repos.len(),
                    codebase.repo_list()
                );
            }
            Ok(RegistrationPlan::Register) => {
                register_new_repo(&client, project_root, repo_name, origin_url).await;
            }
            // Without knowing what the server already has, registering could
            // create the duplicate this check exists to prevent.
            Err(e) => {
                eprintln!(
                    "Warning: could not check the server for an existing repo of this remote \
                     ({e}); not registering. Run `tracevault init` again once the server is \
                     reachable."
                );
            }
        }
    }

    Ok(hook_gitignore_entry)
}

/// The error `init` fails with when no credential resolves: what went wrong,
/// that nothing was installed, and the exact commands that fix it. `url` is
/// the server URL already known (`--server-url`, `TRACEVAULT_SERVER_URL` or
/// the project config), filled into the `login` command when present.
pub fn not_logged_in_message(url: Option<&str>) -> String {
    let url = url.unwrap_or("<URL>");
    format!(
        "Not logged in. Without a login TraceVault cannot register this repo, and its hooks \
         would record nothing, so nothing was installed.\n\
         Log in, then run init again:\n\
         \n    tracevault login --server-url {url}\n    tracevault init\n\n\
         (In CI, set TRACEVAULT_API_KEY instead of logging in.)"
    )
}

/// Record the repo this checkout binds — and its codebase, display-only — in
/// an existing `.tracevault/config.toml`, returning whether it was written.
///
/// Never creates the config: a checkout without one is in workspace mode (a
/// global install binds sessions with `repo switch`), and writing one would
/// silently pin it to bound mode. A failed write never fails the caller.
pub(crate) fn persist_repo_binding(
    project_root: &Path,
    repo_id: uuid::Uuid,
    codebase: Option<&crate::api_client::ResolveRemoteResponse>,
) -> bool {
    let Some(mut cfg) = TracevaultConfig::load(project_root) else {
        return false;
    };
    cfg.repo_id = Some(repo_id.to_string());
    if let Some(remote) = codebase {
        cfg.remote_id = Some(remote.remote_id.to_string());
        cfg.codebase_name = remote.name.clone();
    }
    fs::write(TracevaultConfig::config_path(project_root), cfg.to_toml()).is_ok()
}

/// Register a repo for a remote the server has no live repo for, and persist
/// its id (plus the codebase, display-only) to config.toml. Failure is a
/// warning, never a failed `init`.
async fn register_new_repo(
    client: &ApiClient,
    project_root: &Path,
    repo_name: String,
    origin_url: String,
) {
    match client
        .register_repo(crate::api_client::RegisterRepoRequest {
            repo_name,
            github_url: Some(origin_url.clone()),
        })
        .await
    {
        Ok(resp) => {
            println!("Repo registered on server (id: {})", resp.repo_id);
            // Save repo_id to config
            if let Some(mut cfg) = TracevaultConfig::load(project_root) {
                cfg.repo_id = Some(resp.repo_id.to_string());
                // Best-effort: resolve the codebase (deduped by normalized
                // remote URL) and surface it to the user. Must never fail
                // init — `repo_id` above stays authoritative for ingest;
                // `remote_id`/`codebase_name` here are recorded for
                // display only.
                if let Ok(Some(remote)) = client.resolve_remote(&origin_url).await {
                    println!(
                        "{}",
                        crate::resolution::codebase_line(
                            remote.name.as_deref(),
                            &remote.normalized_url,
                            &remote.clone_status
                        )
                    );
                    cfg.remote_id = Some(remote.remote_id.to_string());
                    cfg.codebase_name = remote.name.clone();
                }
                let _ = fs::write(TracevaultConfig::config_path(project_root), cfg.to_toml());
            }
        }
        Err(e) => {
            eprintln!("{}", registration_failure_message(e.as_ref()));
        }
    }
}

/// The warning line for a failed repo registration during `init`. A 403 means
/// the token is valid but not authorized to register this repo — distinct
/// from a network/server fault, so it gets its own actionable wording; a 404
/// means this server build doesn't recognize the registration route at all,
/// which most plausibly means the CLI is newer than the server (a CLI/server
/// version mismatch). Registration failure is non-fatal to `init` either
/// way (a warning, not an aborted init — the repo can be registered later
/// via `tracevault sync`).
///
/// Matches on the exact status-prefix shape `ApiClient::register_repo`
/// produces (`"Server returned {status} ..."`, from its `authed_send_json`
/// call) rather than a bare `.contains("403")`/`.contains("404")` — a
/// response body that happens to contain that digit string (e.g. a proxy
/// error page) must not be misclassified.
fn registration_failure_message(err: &dyn std::error::Error) -> String {
    let msg = err.to_string();
    if msg.starts_with("Server returned 403 ") {
        "Warning: you are not authorized to register this repo.".to_string()
    } else if msg.starts_with("Server returned 404 ") {
        "Warning: could not register repo on server (404) — this endpoint may not exist on \
         this server yet, possibly a CLI/server version mismatch. Run `tracevault sync` again \
         once the server is upgraded."
            .to_string()
    } else {
        format!("Warning: could not register repo on server: {err}")
    }
}

fn update_root_gitignore(project_root: &Path, settings_entry: &str) -> Result<(), io::Error> {
    let path = project_root.join(".gitignore");
    let existing = if path.exists() {
        fs::read_to_string(&path)?
    } else {
        String::new()
    };

    // Only ignore what `init` actually creates or modifies: the `.tracevault/`
    // directory and the single settings file we wrote hooks into. The
    // other settings file is left untouched, so we don't add it here. An
    // empty `settings_entry` means the agent writes nothing into the repo at
    // all — skip it rather than appending a blank line.
    let needed: Vec<&str> = [".tracevault/", settings_entry]
        .iter()
        .copied()
        .filter(|entry| !entry.is_empty() && !existing.lines().any(|line| line.trim() == *entry))
        .collect();

    if needed.is_empty() {
        return Ok(());
    }

    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str("\n# TraceVault — local only, do not commit\n");
    for entry in needed {
        content.push_str(entry);
        content.push('\n');
    }

    fs::write(path, content)
}

const HOOK_MARKER: &str = "# tracevault:enforce";
const OLD_HOOK_MARKER: &str = "# tracevault:auto-push";

fn install_git_hook(project_root: &Path) -> Result<(), io::Error> {
    let hooks_dir = project_root.join(".git/hooks");
    fs::create_dir_all(&hooks_dir)?;

    let hook_path = hooks_dir.join("pre-push");
    // Capture git's pre-push ref list once, then feed it to `check` explicitly
    // rather than relying on it inheriting stdin from the hook process. `sync`
    // never reads stdin, so today it reaches `check` anyway — this only drops
    // the dependency on that inheritance for hooks installed from here on;
    // already-installed hooks keep working without re-running `tracevault
    // init`.
    let tracevault_block = format!(
        "{HOOK_MARKER}\n\
         # Capture git's pre-push ref list once; `check` uses it to determine\n\
         # which files are being pushed.\n\
         tracevault_prepush_stdin=$(cat)\n\
         tracevault sync 2>/dev/null || true\n\
         printf '%s' \"$tracevault_prepush_stdin\" | tracevault check || {{ echo \"tracevault: policy check failed\"; exit 1; }}\n"
    );

    if hook_path.exists() {
        let existing = fs::read_to_string(&hook_path)?;

        // Already has new-style hook
        if existing.contains(HOOK_MARKER) {
            return Ok(());
        }

        // Replace old-style hook block if present
        if existing.contains(OLD_HOOK_MARKER) {
            let mut new_content = String::new();
            let mut skip = false;
            for line in existing.lines() {
                if line.contains(OLD_HOOK_MARKER) {
                    skip = true;
                    continue;
                }
                if skip {
                    // Skip old tracevault lines (they start with "tracevault " or are empty continuations)
                    if line.starts_with("tracevault ") {
                        continue;
                    }
                    skip = false;
                }
                new_content.push_str(line);
                new_content.push('\n');
            }
            if !new_content.ends_with('\n') {
                new_content.push('\n');
            }
            new_content.push_str(&tracevault_block);
            fs::write(&hook_path, new_content)?;
        } else {
            // Append to existing hook
            let mut content = existing;
            if !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&tracevault_block);
            fs::write(&hook_path, content)?;
        }
    } else {
        let content = format!("#!/bin/sh\n{tracevault_block}");
        fs::write(&hook_path, content)?;
    }

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&hook_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&hook_path, perms)?;
    }

    Ok(())
}

const POST_COMMIT_MARKER: &str = "# tracevault:post-commit";

fn install_post_commit_hook(project_root: &Path) -> Result<(), io::Error> {
    let hooks_dir = project_root.join(".git/hooks");
    fs::create_dir_all(&hooks_dir)?;

    let hook_path = hooks_dir.join("post-commit");
    let tracevault_block = format!("{POST_COMMIT_MARKER}\ntracevault commit-push 2>/dev/null &\n");

    if hook_path.exists() {
        let existing = fs::read_to_string(&hook_path)?;

        if existing.contains(POST_COMMIT_MARKER) {
            return Ok(());
        }

        let mut content = existing;
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&tracevault_block);
        fs::write(&hook_path, content)?;
    } else {
        let content = format!("#!/bin/sh\n{tracevault_block}");
        fs::write(&hook_path, content)?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&hook_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&hook_path, perms)?;
    }

    Ok(())
}

fn install_claude_hooks(
    project_root: &Path,
    target: ClaudeSettingsTarget,
) -> Result<(), io::Error> {
    let claude_dir = project_root.join(".claude");
    fs::create_dir_all(&claude_dir)?;
    merge_hooks_into_file(&claude_dir.join(target.filename()), &tracevault_hooks())
}

pub fn tracevault_hooks() -> serde_json::Value {
    serde_json::json!({
        "PreToolUse": [{
            "matcher": "Write|Edit|Bash",
            "hooks": [{
                "type": "command",
                "command": "tracevault stream --event pre-tool-use",
                "timeout": 10,
                "statusMessage": "TraceVault: streaming pre-tool event"
            }]
        }],
        "PostToolUse": [{
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": "tracevault stream --event post-tool-use",
                "timeout": 10,
                "statusMessage": "TraceVault: streaming post-tool event"
            }]
        }],
        "Notification": [{
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": "tracevault stream --event notification",
                "timeout": 10,
                "statusMessage": "TraceVault: streaming notification"
            }]
        }],
        "Stop": [{
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": "tracevault stream --event stop",
                "timeout": 10,
                "statusMessage": "TraceVault: finalizing session"
            }]
        }],
        "SessionStart": [{
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": "tracevault session-start",
                "timeout": 10,
                "statusMessage": "TraceVault: session start"
            }]
        }],
        "UserPromptSubmit": [{
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": "tracevault user-prompt",
                "timeout": 10,
                "statusMessage": "TraceVault: policy reinforcement"
            }]
        }]
    })
}

/// Codex CLI hook set, mirroring `tracevault_hooks()` but with Codex commands.
/// What to tell the user after Codex hooks are installed.
///
/// Writing `hooks.json` is only half the setup: Codex >= 0.153 gates hooks
/// behind persisted *hook trust*, and a non-interactive `codex exec` skips
/// untrusted hooks without printing anything — so capture looks installed and
/// silently produces no sessions until trust is granted once. Observed on
/// Codex 0.153.4, where neither `codex doctor` nor `codex features` reports
/// the trust state, which is why the CLI has to say it here.
pub fn codex_hook_trust_notice() -> &'static str {
    "Codex gates hooks behind hook trust: until it is granted, `codex exec` skips them \
     silently and no session is captured. Run `codex` once interactively in a repo and \
     approve the hook-trust prompt; unattended runs (CI) can pass \
     `--dangerously-bypass-hook-trust` instead."
}

/// Capture hooks carry `--agent codex`; the injection hooks (session-start /
/// user-prompt) are agent-agnostic. Matchers are `""` (all tools).
///
/// We intentionally do NOT register a `PreToolUse` hook for Codex. Unlike Claude
/// Code — whose `PreToolUse` is scoped to `Write|Edit|Bash` — the CLI does no
/// Codex-specific pre-tool work, and `PostToolUse` + `Stop` already stream the
/// (incremental, offset-tracked) rollout. A pre-tool hook on every tool call
/// would only double the per-call streaming for no added capture. No
/// `Notification` event either (Codex has none).
pub fn codex_hooks() -> serde_json::Value {
    serde_json::json!({
        "SessionStart": [{
            "matcher": "",
            "hooks": [{ "type": "command", "command": "tracevault session-start", "timeout": 10, "statusMessage": "TraceVault: session start" }]
        }],
        "UserPromptSubmit": [{
            "matcher": "",
            "hooks": [{ "type": "command", "command": "tracevault user-prompt", "timeout": 10, "statusMessage": "TraceVault: policy reinforcement" }]
        }],
        "PostToolUse": [{
            "matcher": "",
            "hooks": [{ "type": "command", "command": "tracevault stream --event post-tool-use --agent codex", "timeout": 10, "statusMessage": "TraceVault: streaming post-tool event" }]
        }],
        "Stop": [{
            "matcher": "",
            "hooks": [{ "type": "command", "command": "tracevault stream --event stop --agent codex", "timeout": 10, "statusMessage": "TraceVault: finalizing session" }]
        }]
    })
}

/// Embedded pi/GSD extension source (bundled at build time — see
/// `assets/gsd-extension/{index.ts,package.json,extension-manifest.json}`,
/// the checked-in source this installer writes verbatim).
const GSD_EXT_INDEX_TS: &str = include_str!("../../assets/gsd-extension/index.ts");
const GSD_EXT_PACKAGE_JSON: &str = include_str!("../../assets/gsd-extension/package.json");
const GSD_EXT_MANIFEST_JSON: &str =
    include_str!("../../assets/gsd-extension/extension-manifest.json");

/// Path of the stable directory (under the user's config dir) that holds the
/// TraceVault GSD extension source, shared by `install_gsd_extension` so the
/// write and the `gsd install` invocation always agree on where it lives.
fn gsd_extension_source_dir() -> io::Result<PathBuf> {
    let config_dir = dirs::config_dir().or_else(|| dirs::home_dir().map(|h| h.join(".config")));
    let config_dir = config_dir
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no config or home dir"))?;
    Ok(config_dir.join("tracevault").join("gsd-extension"))
}

/// Install the TraceVault pi/GSD extension and register it with GSD so it
/// actually loads.
///
/// GSD (pi) only loads an extension that (a) has an `extension-manifest.json`
/// in its directory *and* (b) was registered via `gsd install <dir>` — a
/// hand-written `registry.json` entry is inert (`gsd list` won't show it).
/// So this writes the extension source (`index.ts`, `package.json`,
/// `extension-manifest.json`) to a stable, idempotent source directory
/// (`<config_dir>/tracevault/gsd-extension`), best-effort cleans up any
/// stale/inert registration left behind by an older version of this
/// installer, and then shells out to `gsd install` to register + load it.
///
/// `project_root` selects how `gsd install` is invoked:
/// - `Some(repo)`: project-local install (`gsd install <dir> -l`, run with
///   `repo` as the working directory) — used by `tracevault init`.
/// - `None`: global install (`gsd install <dir>`, applies to all GSD
///   sessions on this machine) — used by `tracevault init --global`.
///
/// If the `gsd` binary can't be found or the install command fails, this
/// does NOT fail `init`: it prints a warning explaining how to finish the
/// registration by hand and returns `Ok(())`. The extension source is still
/// written either way, so the user (or a later retry) has something to
/// register.
pub fn install_gsd_extension(project_root: Option<&Path>) -> io::Result<()> {
    let home =
        dirs::home_dir().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home dir"))?;
    let gsd_home = home.join(".gsd");

    let source_dir = gsd_extension_source_dir()?;
    fs::create_dir_all(&source_dir)?;
    fs::write(source_dir.join("index.ts"), GSD_EXT_INDEX_TS)?;
    fs::write(source_dir.join("package.json"), GSD_EXT_PACKAGE_JSON)?;
    fs::write(
        source_dir.join("extension-manifest.json"),
        GSD_EXT_MANIFEST_JSON,
    )?;

    // Best-effort cleanup of the OLD, broken install (hand-written registry
    // entries that GSD never actually loaded). Never blocks the rest of the
    // install — see `cleanup_stale_gsd_registration`.
    if let Err(e) = cleanup_stale_gsd_registration(&gsd_home) {
        eprintln!("Warning: could not clean up the previous GSD extension registration: {e}");
    }

    // Register + load the extension via the `gsd` CLI. `gsd install` is what
    // actually makes GSD load the extension; our own registry.json edits
    // never did.
    let local_flag = project_root.is_some();
    let mut cmd = std::process::Command::new("gsd");
    cmd.arg("install").arg(&source_dir);
    if let Some(root) = project_root {
        cmd.arg("-l");
        cmd.current_dir(root);
    }

    let retry_hint = format!(
        "gsd install {}{}",
        source_dir.display(),
        if local_flag { " -l" } else { "" }
    );

    match cmd.output() {
        Ok(output) if output.status.success() => {
            println!(
                "TraceVault: installed and registered the GSD extension ({}).",
                source_dir.display()
            );
        }
        Ok(output) => {
            eprintln!(
                "TraceVault: installed the GSD extension source to {}, but `gsd install` failed:\n{}Ensure `gsd` is on PATH and run: `{retry_hint}` to enable capture.",
                source_dir.display(),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        Err(e) => {
            eprintln!(
                "TraceVault: installed the GSD extension source to {}, but could not run `gsd install` ({e}). Ensure `gsd` is on PATH and run: `{retry_hint}` to enable capture.",
                source_dir.display(),
            );
        }
    }

    Ok(())
}

/// TraceVault OpenCode plugin source, embedded from the checked-in assets
/// this installer writes verbatim (mirrors the GSD extension embeds above).
const OPENCODE_PLUGIN_INDEX: &str = include_str!("../../assets/opencode-plugin/index.ts");

/// Install the TraceVault OpenCode plugin. `project_root = Some(dir)` installs
/// project-local at `<dir>/.opencode/plugins/tracevault.ts`; `None` installs
/// global at `~/.config/opencode/plugins/tracevault.ts` (using the same
/// config-dir resolution as `gsd_extension_source_dir` above). OpenCode
/// auto-loads `*.ts` modules from that directory — no separate "install"
/// command. Returns an error on failure (unresolvable config dir, or a write
/// error) so the caller aborts init loudly, consistent with the Codex/GSD arms
/// — a swallowed failure would make `init` report success while no plugin was
/// written, silently disabling capture.
///
/// A FLAT `.ts` file is used rather than a `tracevault/` package directory: a
/// package subdir (index.ts + package.json) makes OpenCode try to resolve it as
/// an npm package and stalls at startup offline, whereas a flat module loads
/// directly (both confirmed during the capture spike). The plugin only imports
/// the `node:child_process` builtin, so it needs no package manifest.
pub fn install_opencode_plugin(project_root: Option<&Path>) -> io::Result<()> {
    let plugins_dir = match project_root {
        Some(root) => root.join(".opencode").join("plugins"),
        None => {
            let config_dir =
                dirs::config_dir().or_else(|| dirs::home_dir().map(|h| h.join(".config")));
            config_dir
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "could not determine the config directory for the OpenCode plugin install",
                    )
                })?
                .join("opencode")
                .join("plugins")
        }
    };

    write_opencode_plugin_files(&plugins_dir)
}

fn write_opencode_plugin_files(plugins_dir: &Path) -> io::Result<()> {
    fs::create_dir_all(plugins_dir)?;
    fs::write(plugins_dir.join("tracevault.ts"), OPENCODE_PLUGIN_INDEX)?;
    Ok(())
}

/// Best-effort cleanup of the extension registration left behind by an OLDER
/// version of this installer: the `tracevault-tracker` directory (an earlier,
/// incorrect Codex-clone `hooks.json` shim that impersonated this extension
/// under the wrong name) and any registry entries for `tracevault-tracker` or
/// `tracevault` — both are inert now that `gsd install` manages registration.
///
/// If `registry.json` is missing, there's nothing to clean up. If it exists
/// but fails to parse, or its shape is not a valid registry (root not an
/// object, or a non-object `entries` field), it is structurally corrupt: we
/// leave it byte-for-byte untouched (skip cleanup) rather than risk
/// destroying every other extension the user has registered.
pub fn cleanup_stale_gsd_registration(gsd_home: &Path) -> io::Result<()> {
    let ext_root = gsd_home.join("extensions");
    let reg_path = ext_root.join("registry.json");

    // Validate the registry BEFORE mutating anything under ~/.gsd. If it exists
    // but is structurally corrupt (unparseable, or a non-object root/`entries`),
    // leave the extensions directory entirely untouched — not even the stale
    // tracker dir is removed — so a corrupt registry yields zero mutation.
    let reg: Option<serde_json::Value> = if reg_path.exists() {
        let content = fs::read_to_string(&reg_path)?;
        let parsed: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => return Ok(()), // corrupt: touch nothing
        };
        let corrupt = match parsed.as_object() {
            None => true,
            Some(obj) => matches!(obj.get("entries"), Some(v) if !v.is_object()),
        };
        if corrupt {
            return Ok(()); // structurally corrupt: touch nothing
        }
        Some(parsed)
    } else {
        None
    };

    // Registry is valid or absent — now safe to remove the superseded tracker shim.
    let stale = ext_root.join("tracevault-tracker");
    if stale.exists() {
        fs::remove_dir_all(&stale)?;
    }

    // Drop stale registry entries (only when a valid registry exists).
    if let Some(mut reg) = reg {
        if let Some(entries) = reg.get_mut("entries").and_then(|e| e.as_object_mut()) {
            let removed_tracker = entries.remove("tracevault-tracker").is_some();
            let removed_inert = entries.remove("tracevault").is_some();
            if removed_tracker || removed_inert {
                // Atomic write (temp file + rename), pretty-printed.
                let tmp = reg_path.with_extension("json.tmp");
                fs::write(&tmp, serde_json::to_string_pretty(&reg)?)?;
                fs::rename(&tmp, &reg_path)?;
            }
        }
    }
    Ok(())
}

/// Read `hooks_path` as a JSON object (or `{}` if absent), bring its TraceVault
/// hooks to `ours` via [`merge_hooks`] (foreign hooks and other keys are kept),
/// and write the result back atomically (sibling temp file + rename). The one
/// install path for every hook file — per-repo Claude (`settings.json` or
/// `settings.local.json`), global Claude and Codex (`hooks.json`), whose roots
/// are the object `merge_hooks` mutates (`root["hooks"]`). A file that is
/// already current is not rewritten, so its formatting survives a re-run. On a
/// merge error (malformed existing file) it returns before writing, so the
/// target is never clobbered and no temp file is left behind.
fn merge_hooks_into_file(hooks_path: &Path, ours: &serde_json::Value) -> io::Result<()> {
    // Defense-in-depth against path traversal: callers pass a constant filename
    // joined to a home/cwd base (global callers additionally go through
    // `safe_join`), so a `..` component should never appear — reject it outright
    // rather than read/write through it.
    if hooks_path
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to use path with '..': {}", hooks_path.display()),
        ));
    }
    // A settings file symlinked to a centrally managed config is updated at its
    // target, so the link survives (renaming over it would replace it with a
    // regular file). A dangling link fails here instead of being overwritten.
    let target = match fs::symlink_metadata(hooks_path) {
        Ok(meta) if meta.file_type().is_symlink() => fs::canonicalize(hooks_path)?,
        _ => hooks_path.to_path_buf(),
    };
    let existing_permissions = match fs::metadata(&target) {
        Ok(meta) => Some(meta.permissions()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let mut root: serde_json::Value = if existing_permissions.is_some() {
        let content = fs::read_to_string(&target)?;
        serde_json::from_str(&content).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Failed to parse {}: {e}", hooks_path.display()),
            )
        })?
    } else {
        serde_json::json!({})
    };

    let before = existing_permissions.is_some().then(|| root.to_string());
    merge_hooks(&mut root, ours, hooks_path)?;
    if before.as_deref() == Some(root.to_string().as_str()) {
        return Ok(());
    }

    let formatted = serde_json::to_string_pretty(&root).map_err(|e| {
        io::Error::other(format!("Failed to serialize {}: {e}", hooks_path.display()))
    })?;
    let mut tmp_name = target.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = target.with_file_name(tmp_name);
    write_owner_only(&tmp, formatted.as_bytes())?;
    // Keep the mode the user gave the file (a 0600 settings.local.json may
    // carry credentials in `env`); a new file stays owner-only.
    if let Some(permissions) = existing_permissions {
        fs::set_permissions(&tmp, permissions)?;
    }
    fs::rename(&tmp, &target)?;
    Ok(())
}

/// Create `path` afresh, readable only by its owner on Unix, and write
/// `contents` — so the temp copy of a settings file never exposes it while
/// being written.
fn write_owner_only(path: &Path, contents: &[u8]) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(contents)
}

/// Deep-merge the Codex hook set into `<project_root>/.codex/hooks.json`,
/// creating it if absent (idempotent, never clobbers existing hooks/keys,
/// atomic write). Repo-local counterpart of [`install_global_codex_hooks`].
pub fn install_codex_hooks(project_root: &Path) -> io::Result<()> {
    let codex_dir = project_root.join(".codex");
    fs::create_dir_all(&codex_dir)?;
    merge_hooks_into_file(&codex_dir.join("hooks.json"), &codex_hooks())
}

/// Install TraceVault's Codex hooks into `<codex_dir>/hooks.json` and append a
/// workspace-mode instruction block to `<codex_dir>/AGENTS.md` (idempotent,
/// atomic), mirroring the Claude global install's `settings.json` + `CLAUDE.md`.
/// Intended for `tracevault init --global --agent codex`. Codex reads `AGENTS.md`
/// for agent instructions, so the workspace-mode block reaches detached Codex
/// sessions the same way the `CLAUDE.md` block reaches Claude ones.
pub fn install_global_codex_hooks(codex_dir: &Path) -> io::Result<()> {
    fs::create_dir_all(codex_dir)?;
    // Canonicalize the base so safe_join keeps target files directly within it.
    let base = codex_dir.canonicalize()?;
    merge_hooks_into_file(&safe_join(&base, "hooks.json")?, &codex_hooks())?;
    append_workspace_md(&safe_join(&base, "AGENTS.md")?)
}

// Workspace-mode instruction block. Agent-agnostic (it only references
// `tracevault repo …`), so it's shared verbatim between the Claude global
// install's `CLAUDE.md` and the Codex global install's `AGENTS.md`.
const GLOBAL_CLAUDE_MD_MARKER: &str = "<!-- tracevault:workspace-mode -->";

const GLOBAL_CLAUDE_MD_BLOCK: &str = "\
<!-- tracevault:workspace-mode -->
## TraceVault (workspace mode)
You may be running detached from any single repository. Before working on a repo, run
`tracevault repo switch <path>` to bind tracing and fetch that repo's policies; treat its
output as binding. Use `--path <path>` on `tracevault repo status` for a one-off. Repos must
already be registered with TraceVault.
";

/// Append the workspace-mode instruction block to `md_path` (creating it if
/// absent), idempotently — guarded by [`GLOBAL_CLAUDE_MD_MARKER`], so a re-run
/// never duplicates it and any existing user content is preserved. Shared by
/// the Claude global install (`CLAUDE.md`) and the Codex global install
/// (`AGENTS.md`).
fn append_workspace_md(md_path: &Path) -> io::Result<()> {
    // Defense-in-depth against path traversal (see `merge_hooks_into_file`).
    if md_path
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to use path with '..': {}", md_path.display()),
        ));
    }
    let existing = if md_path.exists() {
        fs::read_to_string(md_path)?
    } else {
        String::new()
    };
    if existing.contains(GLOBAL_CLAUDE_MD_MARKER) {
        return Ok(());
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    if !content.is_empty() {
        content.push('\n');
    }
    content.push_str(GLOBAL_CLAUDE_MD_BLOCK);
    fs::write(md_path, content)
}

/// Join a constant file name to `base` and confirm the result stays directly
/// within `base` — defense-in-depth so a path derived from the environment
/// ($HOME) cannot escape the intended `.claude` directory.
fn safe_join(base: &Path, name: &str) -> io::Result<PathBuf> {
    let joined = base.join(name);
    if joined.parent() != Some(base) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to access path outside {}: {}",
                base.display(),
                joined.display()
            ),
        ));
    }
    Ok(joined)
}

/// An inner hook belongs to TraceVault when its `command` starts with
/// `tracevault ` — every hook command `init` has ever installed does, and a
/// user's own hook running something else never does. This is what lets a
/// re-run replace exactly our entries (old matcher, timeout, status message,
/// a dropped event) and nothing else.
const TRACEVAULT_COMMAND_PREFIX: &str = "tracevault ";

fn is_tracevault_hook(hook: &serde_json::Value) -> bool {
    hook.get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|c| c.trim_start().starts_with(TRACEVAULT_COMMAND_PREFIX))
}

/// Remove TraceVault-owned inner hooks from one event's matcher groups. A
/// group that held only our hooks is dropped; a group shared with foreign
/// hooks keeps them and everything else it had. Groups without any of our
/// hooks are left untouched. Returns the position of the first dropped group,
/// so the current set can go back where the old one was — which keeps a
/// re-run on an up-to-date file a no-op.
fn strip_tracevault_hooks(entries: &mut Vec<serde_json::Value>) -> Option<usize> {
    let mut slot = None;
    let mut kept = Vec::with_capacity(entries.len());
    for mut entry in entries.drain(..) {
        if let Some(inner) = entry.get_mut("hooks").and_then(|h| h.as_array_mut()) {
            let before = inner.len();
            inner.retain(|hook| !is_tracevault_hook(hook));
            if inner.is_empty() && before > 0 {
                slot.get_or_insert(kept.len());
                continue;
            }
        }
        kept.push(entry);
    }
    *entries = kept;
    slot
}

/// Bring `settings["hooks"]` to exactly `ours` for TraceVault's entries while
/// keeping every foreign hook: our inner hooks (see [`is_tracevault_hook`])
/// are removed from every event, then the current `ours` entries are put back
/// in their place (appended for an event that had none). Re-running converges
/// to the current hook set, and events we no longer install lose our entries.
/// Top-level keys and foreign hooks are never touched.
///
/// Absent values (no `hooks` key, no entry for a given event) are created
/// fresh — that's fine; a missing file reaches here as `{}`. But if an
/// EXISTING value has an unexpected shape (top-level `settings` isn't an
/// object — `null` included —, an existing `hooks` isn't an object, or one of
/// our events isn't an array), this errors loudly instead of silently
/// discarding it. A foreign event whose value isn't an array is left as it is.
fn merge_hooks(
    settings: &mut serde_json::Value,
    ours: &serde_json::Value,
    path: &Path,
) -> io::Result<()> {
    let settings_obj = settings.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a JSON object", path.display()),
        )
    })?;

    let hooks_value = settings_obj
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    let hooks_obj = hooks_value.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} \"hooks\" key is not a JSON object", path.display()),
        )
    })?;

    let empty = serde_json::Map::new();
    let ours_obj = ours.as_object().unwrap_or(&empty);

    // Validate before mutating anything, so an error leaves `settings` as read.
    for event in ours_obj.keys() {
        if hooks_obj.get(event).is_some_and(|v| !v.is_array()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} \"hooks\".\"{event}\" is not a JSON array",
                    path.display()
                ),
            ));
        }
    }

    let mut emptied = Vec::new();
    for (event, value) in hooks_obj.iter_mut() {
        let Some(existing) = value.as_array_mut() else {
            continue;
        };
        let had_entries = !existing.is_empty();
        let slot = strip_tracevault_hooks(existing);
        let our_entries = ours_obj.get(event).and_then(|v| v.as_array());
        match our_entries {
            Some(our_entries) => {
                let at = slot.unwrap_or(existing.len());
                existing.splice(at..at, our_entries.iter().cloned());
            }
            None if had_entries && existing.is_empty() => emptied.push(event.clone()),
            None => {}
        }
    }
    // An event that only ever held our (now retired) hooks goes away entirely.
    hooks_obj.retain(|event, _| !emptied.contains(event));

    for (event, our_array) in ours_obj {
        if !hooks_obj.contains_key(event) {
            hooks_obj.insert(event.clone(), our_array.clone());
        }
    }

    Ok(())
}

/// Install TraceVault's Claude Code hooks into `claude_dir/settings.json`
/// (deep-merged, never clobbering existing hooks/keys) and append a
/// workspace-mode instruction block to `claude_dir/CLAUDE.md` (idempotent,
/// guarded by a marker comment). Intended for `tracevault init --global`,
/// installing once for all Claude Code sessions rather than per-repo.
pub fn install_global_hooks(claude_dir: &Path) -> io::Result<()> {
    fs::create_dir_all(claude_dir)?;

    // Canonicalize the base to defend against path traversal attacks.
    // safe_join will verify that target files stay directly within this base.
    let base = claude_dir.canonicalize()?;

    // --- settings.json: deep-merge hooks (atomic) ---
    merge_hooks_into_file(&safe_join(&base, "settings.json")?, &tracevault_hooks())?;

    // --- CLAUDE.md: append the workspace-mode instruction block, idempotently ---
    append_workspace_md(&safe_join(&base, "CLAUDE.md")?)
}

/// Write the user-level `config.toml` (under `config_root`) carrying the
/// user-context setting. An EXISTING user config is preserved (this mirrors the
/// "don't clobber" contract of the global-hooks install): `requested` overrides
/// the user_context when a flag was given; otherwise an existing value is kept,
/// and only a first-time setup (no existing config) defaults to enabled.
/// When the resulting layer is enabled, the referenced context file is seeded
/// (empty) if absent — a custom `--user-context <path>` seeds THAT file, a
/// disabled layer seeds nothing. Returns the active context file path (the file
/// the hook will read), or `None` when the layer is disabled. Used by `init --global`.
pub fn write_global_user_config_in(
    config_root: &Path,
    requested: Option<UserContext>,
) -> io::Result<Option<PathBuf>> {
    fs::create_dir_all(config_root)?;
    // Preserve an existing (valid) user config; malformed/missing → start fresh.
    let mut config = crate::config::try_load_user_config_in(config_root)
        .ok()
        .flatten()
        .unwrap_or_default();
    // Requested flag wins; else keep an existing value; else first-run default = enabled.
    let user_context = requested
        .or_else(|| config.user_context.clone())
        .unwrap_or(UserContext::Toggle(true));
    let active = user_context
        .resolve()
        .map(|_| user_context.path_in(config_root));
    config.user_context = Some(user_context);
    fs::write(user_config_path_in(config_root), config.to_toml())?;
    if let Some(ref ctx_path) = active {
        if !ctx_path.exists() {
            Context::default().save_to(ctx_path)?;
        }
    }
    Ok(active)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

    /// Installing the hooks is not enough on Codex >= 0.153: they are gated
    /// behind persisted hook trust, and `codex exec` skips untrusted hooks
    /// silently. The notice must say how to grant that trust interactively and
    /// name the flag that unblocks unattended runs — otherwise a user (or a CI
    /// image) reads "Installed" and gets no capture at all.
    #[test]
    fn codex_hook_trust_notice_explains_how_to_grant_trust() {
        let notice = codex_hook_trust_notice();
        assert!(
            notice.contains("hook trust"),
            "notice must name what is missing: {notice}"
        );
        assert!(
            notice.contains("codex") && notice.contains("approve"),
            "notice must tell the user to run codex and approve the prompt: {notice}"
        );
        assert!(
            notice.contains("--dangerously-bypass-hook-trust"),
            "notice must name the flag for unattended/CI runs: {notice}"
        );
        assert!(
            notice.contains("silently"),
            "notice must say the failure is silent, which is why it needs saying: {notice}"
        );
    }

    /// Spawn a one-shot raw-HTTP server that returns `response` to the first
    /// request it accepts (mirrors `commands::repo`'s test module).
    fn spawn_once(response: String) -> String {
        let response: &'static str = Box::leak(response.into_boxed_str());
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

    /// Format a raw HTTP/1.1 response with the given `status` line and a
    /// correct `Content-Length` for `body`.
    fn http_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status,
            body.len(),
            body
        )
    }

    fn add_origin_remote(dir: &Path, url: &str) {
        let ok = std::process::Command::new("git")
            .args(["-C", &dir.to_string_lossy(), "remote", "add", "origin", url])
            .status()
            .expect("git remote add failed")
            .success();
        assert!(ok, "git remote add must succeed");
    }

    #[test]
    fn linked_worktree_primary_detects_linked_and_primary() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("linked-wt");
        std::fs::create_dir_all(&repo).unwrap();
        crate::test_helpers::init_git_repo(&repo);
        crate::test_helpers::add_worktree(&repo, &wt);

        // Primary checkout → None.
        assert!(
            linked_worktree_primary(&repo).is_none(),
            "primary checkout must not be flagged as a linked worktree"
        );
        // Linked worktree → Some(primary root).
        let primary = linked_worktree_primary(&wt).expect("linked worktree must be detected");
        assert_eq!(
            primary.canonicalize().unwrap(),
            repo.canonicalize().unwrap(),
            "must resolve to the primary repo root"
        );
    }

    #[test]
    fn linked_worktree_primary_none_for_non_git() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            linked_worktree_primary(tmp.path()).is_none(),
            "a non-git directory is not a linked worktree"
        );
    }

    /// The inner `hooks[0].command` of a hook-event entry, if present.
    fn entry_command(entry: &serde_json::Value) -> Option<&str> {
        entry.get("hooks")?.get(0)?.get("command")?.as_str()
    }

    #[test]
    fn merge_hooks_into_empty_settings_adds_all_our_events() {
        let mut settings = serde_json::json!({});
        let ours = tracevault_hooks();
        merge_hooks(&mut settings, &ours, Path::new("settings.json")).unwrap();

        let hooks = settings.get("hooks").unwrap();
        for event in [
            "PreToolUse",
            "PostToolUse",
            "Notification",
            "Stop",
            "SessionStart",
            "UserPromptSubmit",
        ] {
            assert!(hooks.get(event).is_some(), "missing event {event}");
        }
    }

    #[test]
    fn merge_hooks_preserves_unrelated_user_hook_and_keys() {
        let mut settings = serde_json::json!({
            "model": "opus",
            "hooks": {
                "PreToolUse": [{
                    "matcher": "",
                    "hooks": [{ "type": "command", "command": "my-own-hook" }]
                }]
            }
        });
        let ours = tracevault_hooks();
        merge_hooks(&mut settings, &ours, Path::new("settings.json")).unwrap();

        // Other top-level keys preserved.
        assert_eq!(settings.get("model").unwrap(), "opus");

        let pre_tool_use = settings["hooks"]["PreToolUse"].as_array().unwrap();
        // Both the user's own hook and ours are present.
        assert_eq!(pre_tool_use.len(), 2);
        let commands: Vec<&str> = pre_tool_use
            .iter()
            .map(|e| entry_command(e).unwrap())
            .collect();
        assert!(commands.contains(&"my-own-hook"));
        assert!(commands.contains(&"tracevault stream --event pre-tool-use"));

        // Other events were added fresh.
        assert!(settings["hooks"]["SessionStart"].is_array());
    }

    #[test]
    fn merge_hooks_is_idempotent() {
        let mut settings = serde_json::json!({});
        let ours = tracevault_hooks();
        merge_hooks(&mut settings, &ours, Path::new("settings.json")).unwrap();
        merge_hooks(&mut settings, &ours, Path::new("settings.json")).unwrap();

        for event in [
            "PreToolUse",
            "PostToolUse",
            "Notification",
            "Stop",
            "SessionStart",
            "UserPromptSubmit",
        ] {
            let arr = settings["hooks"][event].as_array().unwrap();
            assert_eq!(arr.len(), 1, "event {event} should not be duplicated");
        }
    }

    #[test]
    fn merge_hooks_keeps_foreign_hook_sharing_a_group_with_ours() {
        // Our command sits at hooks[1] of a group the user owns: the group,
        // its matcher and the user's hook must survive unchanged, and our
        // command must end up exactly once — in its own current group.
        let mut settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Write|Edit|Bash",
                    "hooks": [
                        { "type": "command", "command": "other", "timeout": 3 },
                        { "type": "command", "command": "tracevault stream --event pre-tool-use" }
                    ]
                }]
            }
        });
        merge_hooks(
            &mut settings,
            &tracevault_hooks(),
            Path::new("settings.json"),
        )
        .unwrap();

        let pre_tool_use = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(
            pre_tool_use[0],
            serde_json::json!({
                "matcher": "Write|Edit|Bash",
                "hooks": [{ "type": "command", "command": "other", "timeout": 3 }]
            })
        );
        assert_eq!(
            &pre_tool_use[1..],
            tracevault_hooks()["PreToolUse"].as_array().unwrap()
        );
    }

    /// A hook set as an older `init` wrote it: narrower PreToolUse matcher,
    /// timeout 5, no status messages and no SessionStart/UserPromptSubmit.
    fn stale_tracevault_hooks() -> serde_json::Value {
        let entry = |matcher: &str, event: &str| {
            serde_json::json!([{
                "matcher": matcher,
                "hooks": [{ "type": "command", "command": format!("tracevault stream --event {event}"), "timeout": 5 }]
            }])
        };
        serde_json::json!({
            "PreToolUse": entry("Write|Edit", "pre-tool-use"),
            "PostToolUse": entry("", "post-tool-use"),
            "Notification": entry("", "notification"),
            "Stop": entry("", "stop"),
        })
    }

    #[test]
    fn merge_hooks_upgrades_stale_tracevault_entries() {
        let mut settings = serde_json::json!({ "hooks": stale_tracevault_hooks() });
        merge_hooks(
            &mut settings,
            &tracevault_hooks(),
            Path::new("settings.json"),
        )
        .unwrap();
        assert_eq!(settings["hooks"], tracevault_hooks());
    }

    #[test]
    fn merge_hooks_removes_our_entries_from_events_we_no_longer_install() {
        // An older Codex install with a PreToolUse hook we have since dropped,
        // next to a foreign event and a foreign non-array value we don't own.
        let mut settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [{ "matcher": "", "hooks": [{ "type": "command", "command": "tracevault stream --event pre-tool-use --agent codex" }] }],
                "SubagentStop": [{ "matcher": "", "hooks": [{ "type": "command", "command": "notify-me" }] }],
                "Custom": "left alone"
            }
        });
        merge_hooks(&mut settings, &codex_hooks(), Path::new("hooks.json")).unwrap();

        let hooks = settings["hooks"].as_object().unwrap();
        assert!(!hooks.contains_key("PreToolUse"), "retired event must go");
        assert_eq!(hooks["SubagentStop"][0]["hooks"][0]["command"], "notify-me");
        assert_eq!(hooks["Custom"], "left alone");
        for (event, entries) in codex_hooks().as_object().unwrap() {
            assert_eq!(&hooks[event], entries, "{event}");
        }
    }

    #[test]
    fn merge_hooks_rerun_keeps_our_entry_in_place() {
        // Ours first, a user hook added after it: a re-run must not move ours
        // behind the user's (that would rewrite an up-to-date file).
        let mut settings = serde_json::json!({ "hooks": tracevault_hooks() });
        settings["hooks"]["Stop"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({ "matcher": "", "hooks": [{ "type": "command", "command": "say done" }] }));
        let before = settings.clone();
        merge_hooks(
            &mut settings,
            &tracevault_hooks(),
            Path::new("settings.json"),
        )
        .unwrap();
        assert_eq!(settings.to_string(), before.to_string());
    }

    #[test]
    fn merge_hooks_does_not_claim_commands_merely_mentioning_tracevault() {
        let foreign = serde_json::json!({
            "matcher": "",
            "hooks": [
                { "type": "command", "command": "tracevault-wrapper stream" },
                { "type": "command", "command": "echo tracevault stream" }
            ]
        });
        let mut settings = serde_json::json!({ "hooks": { "Stop": [foreign.clone()] } });
        merge_hooks(
            &mut settings,
            &tracevault_hooks(),
            Path::new("settings.json"),
        )
        .unwrap();
        assert_eq!(settings["hooks"]["Stop"][0], foreign);
        assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn merge_hooks_errors_on_non_object_top_level() {
        let mut settings = serde_json::json!([1, 2, 3]);
        let ours = tracevault_hooks();
        let err = merge_hooks(&mut settings, &ours, Path::new("settings.json")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("settings.json"));
    }

    #[test]
    fn merge_hooks_errors_on_non_object_top_level_number() {
        let mut settings = serde_json::json!(42);
        let ours = tracevault_hooks();
        let err = merge_hooks(&mut settings, &ours, Path::new("settings.json")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn merge_hooks_errors_on_non_object_hooks() {
        let mut settings = serde_json::json!({ "hooks": "not-an-object" });
        let ours = tracevault_hooks();
        let err = merge_hooks(&mut settings, &ours, Path::new("settings.json")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("hooks"));
    }

    #[test]
    fn merge_hooks_errors_on_non_array_event_value() {
        let mut settings = serde_json::json!({
            "hooks": {
                "PreToolUse": "not-an-array"
            }
        });
        let ours = tracevault_hooks();
        let err = merge_hooks(&mut settings, &ours, Path::new("settings.json")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("PreToolUse"));
    }

    #[test]
    fn install_global_hooks_writes_settings_and_claude_md() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");

        install_global_hooks(&claude_dir).unwrap();

        let settings_content = fs::read_to_string(claude_dir.join("settings.json")).unwrap();
        let settings: serde_json::Value = serde_json::from_str(&settings_content).unwrap();
        for event in ["SessionStart", "UserPromptSubmit", "PreToolUse"] {
            assert!(settings["hooks"].get(event).is_some(), "missing {event}");
        }

        let claude_md = fs::read_to_string(claude_dir.join("CLAUDE.md")).unwrap();
        assert!(claude_md.contains(GLOBAL_CLAUDE_MD_MARKER));
    }

    #[test]
    fn install_global_hooks_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");

        install_global_hooks(&claude_dir).unwrap();
        install_global_hooks(&claude_dir).unwrap();

        let settings_content = fs::read_to_string(claude_dir.join("settings.json")).unwrap();
        let settings: serde_json::Value = serde_json::from_str(&settings_content).unwrap();
        for event in [
            "PreToolUse",
            "PostToolUse",
            "Notification",
            "Stop",
            "SessionStart",
            "UserPromptSubmit",
        ] {
            let arr = settings["hooks"][event].as_array().unwrap();
            assert_eq!(arr.len(), 1, "event {event} should not be duplicated");
        }

        let claude_md = fs::read_to_string(claude_dir.join("CLAUDE.md")).unwrap();
        let marker_count = claude_md.matches(GLOBAL_CLAUDE_MD_MARKER).count();
        assert_eq!(
            marker_count, 1,
            "CLAUDE.md marker should appear exactly once"
        );
    }

    #[test]
    fn install_global_hooks_preserves_existing_claude_md_content() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(
            claude_dir.join("CLAUDE.md"),
            "# My existing notes\nSome content.\n",
        )
        .unwrap();

        install_global_hooks(&claude_dir).unwrap();

        let claude_md = fs::read_to_string(claude_dir.join("CLAUDE.md")).unwrap();
        assert!(claude_md.contains("# My existing notes"));
        assert!(claude_md.contains(GLOBAL_CLAUDE_MD_MARKER));
    }

    #[test]
    fn install_global_hooks_settings_write_is_atomic_no_tmp_leftover() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");

        install_global_hooks(&claude_dir).unwrap();

        let settings_content = fs::read_to_string(claude_dir.join("settings.json")).unwrap();
        let settings: serde_json::Value = serde_json::from_str(&settings_content).unwrap();
        assert!(settings["hooks"].get("SessionStart").is_some());

        let leftover = fs::read_dir(&claude_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(
            !leftover,
            "no settings.json.tmp should remain after install"
        );
    }

    #[test]
    fn install_global_hooks_errors_on_non_object_settings_and_does_not_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        let settings_path = claude_dir.join("settings.json");
        let original = "[1, 2, 3]";
        fs::write(&settings_path, original).unwrap();

        let err = install_global_hooks(&claude_dir).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        // The original (malformed-for-our-purposes but validly-JSON) file
        // must be untouched, and no stray tmp file left behind.
        let after = fs::read_to_string(&settings_path).unwrap();
        assert_eq!(after, original);
        let leftover = fs::read_dir(&claude_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!leftover, "no tmp file should be left behind on error");
    }

    #[test]
    fn install_global_hooks_errors_on_non_object_hooks_key_and_does_not_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        let settings_path = claude_dir.join("settings.json");
        let original = r#"{"hooks": "oops"}"#;
        fs::write(&settings_path, original).unwrap();

        let err = install_global_hooks(&claude_dir).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let after = fs::read_to_string(&settings_path).unwrap();
        assert_eq!(after, original);
    }

    #[test]
    fn install_global_hooks_errors_on_non_array_event_and_does_not_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        let settings_path = claude_dir.join("settings.json");
        let original = r#"{"hooks": {"PreToolUse": "oops"}}"#;
        fs::write(&settings_path, original).unwrap();

        let err = install_global_hooks(&claude_dir).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let after = fs::read_to_string(&settings_path).unwrap();
        assert_eq!(after, original);
    }

    #[test]
    fn safe_join_allows_direct_child_file() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let result = safe_join(base, "settings.json").unwrap();
        assert_eq!(result.parent(), Some(base));
        assert_eq!(result.file_name().unwrap(), "settings.json");
    }

    #[test]
    fn safe_join_rejects_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let result = safe_join(base, "../evil");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("refusing to access path outside"));
    }

    #[test]
    fn safe_join_rejects_absolute_path() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let result = safe_join(base, "/etc/passwd");
        // An absolute path joined to a base will replace the base,
        // so parent will not equal base.
        assert!(result.is_err());
    }

    #[test]
    fn write_global_user_config_enables_and_creates_context() {
        let root = tempfile::tempdir().unwrap();
        let active = write_global_user_config_in(root.path(), None).unwrap();
        let cfg = crate::config::try_load_user_config_in(root.path())
            .unwrap()
            .unwrap();
        // Validate against the INJECTED root (resolve_in), not tv_config_root():
        // a first-run default enables at the `context.json` under `root`.
        assert_eq!(
            cfg.user_context.unwrap().resolve_in(root.path()),
            Some(crate::config::default_user_context_path_in(root.path()))
        );
        let ctx = crate::config::default_user_context_path_in(root.path());
        assert_eq!(active, Some(ctx.clone()));
        assert!(ctx.exists());
    }

    #[test]
    fn write_global_user_config_can_disable() {
        let root = tempfile::tempdir().unwrap();
        let active = write_global_user_config_in(
            root.path(),
            Some(crate::config::UserContext::Toggle(false)),
        )
        .unwrap();
        let cfg = crate::config::try_load_user_config_in(root.path())
            .unwrap()
            .unwrap();
        assert!(matches!(
            cfg.user_context,
            Some(crate::config::UserContext::Toggle(false))
        ));
        assert_eq!(active, None, "disabled → no active context path");
        // A disabled layer must NOT seed a context file.
        assert!(
            !crate::config::default_user_context_path_in(root.path()).exists(),
            "disabled user context must not create a context.json"
        );
    }

    #[test]
    fn write_global_user_config_custom_path_seeds_that_file_only() {
        let root = tempfile::tempdir().unwrap();
        let custom = root.path().join("custom-ctx.json");
        let active = write_global_user_config_in(
            root.path(),
            Some(crate::config::UserContext::Path(
                custom.to_string_lossy().into_owned(),
            )),
        )
        .unwrap();
        assert_eq!(active, Some(custom.clone()));
        assert!(custom.exists(), "the configured custom path must be seeded");
        assert!(
            !crate::config::default_user_context_path_in(root.path()).exists(),
            "the default context.json must NOT be created when a custom path is configured"
        );
    }

    #[test]
    fn write_global_user_config_preserves_existing_on_rerun_without_flags() {
        let root = tempfile::tempdir().unwrap();
        let custom = root.path().join("custom-ctx.json");
        // First run pins a custom path.
        write_global_user_config_in(
            root.path(),
            Some(crate::config::UserContext::Path(
                custom.to_string_lossy().into_owned(),
            )),
        )
        .unwrap();
        // Re-run with NO flags must NOT reset it back to the default.
        let active = write_global_user_config_in(root.path(), None).unwrap();
        assert_eq!(
            active,
            Some(custom.clone()),
            "re-run without flags must preserve the custom path"
        );
        let cfg = crate::config::try_load_user_config_in(root.path())
            .unwrap()
            .unwrap();
        assert_eq!(cfg.user_context.unwrap().path_in(root.path()), custom);
    }

    #[test]
    fn install_global_codex_hooks_writes_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let codex_dir = tmp.path().join(".codex");

        install_global_codex_hooks(&codex_dir).unwrap();
        install_global_codex_hooks(&codex_dir).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(codex_dir.join("hooks.json")).unwrap())
                .unwrap();
        for event in ["SessionStart", "UserPromptSubmit", "PostToolUse", "Stop"] {
            let arr = v["hooks"][event].as_array().unwrap();
            assert_eq!(arr.len(), 1, "event {event} must not be duplicated");
        }
        assert!(v["hooks"].get("PreToolUse").is_none());
        // AGENTS.md carries the workspace-mode block exactly once across re-runs.
        let agents_md = fs::read_to_string(codex_dir.join("AGENTS.md")).unwrap();
        assert_eq!(
            agents_md.matches(GLOBAL_CLAUDE_MD_MARKER).count(),
            1,
            "AGENTS.md workspace-mode block must appear exactly once"
        );
        // No stray temp file left behind.
        let leftover = fs::read_dir(&codex_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!leftover);
    }

    #[test]
    fn codex_hooks_has_expected_events_and_commands() {
        let h = codex_hooks();
        for event in ["SessionStart", "UserPromptSubmit", "PostToolUse", "Stop"] {
            assert!(h.get(event).is_some(), "missing codex event {event}");
        }
        // No PreToolUse (intentionally dropped — PostToolUse + Stop capture the
        // rollout; a pre-tool hook would only double per-call streaming) and no
        // Notification event (Codex has none).
        assert!(h.get("PreToolUse").is_none());
        assert!(h.get("Notification").is_none());
        // Capture commands carry --agent codex; injection hooks do not need it.
        let cmd = |e: &str| h[e][0]["hooks"][0]["command"].as_str().unwrap().to_string();
        assert_eq!(
            cmd("PostToolUse"),
            "tracevault stream --event post-tool-use --agent codex"
        );
        assert_eq!(cmd("Stop"), "tracevault stream --event stop --agent codex");
        assert_eq!(cmd("SessionStart"), "tracevault session-start");
        assert_eq!(cmd("UserPromptSubmit"), "tracevault user-prompt");
        // Matchers are "" (all tools) — Codex edits arrive via the transcript.
        assert_eq!(h["PostToolUse"][0]["matcher"].as_str().unwrap(), "");
    }

    #[test]
    fn install_codex_hooks_writes_hooks_json_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        install_codex_hooks(tmp.path()).unwrap();
        install_codex_hooks(tmp.path()).unwrap();

        let content = fs::read_to_string(tmp.path().join(".codex").join("hooks.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        for event in ["SessionStart", "UserPromptSubmit", "PostToolUse", "Stop"] {
            let arr = v["hooks"][event].as_array().unwrap();
            assert_eq!(
                arr.len(),
                1,
                "event {event} must not be duplicated on re-run"
            );
        }
        assert!(v["hooks"].get("PreToolUse").is_none());
    }

    #[test]
    fn install_codex_hooks_preserves_existing_user_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let codex_dir = tmp.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        // Seed a user hook under an event we install (PostToolUse) plus one under
        // an event we DON'T (PreToolUse) — both must survive the merge.
        fs::write(
            codex_dir.join("hooks.json"),
            r#"{"hooks":{"PostToolUse":[{"matcher":"","hooks":[{"type":"command","command":"my-own"}]}],"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"user-pretool"}]}]}}"#,
        )
        .unwrap();

        install_codex_hooks(tmp.path()).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(codex_dir.join("hooks.json")).unwrap())
                .unwrap();
        // Our PostToolUse command is merged in alongside the user's own.
        let post_cmds: Vec<&str> = v["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["hooks"][0]["command"].as_str().unwrap())
            .collect();
        assert!(
            post_cmds.contains(&"my-own"),
            "user PostToolUse hook preserved"
        );
        assert!(post_cmds.contains(&"tracevault stream --event post-tool-use --agent codex"));
        // The user's PreToolUse hook is left untouched (we install no PreToolUse).
        let pre_cmds: Vec<&str> = v["hooks"]["PreToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["hooks"][0]["command"].as_str().unwrap())
            .collect();
        assert_eq!(pre_cmds, vec!["user-pretool"], "unmanaged event untouched");
    }

    /// A settings file a user might really have: other keys, foreign hooks in
    /// their own groups and in a group shared with ours, keys in non-sorted
    /// order, and a stale TraceVault install.
    const FOREIGN_SETTINGS: &str = r#"{
  "permissions": { "allow": ["Bash(ls)"] },
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [ { "type": "command", "command": "./guard.sh", "timeout": 7 } ] },
      { "matcher": "Write|Edit", "hooks": [
        { "type": "command", "command": "tracevault stream --event pre-tool-use", "timeout": 5 },
        { "type": "command", "command": "prettier --check", "statusMessage": "fmt" }
      ] }
    ],
    "Stop": [ { "matcher": "", "hooks": [ { "type": "command", "command": "tracevault stream --event stop", "timeout": 5 } ] } ],
    "SubagentStop": [ { "hooks": [ { "command": "notify", "type": "command" } ], "matcher": "" } ]
  },
  "model": "opus"
}"#;

    /// Serialised forms of the foreign entries in [`FOREIGN_SETTINGS`], as
    /// they must appear after an install: unchanged values, unchanged key
    /// order; the shared group just loses our hook.
    fn expected_foreign_entries() -> Vec<String> {
        [
            r#"{"matcher":"Bash","hooks":[{"type":"command","command":"./guard.sh","timeout":7}]}"#,
            r#"{"matcher":"Write|Edit","hooks":[{"type":"command","command":"prettier --check","statusMessage":"fmt"}]}"#,
            r#"{"hooks":[{"command":"notify","type":"command"}],"matcher":""}"#,
        ]
        .map(String::from)
        .to_vec()
    }

    fn holds_tracevault_hook(entry: &serde_json::Value) -> bool {
        entry["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .any(is_tracevault_hook)
    }

    fn assert_installed_over_foreign(path: &Path, ours: &serde_json::Value) {
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(
            v.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["permissions", "hooks", "model"],
            "top-level keys kept, in their order"
        );
        assert_eq!(
            v["permissions"],
            serde_json::json!({ "allow": ["Bash(ls)"] })
        );
        let hooks = v["hooks"].as_object().unwrap();
        let foreign: Vec<String> = hooks
            .values()
            .flat_map(|entries| entries.as_array().unwrap())
            .filter(|e| !holds_tracevault_hook(e))
            .map(|e| e.to_string())
            .collect();
        assert_eq!(foreign, expected_foreign_entries());
        let ours = ours.as_object().unwrap();
        for (event, entries) in hooks {
            let mine: Vec<&serde_json::Value> = entries
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| holds_tracevault_hook(e))
                .collect();
            let want: Vec<&serde_json::Value> = ours
                .get(event)
                .map(|o| o.as_array().unwrap().iter().collect())
                .unwrap_or_default();
            assert_eq!(mine, want, "TraceVault entries under {event}");
        }
        for event in ours.keys() {
            assert!(hooks.contains_key(event), "missing {event}");
        }
    }

    #[test]
    fn install_claude_hooks_keeps_foreign_hooks_in_both_settings_files() {
        for target in [ClaudeSettingsTarget::Shared, ClaudeSettingsTarget::Local] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join(".claude").join(target.filename());
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, FOREIGN_SETTINGS).unwrap();

            install_claude_hooks(tmp.path(), target).unwrap();
            assert_installed_over_foreign(&path, &tracevault_hooks());

            // A second run finds the file current and leaves its bytes alone.
            let after_first = fs::read(&path).unwrap();
            install_claude_hooks(tmp.path(), target).unwrap();
            assert_eq!(fs::read(&path).unwrap(), after_first, "{target:?}");
        }
    }

    #[test]
    fn install_global_hooks_keeps_foreign_hooks_and_upgrades_ours() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(claude_dir.join("settings.json"), FOREIGN_SETTINGS).unwrap();

        install_global_hooks(&claude_dir).unwrap();
        assert_installed_over_foreign(&claude_dir.join("settings.json"), &tracevault_hooks());
    }

    #[test]
    fn install_codex_hooks_keeps_foreign_hooks_and_upgrades_ours() {
        for global in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let codex_dir = tmp.path().join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            fs::write(codex_dir.join("hooks.json"), FOREIGN_SETTINGS).unwrap();

            if global {
                install_global_codex_hooks(&codex_dir).unwrap();
            } else {
                install_codex_hooks(tmp.path()).unwrap();
            }
            assert_installed_over_foreign(&codex_dir.join("hooks.json"), &codex_hooks());
        }
    }

    #[test]
    fn up_to_date_hook_file_is_not_rewritten() {
        // Hand-formatted (4-space indent) but already current: no reformatting.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".claude/settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut body = Vec::new();
        let fmt = serde_json::ser::PrettyFormatter::with_indent(b"    ");
        let mut ser = serde_json::Serializer::with_formatter(&mut body, fmt);
        serde::Serialize::serialize(
            &serde_json::json!({ "hooks": tracevault_hooks() }),
            &mut ser,
        )
        .unwrap();
        fs::write(&path, &body).unwrap();

        install_claude_hooks(tmp.path(), ClaudeSettingsTarget::Shared).unwrap();
        assert_eq!(fs::read(&path).unwrap(), body);
    }

    #[test]
    fn install_claude_hooks_refuses_malformed_settings_without_clobbering() {
        for (target, body) in [
            (ClaudeSettingsTarget::Shared, "{ not json"),
            (ClaudeSettingsTarget::Local, "[1, 2]"),
            (ClaudeSettingsTarget::Shared, "null"),
            (ClaudeSettingsTarget::Shared, r#"{"hooks": []}"#),
            (ClaudeSettingsTarget::Local, r#"{"hooks": {"Stop": {}}}"#),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join(".claude").join(target.filename());
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();

            let err = install_claude_hooks(tmp.path(), target).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{body}");
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                body,
                "file must be untouched"
            );
            assert_eq!(
                fs::read_dir(path.parent().unwrap()).unwrap().count(),
                1,
                "no temp file left behind"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn install_claude_hooks_keeps_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".claude/settings.local.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, FOREIGN_SETTINGS).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        install_claude_hooks(tmp.path(), ClaudeSettingsTarget::Local).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_installed_over_foreign(&path, &tracevault_hooks());
    }

    #[cfg(unix)]
    #[test]
    fn install_claude_hooks_updates_symlinked_settings_at_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let central = tmp.path().join("central/claude-settings.json");
        fs::create_dir_all(central.parent().unwrap()).unwrap();
        fs::write(&central, FOREIGN_SETTINGS).unwrap();
        let link = tmp.path().join("repo/.claude/settings.json");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&central, &link).unwrap();

        install_claude_hooks(&tmp.path().join("repo"), ClaudeSettingsTarget::Shared).unwrap();

        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_installed_over_foreign(&central, &tracevault_hooks());
    }

    #[cfg(unix)]
    #[test]
    fn install_claude_hooks_refuses_dangling_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join(".claude/settings.json");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("gone.json"), &link).unwrap();

        assert!(install_claude_hooks(tmp.path(), ClaudeSettingsTarget::Shared).is_err());
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!tmp.path().join("gone.json").exists());
    }

    #[test]
    fn validate_init_flags_rejects_claude_settings_with_codex() {
        assert!(validate_init_flags(crate::agent::Agent::Codex, true).is_err());
    }

    #[test]
    fn validate_init_flags_allows_codex_without_claude_settings() {
        assert!(validate_init_flags(crate::agent::Agent::Codex, false).is_ok());
    }

    #[test]
    fn validate_init_flags_allows_claude_with_claude_settings() {
        assert!(validate_init_flags(crate::agent::Agent::ClaudeCode, true).is_ok());
    }

    #[test]
    fn merge_hooks_into_file_rejects_parent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("..").join("hooks.json");
        let err = merge_hooks_into_file(&bad, &codex_hooks()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn append_workspace_md_rejects_parent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("..").join("AGENTS.md");
        let err = append_workspace_md(&bad).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn init_in_directory_codex_installs_codex_hooks_and_gitignores() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        crate::test_helpers::init_git_repo(root);
        // `init` refuses to run without a credential; a per-repo one keeps
        // this test independent of the process env and the user's login.
        fs::create_dir_all(root.join(".tracevault")).unwrap();
        fs::write(
            root.join(".tracevault/config.toml"),
            "api_key = \"tvk_test\"\n",
        )
        .unwrap();

        let entry = init_in_directory(
            root,
            None,
            None,
            false,
            crate::config::UserContext::Toggle(false),
            crate::agent::Agent::Codex,
        )
        .await
        .unwrap();

        // Codex hooks file written; Claude settings NOT created.
        assert!(root.join(".codex").join("hooks.json").exists());
        assert!(!root.join(".claude").join("settings.json").exists());
        assert_eq!(entry, ".codex/hooks.json");

        // .gitignore covers the codex hooks file.
        let gi = fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(gi.contains(".codex/hooks.json"));
        assert!(gi.contains(".tracevault/"));
    }

    /// A successful `POST /api/v1/repos` during `init` must persist the
    /// returned `repo_id` into `.tracevault/config.toml` — this is the
    /// "everything worked" happy path of the registration step at the end of
    /// `init_in_directory`, previously untested.
    ///
    /// `XDG_CONFIG_HOME` is redirected so this doesn't read/write the
    /// developer's real `~/.config/tracevault/credentials.json`;
    /// `TRACEVAULT_API_KEY` is pinned directly since `resolve_credentials`
    /// checks it before the credentials file. `_env_lock` serializes this
    /// against other tests in the crate that mutate the same env vars
    /// (see `test_helpers::lock_env_mutation`).
    #[tokio::test]
    async fn init_in_directory_registers_repo_and_persists_repo_id() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        crate::test_helpers::init_git_repo(root);
        add_origin_remote(root, "git@github.com:org/repo.git");

        let repo_id = "11111111-1111-4111-8111-111111111111";
        let body = format!(r#"{{"repo_id":"{repo_id}"}}"#);
        // The remote is unknown (domain 404), so init registers; the resolve
        // after registering may fail, it is display-only.
        let (base, _rx) = crate::test_helpers::spawn_seq(vec![
            http_response("404 Not Found", NO_REMOTE),
            http_response("200 OK", &body),
        ]);

        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", tmp.path());
        _guard.set("TRACEVAULT_API_KEY", "tok");
        _guard.remove("TRACEVAULT_SERVER_URL");

        init_in_directory(
            root,
            Some(&base),
            None,
            true,
            crate::config::UserContext::Toggle(false),
            crate::agent::Agent::Codex,
        )
        .await
        .unwrap();

        let cfg = crate::config::TracevaultConfig::load(root).expect("config.toml written by init");
        assert_eq!(
            cfg.repo_id,
            Some(repo_id.to_string()),
            "successful registration must persist repo_id to config.toml"
        );
    }

    /// A 403 on `POST /api/v1/repos` must be a non-fatal warning (`init`
    /// still succeeds and installs hooks) and must NOT persist a `repo_id` —
    /// there's nothing valid to persist. The exact wording of the warning is
    /// covered by `registration_failure_message_flags_403_as_not_authorized`
    /// below.
    #[tokio::test]
    async fn init_in_directory_registration_403_does_not_persist_repo_id() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        crate::test_helpers::init_git_repo(root);
        add_origin_remote(root, "git@github.com:org/repo.git");

        let (base, _rx) = crate::test_helpers::spawn_seq(vec![
            http_response("404 Not Found", NO_REMOTE),
            http_response("403 Forbidden", ""),
        ]);

        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", tmp.path());
        _guard.set("TRACEVAULT_API_KEY", "tok");
        _guard.remove("TRACEVAULT_SERVER_URL");

        let entry = init_in_directory(
            root,
            Some(&base),
            None,
            true,
            crate::config::UserContext::Toggle(false),
            crate::agent::Agent::Codex,
        )
        .await
        .expect("a registration failure (403) must not abort init itself");
        assert_eq!(entry, ".codex/hooks.json");

        let cfg = crate::config::TracevaultConfig::load(root).expect("config.toml written by init");
        assert!(
            cfg.repo_id.is_none(),
            "a 403 registration failure must not persist a repo_id"
        );
    }

    /// The actual error shape `ApiClient::register_repo` produces for a 403
    /// response (via the same mock-server harness) must be recognized by
    /// `registration_failure_message` as a not-authorized case, not a
    /// generic server fault.
    #[tokio::test]
    async fn registration_failure_message_flags_403_as_not_authorized() {
        let base = spawn_once(http_response("403 Forbidden", ""));
        let client = ApiClient::new(&base, Some("tok"));
        let err = client
            .register_repo(crate::api_client::RegisterRepoRequest {
                repo_name: "repo".into(),
                github_url: Some("git@github.com:org/repo.git".into()),
            })
            .await
            .unwrap_err();
        let msg = registration_failure_message(err.as_ref());
        assert!(
            msg.contains("not authorized"),
            "403 registration failure must surface a not-authorized message; got: {msg}"
        );
    }

    /// A non-403 registration failure (e.g. a 500) keeps the generic
    /// "could not register" wording, distinguishing it from the 403 case.
    #[tokio::test]
    async fn registration_failure_message_generic_for_non_403() {
        let base = spawn_once(http_response("500 Internal Server Error", ""));
        let client = ApiClient::new(&base, Some("tok"));
        let err = client
            .register_repo(crate::api_client::RegisterRepoRequest {
                repo_name: "repo".into(),
                github_url: Some("git@github.com:org/repo.git".into()),
            })
            .await
            .unwrap_err();
        let msg = registration_failure_message(err.as_ref());
        assert!(
            msg.contains("could not register repo on server"),
            "got: {msg}"
        );
        assert!(!msg.contains("not authorized"), "got: {msg}");
    }

    /// A 500 whose BODY happens to contain the digits "403" (e.g. a proxy
    /// error page quoting an unrelated status) must not be misclassified as
    /// a 403-not-authorized failure — the match must key on the actual
    /// status prefix, not a bare substring search over the whole message.
    #[tokio::test]
    async fn registration_failure_message_does_not_misclassify_403_in_body() {
        let base = spawn_once(http_response(
            "500 Internal Server Error",
            "upstream error 403 from proxy",
        ));
        let client = ApiClient::new(&base, Some("tok"));
        let err = client
            .register_repo(crate::api_client::RegisterRepoRequest {
                repo_name: "repo".into(),
                github_url: Some("git@github.com:org/repo.git".into()),
            })
            .await
            .unwrap_err();
        let msg = registration_failure_message(err.as_ref());
        assert!(
            !msg.contains("not authorized"),
            "a 403 substring in the body must not trigger the not-authorized message; got: {msg}"
        );
        assert!(
            msg.contains("could not register repo on server"),
            "got: {msg}"
        );
    }

    /// A 404 (the registration route doesn't exist on this server) surfaces a
    /// CLI/server version-mismatch hint rather than the generic wording, so
    /// the user has an actionable next step instead of an opaque status code.
    #[tokio::test]
    async fn registration_failure_message_flags_404_as_version_mismatch() {
        let base = spawn_once(http_response("404 Not Found", ""));
        let client = ApiClient::new(&base, Some("tok"));
        let err = client
            .register_repo(crate::api_client::RegisterRepoRequest {
                repo_name: "repo".into(),
                github_url: Some("git@github.com:org/repo.git".into()),
            })
            .await
            .unwrap_err();
        let msg = registration_failure_message(err.as_ref());
        assert!(
            msg.to_lowercase().contains("version mismatch"),
            "a 404 registration failure should hint at a version mismatch; got: {msg}"
        );
    }

    // ── VIS-1018: init never registers a second repo for a known remote ───────

    /// The server's domain-404 body for an untracked remote.
    const NO_REMOTE: &str = r#"{"error":"No remote for that URL"}"#;
    const REMOTE_ID: &str = "2280a1c9-0000-4000-8000-000000000003";
    const CANONICAL: &str = "44000761-0000-4000-8000-000000000001";
    const STRAY: &str = "7995d35d-0000-4000-8000-000000000002";

    fn resolve_remote_ok() -> String {
        http_response(
            "200 OK",
            &format!(
                r#"{{"remote_id":"{REMOTE_ID}","name":"tracing","normalized_url":"github.com/org/repo","clone_status":"ready"}}"#
            ),
        )
    }

    fn remote_detail(repos: &[(&str, &str)]) -> String {
        let repos: Vec<String> = repos
            .iter()
            .map(|(id, name)| format!(r#"{{"id":"{id}","name":"{name}"}}"#))
            .collect();
        http_response(
            "200 OK",
            &format!(
                r#"{{"name":"tracing","normalized_url":"github.com/org/repo","clone_status":"ready","repos":[{}]}}"#,
                repos.join(",")
            ),
        )
    }

    /// Run `init` against `responses` and return the persisted repo_id and
    /// every request the server saw.
    async fn init_against(root: &Path, responses: Vec<String>) -> (Option<String>, Vec<String>) {
        let (base, rx) = crate::test_helpers::spawn_seq(responses);
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", root);
        _guard.set("TRACEVAULT_API_KEY", "tok");
        _guard.remove("TRACEVAULT_SERVER_URL");

        init_in_directory(
            root,
            Some(&base),
            None,
            true,
            crate::config::UserContext::Toggle(false),
            crate::agent::Agent::Codex,
        )
        .await
        .expect("init succeeds");

        let cfg = crate::config::TracevaultConfig::load(root).expect("config.toml written by init");
        (cfg.repo_id, rx.try_iter().collect())
    }

    fn registered(requests: &[String]) -> bool {
        requests.iter().any(|r| r.starts_with("POST /api/v1/repos"))
    }

    /// The prod incident: the checkout's directory is not named like the
    /// remote's existing repo. init binds that repo, and registers nothing.
    #[tokio::test]
    async fn init_binds_the_existing_repo_of_a_known_remote() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(tmp.path());
        add_origin_remote(tmp.path(), "git@github.com:org/repo.git");

        let (repo_id, requests) = init_against(
            tmp.path(),
            vec![
                resolve_remote_ok(),
                remote_detail(&[(CANONICAL, "tracevault")]),
            ],
        )
        .await;

        assert_eq!(repo_id.as_deref(), Some(CANONICAL));
        assert!(!registered(&requests), "requests: {requests:?}");
    }

    /// Several records and one is named like this checkout: that one — the
    /// record registering would have upserted anyway.
    #[tokio::test]
    async fn init_binds_the_record_named_like_the_checkout() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(tmp.path());
        add_origin_remote(tmp.path(), "git@github.com:org/repo.git");
        let dir_name = crate::resolution::git_repo_name(tmp.path());

        let (repo_id, requests) = init_against(
            tmp.path(),
            vec![
                resolve_remote_ok(),
                remote_detail(&[(CANONICAL, "tracevault"), (STRAY, &dir_name)]),
            ],
        )
        .await;

        assert_eq!(repo_id.as_deref(), Some(STRAY));
        assert!(!registered(&requests));
    }

    /// Re-running init keeps the repo the previous config.toml bound.
    #[tokio::test]
    async fn init_rerun_keeps_the_previously_bound_repo() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(tmp.path());
        add_origin_remote(tmp.path(), "git@github.com:org/repo.git");
        fs::create_dir_all(tmp.path().join(".tracevault")).unwrap();
        fs::write(
            tmp.path().join(".tracevault/config.toml"),
            format!("repo_id = \"{STRAY}\"\n"),
        )
        .unwrap();

        let (repo_id, requests) = init_against(
            tmp.path(),
            vec![
                resolve_remote_ok(),
                remote_detail(&[(CANONICAL, "tracevault"), (STRAY, "tracing-old")]),
            ],
        )
        .await;

        assert_eq!(repo_id.as_deref(), Some(STRAY));
        assert!(!registered(&requests));
    }

    /// Several records, nothing says which: no guess, no new record.
    #[tokio::test]
    async fn init_registers_nothing_when_the_codebase_is_ambiguous() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(tmp.path());
        add_origin_remote(tmp.path(), "git@github.com:org/repo.git");

        let (repo_id, requests) = init_against(
            tmp.path(),
            vec![
                resolve_remote_ok(),
                remote_detail(&[(CANONICAL, "tracevault"), (STRAY, "tracing-old")]),
            ],
        )
        .await;

        assert_eq!(repo_id, None);
        assert!(!registered(&requests));
    }

    /// A known codebase with no live repo (e.g. its last one was tombstoned)
    /// still registers.
    #[tokio::test]
    async fn init_registers_for_a_codebase_without_live_repos() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(tmp.path());
        add_origin_remote(tmp.path(), "git@github.com:org/repo.git");

        let (repo_id, requests) = init_against(
            tmp.path(),
            vec![
                resolve_remote_ok(),
                remote_detail(&[]),
                http_response("201 Created", &format!(r#"{{"repo_id":"{CANONICAL}"}}"#)),
            ],
        )
        .await;

        assert_eq!(repo_id.as_deref(), Some(CANONICAL));
        assert!(registered(&requests));
    }

    /// If the server cannot say what it has, init does not register blind.
    #[tokio::test]
    async fn init_does_not_register_when_the_remote_check_fails() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        crate::test_helpers::init_git_repo(tmp.path());
        add_origin_remote(tmp.path(), "git@github.com:org/repo.git");

        let (repo_id, requests) = init_against(
            tmp.path(),
            vec![http_response("500 Internal Server Error", "{}")],
        )
        .await;

        assert_eq!(repo_id, None);
        assert!(!registered(&requests));
    }
}
