//! Shared helpers for the SessionStart / UserPromptSubmit hook commands
//! (sub-plan C): the Claude Code hook-output JSON shape for injecting context,
//! the pure decision logic for when to inject, and the shared
//! resolve-binding-then-inject flow used by both hook commands.

use std::path::Path;

use serde::Serialize;

use tracevault_protocol::hooks::HookEvent;

use crate::api_client::resolve_credentials;
use crate::resolution::{binding_from_config, effective_binding, ResolveInputs};

/// Claude Code hook output. Field names are camelCase to match CC's contract
/// (the shared protocol `HookResponse` is snake_case and server-facing; this is
/// the CLI-local shape used only for hook stdout).
#[derive(Debug, Serialize)]
pub struct HookOutput {
    #[serde(rename = "hookSpecificOutput", skip_serializing_if = "Option::is_none")]
    pub hook_specific_output: Option<HookSpecificOutput>,
    #[serde(rename = "continue", skip_serializing_if = "Option::is_none")]
    pub cont: Option<bool>,
    #[serde(rename = "suppressOutput", skip_serializing_if = "Option::is_none")]
    pub suppress_output: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct HookSpecificOutput {
    #[serde(rename = "hookEventName")]
    pub hook_event_name: String,
    #[serde(rename = "additionalContext")]
    pub additional_context: String,
}

impl HookOutput {
    /// Minimal allow/quiet response with no injected context.
    pub fn allow() -> Self {
        Self {
            hook_specific_output: None,
            cont: None,
            suppress_output: Some(true),
        }
    }
    /// Allow + inject `context` under the given hook event name.
    pub fn with_context(event: &str, context: String) -> Self {
        Self {
            hook_specific_output: Some(HookSpecificOutput {
                hook_event_name: event.to_string(),
                additional_context: context,
            }),
            cont: None,
            suppress_output: Some(true),
        }
    }
}

/// Maximum additionalContext size (CC guidance). Larger → replaced with a notice.
const MAX_CONTEXT: usize = 10_000;

/// Cap injected context; oversize policy text is replaced with a short pointer.
pub fn cap_context(s: String) -> String {
    if s.chars().count() > MAX_CONTEXT {
        "TraceVault policies are too large to inline here — run `tracevault agent-policies` to see them.".to_string()
    } else {
        s
    }
}

/// Whether to inject policies for this hook event.
/// - SessionStart: inject whenever a repo is bound (refresh context at start/resume).
/// - UserPromptSubmit: inject only when the effective repo changed since last injection.
/// - other events: never.
pub fn should_inject(
    event: &str,
    effective_repo_id: Option<&str>,
    last_injected: Option<&str>,
) -> bool {
    match event {
        "SessionStart" => effective_repo_id.is_some(),
        "UserPromptSubmit" => effective_repo_id.is_some() && effective_repo_id != last_injected,
        _ => false,
    }
}

fn print_allow() -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", serde_json::to_string(&HookOutput::allow())?);
    Ok(())
}

