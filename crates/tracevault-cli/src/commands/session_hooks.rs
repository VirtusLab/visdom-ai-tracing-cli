//! Shared helpers for the SessionStart / UserPromptSubmit hook commands
//! (sub-plan C): the Claude Code hook-output JSON shape for injecting context,
//! the pure decision logic for when to inject, and the shared
//! resolve-binding-then-inject flow used by both hook commands.

use std::io::Read as _;
use std::path::Path;

use serde::Serialize;

use tracevault_protocol::hooks::{parse_hook_event, HookEvent};

use crate::api_client::{resolve_credentials, ApiClient};
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

/// Maximum additionalContext size (CC guidance). Larger → truncated.
const MAX_CONTEXT: usize = 10_000;

/// How much of an oversize policy text to keep before appending the
/// truncation suffix. Chosen so `HEAD_LEN` + the suffix stays under
/// `MAX_CONTEXT`.
const HEAD_LEN: usize = 9_500;

/// Cap injected context. Oversize policy text is truncated to its first
/// `HEAD_LEN` chars with a suffix pointing at `tracevault repo status` —
/// unlike `tracevault agent-policies`, `repo status` works in workspace mode
/// (no `.tracevault/config.toml` required), so the fallback is reachable in
/// every install mode.
pub fn cap_context(s: String) -> String {
    if s.chars().count() > MAX_CONTEXT {
        let head: String = s.chars().take(HEAD_LEN).collect();
        format!("{head}\n\n…(truncated; run `tracevault repo status` to see full policies)")
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

/// Print the minimal allow/quiet `HookOutput` and return `Ok(())`. Shared by
/// every hook command's error paths — a hook must always emit valid JSON,
/// even (especially) when something upstream failed.
pub(crate) fn print_allow() -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", serde_json::to_string(&HookOutput::allow())?);
    Ok(())
}

/// Read stdin, parse it as a `HookEvent`, and return it. On any read or parse
/// error, print the allow response (never fail the hook) and return `None`.
/// Shared by `session-start` and `user-prompt`, whose `run()` bodies both
/// start with "read the hook event or bail out allowing".
pub(crate) fn read_hook_event_or_allow() -> Option<HookEvent> {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        let _ = print_allow();
        return None;
    }
    match parse_hook_event(&input) {
        Ok(e) => Some(e),
        Err(_) => {
            let _ = print_allow();
            None
        }
    }
}

/// Fetch rendered policies and build the injection output; `None` on ANY
/// error (best-effort — a fetch failure degrades to allow, never blocks the
/// hook). Factored out of `resolve_and_inject` so the network step can be
/// exercised in a unit test without touching creds/env/git.
async fn fetch_context(
    client: &ApiClient,
    event: &str,
    org_slug: &str,
    repo_id: uuid::Uuid,
) -> Option<HookOutput> {
    let resp = client
        .get_agent_instructions(org_slug, &repo_id)
        .await
        .ok()?;
    Some(HookOutput::with_context(event, cap_context(resp.content)))
}

/// Resolve the session's effective binding and, if `should_inject(event, ...)`,
/// best-effort fetch + print policies as `additionalContext` and persist
/// `last_injected_repo`. Always prints a valid `HookOutput`; never hard-fails.
///
/// Shared by `session-start` and `user-prompt`: both need the same
/// resolve-binding → decide → fetch-and-inject flow, differing only in which
/// hook-specific steps happen around it (SessionStart also writes the session
/// env file, which stays in `session_start.rs`).
pub async fn resolve_and_inject(hook_event: &HookEvent) -> Result<(), Box<dyn std::error::Error>> {
    let event = hook_event.hook_event_name.as_str();
    let hook_cwd = Path::new(&hook_event.cwd);
    let project_root = crate::paths::resolve_project_root(hook_cwd).root;
    let session = crate::session_state::load(&hook_event.session_id);
    let worktree = crate::paths::worktree_toplevel(hook_cwd);
    let bound =
        crate::config::TracevaultConfig::load(&project_root).and_then(|c| binding_from_config(&c));
    let user_default = crate::user_default::load();
    let binding = effective_binding(ResolveInputs {
        repo_flag: None,
        session: &session,
        worktree_path: Some(&worktree),
        bound,
        user_default,
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
    let client = ApiClient::new(&server_url, token.as_deref());

    match fetch_context(&client, event, &binding.org_slug, repo_uuid).await {
        Some(output) => {
            println!("{}", serde_json::to_string(&output)?);
            // Best-effort: persist the last-injected repo so a later
            // UserPromptSubmit can avoid redundant re-injection. A save
            // failure must not turn this into a hook failure. Reload the
            // session state right before saving (rather than reusing the
            // snapshot loaded before the network `await` above) so a
            // concurrent `repo switch`/`reset` that happened during the
            // fetch isn't silently reverted by this save.
            let mut fresh = crate::session_state::load(&hook_event.session_id);
            fresh.last_injected_repo = Some(binding.repo_id.clone());
            let _ = crate::session_state::save(&hook_event.session_id, &fresh);
            Ok(())
        }
        None => print_allow(),
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
    fn cap_context_truncates_oversize_string_and_points_at_repo_status() {
        let s = "a".repeat(MAX_CONTEXT + 1);
        let capped = cap_context(s.clone());
        // Head is preserved (truncated policy text is still useful context)…
        assert!(capped.starts_with(&"a".repeat(HEAD_LEN)));
        // …and the fallback points at a command that works without
        // `.tracevault/config.toml` (workspace mode), unlike
        // `tracevault agent-policies`.
        assert!(capped.contains("tracevault repo status"));
        assert!(!capped.contains("agent-policies"));
        assert!(capped.chars().count() <= MAX_CONTEXT);
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

        let result = resolve_and_inject(&hook_event).await;
        assert!(result.is_ok());
    }

    // ── fetch_context ────────────────────────────────────────────────────
    //
    // `resolve_and_inject` can't be unit-tested end-to-end without real
    // creds/env/git, but the network step it delegates to (`fetch_context`)
    // can be: point an `ApiClient` at a port nothing listens on so the
    // request fails fast with connection-refused, and assert the failure
    // degrades to `None` (→ `print_allow()` in the caller) rather than
    // propagating an error. This proves the hook never blocks on a fetch
    // failure.

    #[tokio::test]
    async fn fetch_context_returns_none_on_network_failure() {
        let client = ApiClient::new("http://127.0.0.1:1", None);
        let repo_id = uuid::Uuid::new_v4();
        let result = fetch_context(&client, "SessionStart", "org", repo_id).await;
        assert!(result.is_none());
    }
}
