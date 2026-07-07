//! Shared helpers for the SessionStart / UserPromptSubmit hook commands
//! (sub-plan C): the Claude Code hook-output JSON shape for injecting context,
//! and the pure decision logic for when to inject.

use serde::Serialize;

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
}
