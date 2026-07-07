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

use std::path::Path;

use crate::commands::session_hooks::{read_hook_event_or_allow, resolve_and_inject};

/// Append `export TRACEVAULT_SESSION_ID='<session_id>'` to the Claude Code
/// session env file, unless that exact line is already present. Pure I/O
/// helper factored out so it can be unit-tested without touching the real
/// `CLAUDE_ENV_FILE` env var. Creates the file if it doesn't exist.
///
/// `SessionStart` fires on startup *and* on resume/clear/compact — all with
/// the same session id — so an unconditional append would grow the env file
/// with duplicate export lines every time. Reading the file first and
/// skipping an already-present line makes the write idempotent.
///
/// `session_id` is untrusted input from the hook event JSON, and this file is
/// later `source`d by the shell — a naive unquoted interpolation would be a
/// shell-injection sink. Defend in depth: reject anything outside the
/// existing session-id allowlist (`[A-Za-z0-9_-]`, non-empty) by silently
/// skipping the write (best-effort: a malformed id is simply never
/// exported), and single-quote the value we do write. After the allowlist
/// check there are no quote or metacharacters left, so single-quoting closes
/// the sink.
pub fn write_session_env(env_file: &Path, session_id: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    if !crate::session_state::is_safe_session_id(session_id) {
        return Ok(());
    }
    let line = format!("export TRACEVAULT_SESSION_ID='{session_id}'");

    if let Ok(existing) = std::fs::read_to_string(env_file) {
        if existing.lines().any(|l| l == line) {
            return Ok(());
        }
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(env_file)?;
    writeln!(file, "{line}")
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Read the hook event from stdin. A parse error must never fail the
    // hook — print the minimal allow response and return Ok.
    let Some(hook_event) = read_hook_event_or_allow() else {
        return Ok(());
    };

    // 2. Export the session id, best-effort. Older Claude Code versions don't
    // set CLAUDE_ENV_FILE — silently skip in that case.
    if let Ok(env_file) = std::env::var("CLAUDE_ENV_FILE") {
        let _ = write_session_env(Path::new(&env_file), &hook_event.session_id);
    }

    // 3. Resolve the effective repo binding and, if warranted, inject its
    // policies. Shared with `user-prompt` (sub-plan C's UserPromptSubmit hook).
    resolve_and_inject(&hook_event).await
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
        assert_eq!(content, "export TRACEVAULT_SESSION_ID='sess-123'\n");
    }

    #[test]
    fn write_session_env_appends_on_repeated_calls_with_different_ids() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join("env.sh");

        write_session_env(&env_file, "sess-a").unwrap();
        write_session_env(&env_file, "sess-b").unwrap();

        let content = std::fs::read_to_string(&env_file).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(
            lines,
            vec![
                "export TRACEVAULT_SESSION_ID='sess-a'",
                "export TRACEVAULT_SESSION_ID='sess-b'",
            ]
        );
    }

    #[test]
    fn write_session_env_is_idempotent_for_repeated_calls_with_same_id() {
        // SessionStart fires on startup, resume, clear, AND compact — all
        // with the SAME session id — so repeated calls for the same id must
        // not grow the file with duplicate export lines.
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join("env.sh");

        write_session_env(&env_file, "sess-a").unwrap();
        write_session_env(&env_file, "sess-a").unwrap();
        write_session_env(&env_file, "sess-a").unwrap();

        let content = std::fs::read_to_string(&env_file).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines, vec!["export TRACEVAULT_SESSION_ID='sess-a'"]);
    }

    #[test]
    fn write_session_env_creates_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join("nested").join("env.sh");
        // Parent dir does not exist — OpenOptions::create only creates the
        // file itself, not parent dirs, so this must error rather than panic.
        assert!(write_session_env(&env_file, "sess-x").is_err());
    }

    #[test]
    fn write_session_env_quotes_valid_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join("env.sh");
        let uuid = "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";

        write_session_env(&env_file, uuid).unwrap();

        let content = std::fs::read_to_string(&env_file).unwrap();
        assert_eq!(content, format!("export TRACEVAULT_SESSION_ID='{uuid}'\n"));
    }

    #[test]
    fn write_session_env_skips_unsafe_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join("env.sh");

        // Shell-metacharacter / path-traversal ids must never be written.
        write_session_env(&env_file, "x; rm -rf /").unwrap();
        write_session_env(&env_file, "../evil").unwrap();

        // Nothing was written — the file wasn't even created for the first
        // call, and the second call didn't add anything either.
        match std::fs::read_to_string(&env_file) {
            Ok(content) => assert!(content.is_empty()),
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
        }
    }
}