/// Resolve the session's effective binding and, if `should_inject(event, ...)`,
/// best-effort fetch + print policies as `additionalContext` and persist
/// `last_injected_repo`. Always prints a valid `HookOutput`; never hard-fails.
///
/// Shared by `session-start` and `user-prompt`: both need the same
/// resolve-binding → decide → fetch-and-inject flow, differing only in which
/// hook-specific steps happen around it (SessionStart also writes the session
/// env file, which stays in `session_start.rs`).
pub async fn resolve_and_inject(
    event: &str,
    hook_event: &HookEvent,
) -> Result<(), Box<dyn std::error::Error>> {
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

    if !should_inject(
        event,
        binding.as_ref().map(|b| b.repo_id.as_str()),
        session.last_injected_repo.as_deref(),
    ) {
        return print_allow();
    }
    // `should_inject` returning true requires `effective_repo_id.is_some()`, so
    // `binding` is always `Some` here.
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
    let client = crate::api_client::ApiClient::new(&server_url, token.as_deref());

    match client
        .get_agent_instructions(&binding.org_slug, &repo_uuid)
        .await
    {
        Ok(resp) => {
            let output = HookOutput::with_context(event, cap_context(resp.content));
            println!("{}", serde_json::to_string(&output)?);
            // Best-effort: persist the last-injected repo so a later
            // UserPromptSubmit can avoid redundant re-injection. A save
            // failure must not turn this into a hook failure.
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

    // ── should_inject ────────────────────────────────────────────────────

    #[test]
    fn session_start_injects_when_bound() {
        assert!(should_inject("SessionStart", Some("repo-1"), None));
    }

    #[test]
    fn session_start_skips_when_unbound() {
        assert!(!should_inject("SessionStart", None, None));
    }

    #[test]
    fn session_start_injects_even_if_same_as_last_injected() {
        // SessionStart always refreshes, regardless of last_injected.
        assert!(should_inject(
            "SessionStart",
            Some("repo-1"),
            Some("repo-1")
        ));
    }

    #[test]
    fn user_prompt_submit_injects_when_repo_changed() {
        assert!(should_inject(
            "UserPromptSubmit",
            Some("repo-2"),
            Some("repo-1")
        ));
    }

    #[test]
    fn user_prompt_submit_injects_when_no_prior_injection() {
        assert!(should_inject("UserPromptSubmit", Some("repo-1"), None));
    }

    #[test]
    fn user_prompt_submit_skips_when_repo_unchanged() {
        assert!(!should_inject(
            "UserPromptSubmit",
            Some("repo-1"),
            Some("repo-1")
        ));
    }

    #[test]
    fn user_prompt_submit_skips_when_unbound() {
        assert!(!should_inject("UserPromptSubmit", None, None));
        assert!(!should_inject("UserPromptSubmit", None, Some("repo-1")));
    }

    #[test]
    fn unknown_event_never_injects() {
        assert!(!should_inject("SomeOtherEvent", Some("repo-1"), None));
        assert!(!should_inject("SomeOtherEvent", None, None));
    }

    // ── cap_context ──────────────────────────────────────────────────────

    #[test]
    fn cap_context_leaves_short_string_unchanged() {
        let s = "short policy text".to_string();
        assert_eq!(cap_context(s.clone()), s);
    }

    #[test]
    fn cap_context_replaces_oversize_string_with_notice() {
        let s = "a".repeat(MAX_CONTEXT + 1);
        let capped = cap_context(s);
        assert_eq!(
            capped,
            "TraceVault policies are too large to inline here — run `tracevault agent-policies` to see them."
        );
    }

    #[test]
    fn cap_context_allows_exactly_max_context() {
        let s = "a".repeat(MAX_CONTEXT);
        assert_eq!(cap_context(s.clone()), s);
    }

    // ── HookOutput serialization ─────────────────────────────────────────

    #[test]
    fn with_context_serializes_expected_shape() {
        let out = HookOutput::with_context("SessionStart", "x".to_string());
        let json = serde_json::to_string(&out).unwrap();
        assert!(json.contains("\"hookSpecificOutput\""));
        assert!(json.contains("\"hookEventName\":\"SessionStart\""));
        assert!(json.contains("\"additionalContext\":\"x\""));
    }

    #[test]
    fn allow_serializes_without_hook_specific_output() {
        let out = HookOutput::allow();
        let json = serde_json::to_string(&out).unwrap();
        assert!(!json.contains("hookSpecificOutput"));
        assert!(json.contains("\"suppressOutput\":true"));
    }

    // ── resolve_and_inject ───────────────────────────────────────────────
    //
    // `resolve_and_inject` is network/IO-bound, so coverage here stays thin:
    // this exercises the no-binding path (empty session state, no
    // `.tracevault/config.toml`, non-git cwd), which `should_inject` already
    // proves resolves to "don't inject" — so this must return `Ok(())`
    // without ever reaching the network. No env mutation: `cwd` is an
    // isolated tempdir and the session id is unique to this test.

    #[tokio::test]
    async fn resolve_and_inject_allows_when_no_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let hook_event = HookEvent {
            session_id: "resolve-and-inject-no-binding-test".to_string(),
            transcript_path: "t".to_string(),
            cwd: tmp.path().to_string_lossy().into_owned(),
            permission_mode: None,
            hook_event_name: "UserPromptSubmit".to_string(),
            tool_name: None,
            tool_input: None,
            tool_response: None,
            tool_use_id: None,
            source: None,
        };

        let result = resolve_and_inject("UserPromptSubmit", &hook_event).await;
        assert!(result.is_ok());
    }
}
