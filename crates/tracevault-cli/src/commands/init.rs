use crate::api_client::ApiClient;
use crate::config::{TracevaultConfig, UserContext};
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

pub fn git_remote_url(project_root: &Path) -> Option<String> {
    std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn parse_github_org(remote_url: &str) -> Option<String> {
    // SSH: git@github.com:VirtusLab/visdom-ai-tracing.git
    if let Some(path) = remote_url.strip_prefix("git@github.com:") {
        return path.split('/').next().map(String::from);
    }
    // HTTPS: https://github.com/VirtusLab/visdom-ai-tracing.git
    if let Some(path) = remote_url
        .strip_prefix("https://github.com/")
        .or_else(|| remote_url.strip_prefix("http://github.com/"))
    {
        return path.split('/').next().map(String::from);
    }
    None
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

pub async fn init_in_directory(
    project_root: &Path,
    server_url: Option<&str>,
    claude_settings: Option<ClaudeSettingsTarget>,
    no_gitignore: bool,
    user_context: UserContext,
) -> Result<ClaudeSettingsTarget, io::Error> {
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

    let target = resolve_claude_target(claude_settings)?;

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
        update_root_gitignore(project_root, target)?;
    }

    // Register repo on server if authenticated, server URL known, and git remote available
    let remote_url = git_remote_url(project_root);
    if remote_url.is_none() {
        eprintln!("Warning: no git remote 'origin' configured. Skipping server registration.");
        eprintln!("Run 'git remote add origin <url>' then 'tracevault sync' to register.");
    }

    // Extract org slug from GitHub remote URL
    let org_slug = remote_url.as_deref().and_then(parse_github_org);

    // Write config (include server_url and org_slug if available)
    let mut config = TracevaultConfig::default();
    if let Some(url) = server_url {
        config.server_url = Some(url.to_string());
    }
    config.org_slug = org_slug.clone();
    config.user_context = user_context;
    fs::write(
        TracevaultConfig::config_path(project_root),
        config.to_toml(),
    )?;

    // Install Claude Code hooks into the chosen settings file
    install_claude_hooks(project_root, target)?;

    // Install git hooks
    install_git_hook(project_root)?;
    install_post_commit_hook(project_root)?;

    // Detect AI tools in the project
    let detected = crate::hooks::detect_tools(project_root);
    for tool in &detected {
        println!("  Detected: {}", tool.name());
    }

    let (resolved_url, resolved_token) = crate::api_client::resolve_credentials(project_root);
    let effective_url = server_url.map(String::from).or(resolved_url);

    if resolved_token.is_none() {
        eprintln!("Not logged in. Run 'tracevault login' to register this repo with the server.");
    } else if let (Some(url), Some(remote), Some(slug)) = (effective_url, remote_url, org_slug) {
        let client = ApiClient::new(&url, resolved_token.as_deref());
        let repo_name = git_repo_name(project_root);

        match client
            .register_repo(
                &slug,
                crate::api_client::RegisterRepoRequest {
                    repo_name,
                    github_url: Some(remote),
                },
            )
            .await
        {
            Ok(resp) => {
                println!("Repo registered on server (id: {})", resp.repo_id);
                // Save repo_id to config
                if let Some(mut cfg) = TracevaultConfig::load(project_root) {
                    cfg.repo_id = Some(resp.repo_id.to_string());
                    let _ = fs::write(TracevaultConfig::config_path(project_root), cfg.to_toml());
                }
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("404") {
                    eprintln!("Warning: organization '{}' not found on the server.", slug);
                    eprintln!(
                        "Create it first at your TraceVault instance, then run 'tracevault sync'."
                    );
                } else if msg.contains("403") {
                    eprintln!("Warning: you are not a member of organization '{}'.", slug);
                } else {
                    eprintln!("Warning: could not register repo on server: {e}");
                }
            }
        }
    }

    Ok(target)
}

fn update_root_gitignore(
    project_root: &Path,
    target: ClaudeSettingsTarget,
) -> Result<(), io::Error> {
    let path = project_root.join(".gitignore");
    let existing = if path.exists() {
        fs::read_to_string(&path)?
    } else {
        String::new()
    };

    // Only ignore what `init` actually creates or modifies: the `.tracevault/`
    // directory and the single Claude settings file we wrote hooks into. The
    // other settings file is left untouched, so we don't add it here.
    let needed: Vec<&str> = [".tracevault/", target.gitignore_entry()]
        .iter()
        .copied()
        .filter(|entry| !existing.lines().any(|line| line.trim() == *entry))
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

fn git_repo_name(project_root: &Path) -> String {
    std::process::Command::new("git")
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

const HOOK_MARKER: &str = "# tracevault:enforce";
const OLD_HOOK_MARKER: &str = "# tracevault:auto-push";

fn install_git_hook(project_root: &Path) -> Result<(), io::Error> {
    let hooks_dir = project_root.join(".git/hooks");
    fs::create_dir_all(&hooks_dir)?;

    let hook_path = hooks_dir.join("pre-push");
    let tracevault_block = format!(
        "{HOOK_MARKER}\ntracevault sync 2>/dev/null || true\ntracevault check || {{ echo \"tracevault: policy check failed\"; exit 1; }}\n"
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

    let filename = target.filename();
    let settings_path = claude_dir.join(filename);
    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Failed to parse .claude/{filename}: {e}"),
            )
        })?
    } else {
        serde_json::json!({})
    };

    let hooks = tracevault_hooks();

    // Merge hooks into existing settings
    let settings_obj = settings.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(".claude/{filename} is not a JSON object"),
        )
    })?;

    settings_obj.insert("hooks".to_string(), hooks);

    let formatted = serde_json::to_string_pretty(&settings)
        .map_err(|e| io::Error::other(format!("Failed to serialize settings: {e}")))?;
    fs::write(&settings_path, formatted)?;

    Ok(())
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

