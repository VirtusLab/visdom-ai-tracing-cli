//! `tracevault user-prompt` — the Claude Code `UserPromptSubmit` hook.
//!
//! Installed (sub-plan C) as the `UserPromptSubmit` hook for `init --global`.
//! Runs on every user prompt and, best-effort, reinforces the bound repo's
//! policies as `additionalContext` — but only when the effective repo has
//! changed since the last injection (tracked via `session_state`'s
//! `last_injected_repo`), so a long session isn't spammed with the same
//! policy text on every turn.
//!
//! This hook must NEVER hard-fail: a failing `UserPromptSubmit` hook blocks
//! the prompt from being submitted. Every fallible step is handled
//! internally, and the function always prints a valid `HookOutput` JSON
//! payload before returning `Ok(())`.

use std::io::Read as _;

use tracevault_protocol::hooks::parse_hook_event;

use crate::commands::session_hooks::{resolve_and_inject, HookOutput};

fn print_allow() -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", serde_json::to_string(&HookOutput::allow())?);
    Ok(())
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Read the hook event from stdin. A parse error must never fail the
    // hook — print the minimal allow response and return Ok.
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return print_allow();
    }
    let hook_event = match parse_hook_event(&input) {
        Ok(e) => e,
        Err(_) => return print_allow(),
    };

    // 2. Resolve the effective repo binding and, if the repo changed since
    // the last injection, inject its policies. Shared with `session-start`.
    resolve_and_inject("UserPromptSubmit", &hook_event).await
}
