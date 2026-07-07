//! `tracevault session-start` — the Claude Code `SessionStart` hook.
//!
//! Installed (sub-plan C) as the `SessionStart` hook for `init --global`. Runs
//! on every session start/resume and does two things, both best-effort:
//!
//! 1. Exports `TRACEVAULT_SESSION_ID` into Claude Code's session env file (when
//!    present) so agent-invoked `tracevault repo …` commands can find the
//!    session without an explicit `--session-id` flag.
//! 2. Resolves the effective repo binding (workspace-mode precedence chain)
//!    and, if one exists, injects that repo's policies as `SessionStart`
//!    `additionalContext` — the reliable way to hand an agent up-to-date
//!    policy text at the start of a session.
//!
//! This hook must NEVER hard-fail: a failing `SessionStart` hook blocks
//! Claude Code from starting the session. Every fallible step below is
//! handled internally, and the function always prints a valid `HookOutput`
//! JSON payload before returning `Ok(())`.

use std::io::Read as _;
use std::path::Path;

use tracevault_protocol::hooks::parse_hook_event;

use crate::api_client::{resolve_credentials, ApiClient};
use crate::commands::session_hooks::{cap_context, should_inject, HookOutput};
use crate::resolution::{binding_from_config, effective_binding, ResolveInputs};

/// Append `export TRACEVAULT_SESSION_ID=<session_id>` to the Claude Code
/// session env file. Pure I/O helper factored out so it can be unit-tested
/// without touching the real `CLAUDE_ENV_FILE` env var. Creates the file if it
/// doesn't exist; appends (never truncates) so it composes with whatever else
/// writes to the same env file.
pub fn write_session_env(env_file: &Path, session_id: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(env_file)?;
    writeln!(file, "export TRACEVAULT_SESSION_ID={session_id}")
}

fn print_allow() -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", serde_json::to_string(&HookOutput::allow())?);
    Ok(())
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Read the hook event from stdin. A parse error must never fail the
    // hook — print the minimal allow response and return Ok.
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let hook_event = match parse_hook_event(&input) {
        Ok(e) => e,
        Err(_) => return print_allow(),
    };

    // 2. Export the session id, best-effort. Older Claude Code versions don't
    // set CLAUDE_ENV_FILE — silently skip in that case.
    if let Ok(env_file) = std::env::var("CLAUDE_ENV_FILE") {
        let _ = write_session_env(Path::new(&env_file), &hook_event.session_id);
    }

    // 3. Resolve the effective repo binding via the same workspace-mode
    // precedence chain the stream hook uses.
    let hook_cwd = Path::new(&hook_event.cwd);
    let project_root = crate::paths::resolve_project_root(hook_cwd).root;
    let session = crate::session_state::load(&hook_event.session_id);
    let worktree = crate::paths::worktree_toplevel(hook_cwd);
    let bound =
        crate::config::TracevaultConfig::load(&project_root).and_then(|c| binding_from_config(&c));
    let binding = effective_binding(ResolveInputs {
        repo_flag: None,
        session: &session,
        worktree_path: Some(&worktree),
        bound,
    })
    .map(|(b, _)| b);

    // 4. Inject policies, best-effort, only when a repo is bound.
    if !should_inject(
        "SessionStart",
        binding.as_ref().map(|b| b.repo_id.as_str()),
        session.last_injected_repo.as_deref(),
    ) {
        return print_allow();
    }
    // `should_inject` returning true for "SessionStart" requires
    // `effective_repo_id.is_some()`, so `binding` is always `Some` here.
    let Some(binding) = binding else {
        return print_allow();
    };

    let Ok(repo_uuid) = binding.repo_id.parse::<uuid::Uuid>() else {
        // A corrupted/hand-edited session-state or config file could contain a
        // non-UUID repo_id — skip injection rather than fail the hook.
        return print_allow();
    };

    let (server_url, token) = resolve_credentials(&project_root);
    let Some(server_url) = server_url else {
        return print_allow();
    };
    let client = ApiClient::new(&server_url, token.as_deref());

    match client
        .get_agent_instructions(&binding.org_slug, &repo_uuid)
        .await
    {
        Ok(resp) => {
            let output = HookOutput::with_context("SessionStart", cap_context(resp.content));
            println!("{}", serde_json::to_string(&output)?);
            // Best-effort: persist the last-injected repo so UserPromptSubmit
            // (a later task) can avoid redundant re-injection. A save failure
            // must not turn this into a hook failure.
            let mut session = session;
            session.last_injected_repo = Some(binding.repo_id.clone());
            let _ = crate::session_state::save(&hook_event.session_id, &session);
            Ok(())
        }
        Err(_) => print_allow(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_session_env_appends_export_line() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join("env.sh");

        write_session_env(&env_file, "sess-123").unwrap();

        let content = std::fs::read_to_string(&env_file).unwrap();
        assert_eq!(content, "export TRACEVAULT_SESSION_ID=sess-123\n");
    }

    #[test]
    fn write_session_env_appends_on_repeated_calls() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join("env.sh");

        write_session_env(&env_file, "sess-a").unwrap();
        write_session_env(&env_file, "sess-b").unwrap();

        let content = std::fs::read_to_string(&env_file).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(
            lines,
            vec![
                "export TRACEVAULT_SESSION_ID=sess-a",
                "export TRACEVAULT_SESSION_ID=sess-b",
            ]
        );
    }

    #[test]
    fn write_session_env_creates_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join("nested").join("env.sh");
        // Parent dir does not exist — OpenOptions::create only creates the
        // file itself, not parent dirs, so this must error rather than panic.
        assert!(write_session_env(&env_file, "sess-x").is_err());
    }
}