const GLOBAL_CLAUDE_MD_MARKER: &str = "<!-- tracevault:workspace-mode -->";

const GLOBAL_CLAUDE_MD_BLOCK: &str = "\
<!-- tracevault:workspace-mode -->
## TraceVault (workspace mode)
You may be running detached from any single repository. Before working on a repo, run
`tracevault repo switch <path>` to bind tracing and fetch that repo's policies; treat its
output as binding. Use `--path <path>` on `tracevault repo status` for a one-off. Repos must
already be registered with TraceVault.
";

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

/// Merge our hook entries into an existing `settings["hooks"]` object, per
/// event, appending our entry only if no existing entry in that event's
/// array already has an inner hook with the same `command` (dedupe → makes
/// repeated calls idempotent). Never removes or overwrites the user's other
/// hooks or top-level keys.
fn merge_hooks(settings: &mut serde_json::Value, ours: &serde_json::Value) {
    if !settings.is_object() {
        *settings = serde_json::json!({});
    }
    let settings_obj = settings.as_object_mut().expect("just ensured object");

    let hooks_value = settings_obj
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if !hooks_value.is_object() {
        *hooks_value = serde_json::json!({});
    }
    let hooks_obj = hooks_value.as_object_mut().expect("just ensured object");

    let Some(ours_obj) = ours.as_object() else {
        return;
    };

    for (event, our_array) in ours_obj {
        let Some(our_entries) = our_array.as_array() else {
            continue;
        };

        let existing_value = hooks_obj
            .entry(event.clone())
            .or_insert_with(|| serde_json::json!([]));
        if !existing_value.is_array() {
            *existing_value = serde_json::json!([]);
        }
        let existing_array = existing_value.as_array_mut().expect("just ensured array");

        for our_entry in our_entries {
            let our_command = entry_command(our_entry);
            let already_present = existing_array
                .iter()
                .any(|existing_entry| entry_contains_command(existing_entry, our_command));
            if !already_present {
                existing_array.push(our_entry.clone());
            }
        }
    }
}

/// Extract the inner `hooks[0].command` string from a hook-event entry, if
/// present.
fn entry_command(entry: &serde_json::Value) -> Option<&str> {
    entry.get("hooks")?.get(0)?.get("command")?.as_str()
}

/// Whether any of `entry`'s inner `hooks[].command` values equal `cmd`. Unlike
/// `entry_command` (which only looks at `hooks[0]`), this scans the whole
/// inner `hooks` array — a user entry can have our command anywhere in it
/// (e.g. `hooks[1]`), and missing that would let a duplicate get appended.
fn entry_contains_command(entry: &serde_json::Value, cmd: Option<&str>) -> bool {
    let Some(hooks) = entry.get("hooks").and_then(|h| h.as_array()) else {
        return false;
    };
    hooks
        .iter()
        .any(|hook| hook.get("command").and_then(|c| c.as_str()) == cmd)
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

    // --- settings.json: deep-merge hooks ---
    let settings_path = safe_join(&base, "settings.json")?;
    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Failed to parse {}: {e}", settings_path.display()),
            )
        })?
    } else {
        serde_json::json!({})
    };

    merge_hooks(&mut settings, &tracevault_hooks());

    let formatted = serde_json::to_string_pretty(&settings)
        .map_err(|e| io::Error::other(format!("Failed to serialize settings: {e}")))?;
    fs::write(&settings_path, formatted)?;

    // --- CLAUDE.md: append instruction block, idempotently ---
    let claude_md_path = safe_join(&base, "CLAUDE.md")?;
    let existing = if claude_md_path.exists() {
        fs::read_to_string(&claude_md_path)?
    } else {
        String::new()
    };

    if !existing.contains(GLOBAL_CLAUDE_MD_MARKER) {
        let mut content = existing;
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(GLOBAL_CLAUDE_MD_BLOCK);
        fs::write(&claude_md_path, content)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn parse_github_org_ssh() {
        assert_eq!(
            parse_github_org("git@github.com:myorg/myrepo.git"),
            Some("myorg".into())
        );
    }

    #[test]
    fn parse_github_org_https() {
        assert_eq!(
            parse_github_org("https://github.com/myorg/myrepo"),
            Some("myorg".into())
        );
    }

    #[test]
    fn parse_github_org_non_github_returns_none() {
        assert_eq!(parse_github_org("https://gitlab.com/org/repo"), None);
    }

    #[test]
    fn parse_github_org_invalid() {
        assert_eq!(parse_github_org("not-a-url"), None);
    }

    #[test]
    fn merge_hooks_into_empty_settings_adds_all_our_events() {
        let mut settings = serde_json::json!({});
        let ours = tracevault_hooks();
        merge_hooks(&mut settings, &ours);

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
        merge_hooks(&mut settings, &ours);

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
        merge_hooks(&mut settings, &ours);
        merge_hooks(&mut settings, &ours);

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
    fn merge_hooks_dedupes_command_found_in_non_first_inner_hook() {
        // The existing PreToolUse entry has our command at hooks[1], not
        // hooks[0] — a naive `entry_command` (hooks[0] only) comparison would
        // miss it and append a duplicate entry.
        let mut settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Write|Edit|Bash",
                    "hooks": [
                        { "type": "command", "command": "other" },
                        { "type": "command", "command": "tracevault stream --event pre-tool-use" }
                    ]
                }]
            }
        });
        let ours = tracevault_hooks();
        merge_hooks(&mut settings, &ours);

        let pre_tool_use = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(
            pre_tool_use.len(),
            1,
            "must not append a duplicate PreToolUse entry when our command is present at hooks[1]"
        );
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
}
