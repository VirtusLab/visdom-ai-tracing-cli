use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::path::Path;

use tracevault_protocol::hooks::{parse_hook_event, HookResponse};
use tracevault_protocol::streaming::{
    extract_is_error_from_transcript, StreamEventRequest, StreamEventType,
};

/// Convert a loaded [`crate::context::Context`] into the three optional fields
/// that are stamped onto a [`StreamEventRequest`].
///
/// - `flow_id`  — taken directly from `ctx.flow_id`
/// - `labels`   — `None` when the vec is empty, `Some(vec)` otherwise
/// - `params`   — `None` when the map is empty, `Some(HashMap)` otherwise
///   (BTreeMap → HashMap conversion)
///
/// This is a pure function so it can be unit-tested without I/O.
#[allow(clippy::type_complexity)]
pub fn apply_context(
    ctx: crate::context::Context,
) -> (
    Option<String>,
    Option<Vec<String>>,
    Option<HashMap<String, String>>,
) {
    let flow_id = ctx.flow_id;
    let labels = if ctx.labels.is_empty() {
        None
    } else {
        Some(ctx.labels)
    };
    let params = if ctx.params.is_empty() {
        None
    } else {
        Some(ctx.params.into_iter().collect())
    };
    (flow_id, labels, params)
}

pub fn read_new_transcript_lines(
    transcript_path: &Path,
    offset_path: &Path,
) -> Result<(Vec<serde_json::Value>, i64), io::Error> {
    if !transcript_path.exists() {
        return Ok((vec![], 0));
    }

    let offset: i64 = if offset_path.exists() {
        let content = fs::read_to_string(offset_path)?;
        content
            .trim()
            .parse::<i64>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
    } else {
        0
    };

    let mut file = fs::File::open(transcript_path)?;
    file.seek(SeekFrom::Start(offset as u64))?;

    let reader = io::BufReader::new(file);
    let mut lines = Vec::new();
    let mut bytes_read = offset;

    for line_result in reader.lines() {
        let line = line_result?;
        // +1 for the newline character
        bytes_read += line.len() as i64 + 1;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            lines.push(value);
        }
    }

    Ok((lines, bytes_read))
}

pub fn append_pending(pending_path: &Path, json: &str) -> Result<(), io::Error> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(pending_path)?;
    writeln!(file, "{json}")?;
    Ok(())
}

pub fn drain_pending(pending_path: &Path) -> Result<Vec<String>, io::Error> {
    if !pending_path.exists() {
        return Ok(vec![]);
    }
    let content = fs::read_to_string(pending_path)?;
    let lines: Vec<String> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect();
    fs::remove_file(pending_path)?;
    Ok(lines)
}

/// Resolve the project root and session directory for a stream hook invocation.
///
/// This is the pure, testable core of `run_stream`'s path resolution. It uses
/// [`crate::paths::resolve_project_root`] which queries `git rev-parse
/// --git-common-dir` first, so it correctly resolves to the **primary**
/// `.tracevault/` directory from any worktree — including sibling linked
/// worktrees where the primary `.tracevault/` is not an ancestor of `hook_cwd`.
///
/// Returns `(resolved, session_dir)` — the resolved
/// [`crate::paths::ProjectRoot`] (so callers can inspect `.source`, e.g. to warn
/// on a Fallback that would create a stray `.tracevault/`) and `session_dir`
/// under `<resolved.root>/.tracevault/sessions/<session_id>/`.
pub fn resolve_session_paths(
    hook_cwd: &Path,
    session_id: &str,
) -> (crate::paths::ProjectRoot, std::path::PathBuf) {
    let resolved = crate::paths::resolve_project_root(hook_cwd);
    let session_dir = resolved
        .root
        .join(".tracevault")
        .join("sessions")
        .join(session_id);
    (resolved, session_dir)
}

pub async fn run_stream(event_type: &str) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Read HookEvent from stdin
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let hook_event = parse_hook_event(&input)?;

    // Resolve project_root and session_dir via the shared git-aware resolver.
    //
    // `resolve_session_paths` uses `git rev-parse --git-common-dir` to locate
    // the primary worktree root, so it works correctly from a primary checkout,
    // a nested worktree, AND a sibling linked worktree (where the primary
    // `.tracevault/` is NOT an ancestor of hook_cwd — the old ancestor-walk
    // would fall back to hook_cwd itself, fail to load config, and silently
    // drop the event).
    //
    // The hook must never hard-fail (a failing hook blocks the Claude Code tool
    // call).  Genuine resolution failure (no git, no `.tracevault/`) results in
    // a Fallback root (start dir).  Note: `fs::create_dir_all(&session_dir)?`
    // just below runs BEFORE the config/credentials check and can itself
    // `?`-exit early — that remains graceful because `main.rs` catches all
    // `Err` from `run_stream` and exits 0 without blocking the tool.
    let hook_cwd = Path::new(&hook_event.cwd);
    let (resolved, session_dir) = resolve_session_paths(hook_cwd, &hook_event.session_id);
    if resolved.source == crate::paths::ProjectRootSource::Fallback {
        // Neither git nor an ancestor `.tracevault/` resolved a project root, so
        // we are about to create a fresh `.tracevault/` at the hook's working
        // directory — which is how stray per-subdirectory `.tracevault/` dirs
        // appear. Surface it (best-effort; stderr never blocks the hook).
        eprintln!(
            "tracevault: warning: could not resolve a git project root from {}; \
             creating .tracevault/ there. Ensure `git` is on PATH for the hook and \
             that it runs inside the repository to keep sessions under the repo root.",
            hook_cwd.display()
        );
    }
    let project_root = resolved.root;

    // 2. Create session dir
    fs::create_dir_all(&session_dir)?;

    // Ensure runtime artifacts (sessions/, cache/, *.local.toml) are git-ignored
    // inside whatever `.tracevault/` we just created — including a per-subproject
    // one. `tracevault init` writes this for the root dir, but runtime-created
    // dirs would otherwise have no .gitignore and leak sessions into commits.
    // Best-effort: never fail the hook on this.
    let _ = crate::config::TracevaultConfig::ensure_gitignore(&project_root.join(".tracevault"));

    // Write origin marker so verify-start can disambiguate sessions by worktree.
    // Best-effort: never fail the hook on marker errors. Uses the shared
    // canonicalizing helper so the value matches what verify-start compares
    // against on the read side.
    let worktree_top = crate::paths::worktree_toplevel(hook_cwd);
    let _ = fs::write(session_dir.join("origin"), &worktree_top);

    // 3. Mint a time-ordered event id. UUIDv7 is stamped at hook-fire time, so
    //    it both orders events and is a stable idempotency key — no shared
    //    `.event_counter` file (which raced between concurrent parallel-tool
    //    hooks and could collide/drop events).
    let event_uuid = uuid::Uuid::now_v7();

    // 4. Read new transcript lines
    let transcript_path = Path::new(&hook_event.transcript_path);
    let offset_path = session_dir.join(".stream_offset");
    let (transcript_lines, new_offset) = read_new_transcript_lines(transcript_path, &offset_path)?;

    // 5. Build StreamEventRequest
    let stream_event_type = match event_type {
        "notification" => StreamEventType::SessionStart,
        "stop" => StreamEventType::SessionEnd,
        _ => StreamEventType::ToolUse,
    };

    // Extract is_error from transcript for this tool_use_id
    let tool_is_error = hook_event
        .tool_use_id
        .as_deref()
        .and_then(|uid| extract_is_error_from_transcript(uid, &transcript_lines));

    // Load the EFFECTIVE merged context (global merged with per-worktree, if any)
    // and extract fields before building the request.  Using `effective` means
    // parallel sessions in different linked worktrees each stamp their own
    // per-worktree context without interfering with each other.
    let ctx = crate::context::Context::effective(hook_cwd);
    let (ctx_flow_id, ctx_labels, ctx_params) = apply_context(ctx);

    let mut req = StreamEventRequest {
        protocol_version: 1,
        tool: Some("claude-code".to_string()),
        event_type: stream_event_type,
        session_id: hook_event.session_id.clone(),
        timestamp: chrono::Utc::now(),
        hook_event_name: Some(hook_event.hook_event_name.clone()),
        tool_name: hook_event.tool_name.clone(),
        tool_use_id: hook_event.tool_use_id.clone(),
        tool_input: hook_event.tool_input.clone(),
        tool_response: hook_event.tool_response.clone(),
        tool_is_error,
        event_index: None,
        event_uuid: Some(event_uuid),
        transcript_lines: if transcript_lines.is_empty() {
            None
        } else {
            Some(transcript_lines)
        },
        transcript_offset: Some(new_offset),
        model: None,
        cwd: Some(hook_event.cwd.clone()),
        final_stats: None,
        flow_id: ctx_flow_id,
        labels: ctx_labels,
        params: ctx_params,
    };

    req.truncate_large_fields();

    // 6. Resolve credentials
    let (server_url, token) = crate::api_client::resolve_credentials(&project_root);

    // 7. Load config for org_slug and repo_id
    let config =
        crate::config::TracevaultConfig::load(&project_root).ok_or("TracevaultConfig not found")?;
    let org_slug = config
        .org_slug
        .as_deref()
        .ok_or("org_slug not configured")?;
    let repo_id = config.repo_id.as_deref().ok_or("repo_id not configured")?;

    // 8. Create ApiClient
    let server_url = server_url.ok_or("server_url not configured")?;
    let client = crate::api_client::ApiClient::new(&server_url, token.as_deref());

    // 9. Try drain pending queue and send
    let pending_path = session_dir.join("pending.jsonl");
    let pending_events = drain_pending(&pending_path)?;

    let mut send_failed = false;

    // Send pending events first
    for pending_json in &pending_events {
        if let Ok(pending_req) = serde_json::from_str::<StreamEventRequest>(pending_json) {
            if client
                .stream_event(org_slug, repo_id, &pending_req)
                .await
                .is_err()
            {
                // Re-queue all remaining pending events
                for evt in &pending_events {
                    append_pending(&pending_path, evt)?;
                }
                send_failed = true;
                break;
            }
        }
    }

    // Send current event
    let req_json = serde_json::to_string(&req)?;
    if send_failed {
        append_pending(&pending_path, &req_json)?;
    } else {
        match client.stream_event(org_slug, repo_id, &req).await {
            Ok(_) => {
                // 10. On success update .stream_offset
                fs::write(&offset_path, new_offset.to_string())?;
            }
            Err(_) => {
                // 11. On failure append to pending.jsonl
                append_pending(&pending_path, &req_json)?;
            }
        }
    }

    // 12. Always print HookResponse::allow() to stdout
    let response = HookResponse::allow();
    println!("{}", serde_json::to_string(&response)?);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Context;
    use std::collections::BTreeMap;

    // ── apply_context: all three fields populated ─────────────────────────────

    #[test]
    fn apply_context_stamps_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut params = BTreeMap::new();
        params.insert("env".to_string(), "prod".to_string());
        params.insert("region".to_string(), "eu-west-1".to_string());

        let ctx = Context {
            flow_id: Some("flow-xyz".to_string()),
            labels: vec!["backend".to_string(), "urgent".to_string()],
            params,
        };

        let (flow_id, labels, params_out) = apply_context(ctx);

        assert_eq!(flow_id, Some("flow-xyz".to_string()));
        assert_eq!(
            labels,
            Some(vec!["backend".to_string(), "urgent".to_string()])
        );
        let p = params_out.expect("params should be Some");
        assert_eq!(p.get("env").map(String::as_str), Some("prod"));
        assert_eq!(p.get("region").map(String::as_str), Some("eu-west-1"));

        // Verify the context file round-trips through save_to → load_from → apply_context.
        let written = Context {
            flow_id: Some("flow-xyz".to_string()),
            labels: vec!["backend".to_string(), "urgent".to_string()],
            params: {
                let mut m = BTreeMap::new();
                m.insert("env".to_string(), "prod".to_string());
                m.insert("region".to_string(), "eu-west-1".to_string());
                m
            },
        };
        let ctx_path = dir.path().join(".tracevault").join("context.json");
        written.save_to(&ctx_path).unwrap();
        let loaded = Context::load_from(&ctx_path);
        let (flow_id2, labels2, params2) = apply_context(loaded);
        assert_eq!(flow_id2, Some("flow-xyz".to_string()));
        assert!(labels2.is_some());
        assert!(params2.is_some());
    }

    // ── apply_context: missing context file → all None ────────────────────────

    #[test]
    fn apply_context_missing_file_all_none() {
        let dir = tempfile::tempdir().unwrap();
        let missing_path = dir.path().join(".tracevault").join("context.json");
        let ctx = Context::load_from(&missing_path); // no context.json → default
        let (flow_id, labels, params) = apply_context(ctx);
        assert!(flow_id.is_none(), "flow_id should be None");
        assert!(labels.is_none(), "labels should be None");
        assert!(params.is_none(), "params should be None");
    }

    // ── apply_context: empty labels vec / empty params map → None ─────────────

    #[test]
    fn apply_context_empty_collections_are_none() {
        let ctx = Context {
            flow_id: None,
            labels: vec![],
            params: BTreeMap::new(),
        };
        let (flow_id, labels, params) = apply_context(ctx);
        assert!(flow_id.is_none());
        assert!(
            labels.is_none(),
            "empty labels should be None, not Some([])"
        );
        assert!(
            params.is_none(),
            "empty params should be None, not Some({{}})"
        );
    }

    // ── apply_context: flow_id only, collections empty ────────────────────────

    #[test]
    fn apply_context_flow_id_only() {
        let ctx = Context {
            flow_id: Some("my-flow".to_string()),
            labels: vec![],
            params: BTreeMap::new(),
        };
        let (flow_id, labels, params) = apply_context(ctx);
        assert_eq!(flow_id, Some("my-flow".to_string()));
        assert!(labels.is_none());
        assert!(params.is_none());
    }

    // ── resolve_session_paths tests ───────────────────────────────────────────
    //
    // These tests verify that `resolve_session_paths` routes through the
    // git-aware resolver so that sibling linked worktrees capture events under
    // the PRIMARY `.tracevault/sessions/`, not the worktree directory.

    use crate::test_helpers::{add_worktree, init_git_repo};

    /// Primary checkout: project_root resolves to the repo root; session_dir is
    /// under `<repo>/.tracevault/sessions/<id>/`.
    #[test]
    fn resolve_session_paths_primary_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);
        // Create a .tracevault/ in the primary root to match a real init.
        std::fs::create_dir_all(repo.join(".tracevault")).unwrap();

        let (resolved, session_dir) = resolve_session_paths(&repo, "sess-primary-123");
        let project_root = resolved.root;

        assert_eq!(
            project_root.canonicalize().unwrap(),
            repo.canonicalize().unwrap(),
            "primary checkout: project_root must be repo root"
        );
        assert_eq!(
            session_dir.canonicalize().unwrap_or(session_dir.clone()),
            repo.join(".tracevault")
                .join("sessions")
                .join("sess-primary-123"),
            "primary checkout: session_dir must be <repo>/.tracevault/sessions/<id>/"
        );
        // session_dir must be INSIDE the primary repo, not elsewhere.
        assert!(
            session_dir.starts_with(&repo),
            "session_dir must be under the primary repo root"
        );
    }

    /// Sibling linked worktree: `hook_cwd` is OUTSIDE the primary repo tree.
    /// The old ancestor-walk would fall back to `hook_cwd` itself; the new
    /// git-aware resolver must return the PRIMARY root.
    #[test]
    fn resolve_session_paths_sibling_worktree_uses_primary_root() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        // Sibling worktree lives outside the primary repo directory.
        let wt = tmp.path().join("sibling-wt");

        std::fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);
        // Place .tracevault/ ONLY in the primary repo — not in the worktree.
        std::fs::create_dir_all(repo.join(".tracevault")).unwrap();
        add_worktree(&repo, &wt);

        let (resolved, session_dir) = resolve_session_paths(&wt, "sess-sibling-456");
        let project_root = resolved.root;

        // Must resolve to the PRIMARY repo root, not the sibling worktree dir.
        assert_eq!(
            project_root.canonicalize().unwrap(),
            repo.canonicalize().unwrap(),
            "sibling worktree: project_root must be primary repo root (not worktree dir)"
        );
        // session_dir must be inside the PRIMARY .tracevault/, not the worktree.
        assert!(
            session_dir.starts_with(&repo),
            "sibling worktree: session_dir must be under the PRIMARY repo root"
        );
        assert!(
            !session_dir.starts_with(&wt),
            "sibling worktree: session_dir must NOT be under the sibling worktree dir"
        );
        assert!(
            session_dir.ends_with(
                std::path::Path::new(".tracevault")
                    .join("sessions")
                    .join("sess-sibling-456")
            ),
            "session_dir must end with .tracevault/sessions/<session_id>"
        );
    }

    /// Non-git directory with no `.tracevault/` ancestor: project_root falls
    /// back to `hook_cwd` itself (Fallback source); the function must not panic.
    #[test]
    fn resolve_session_paths_non_git_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        // No git repo, no .tracevault/ anywhere.
        let (resolved, session_dir) = resolve_session_paths(tmp.path(), "sess-fallback-789");
        let project_root = resolved.root;

        assert_eq!(
            project_root,
            tmp.path(),
            "non-git: project_root must fall back to hook_cwd"
        );
        assert_eq!(
            session_dir,
            tmp.path()
                .join(".tracevault")
                .join("sessions")
                .join("sess-fallback-789"),
            "non-git: session_dir must be relative to the fallback root"
        );
    }

    /// The origin marker written by run_stream contains the worktree toplevel.
    /// This test exercises the same `paths::worktree_toplevel` helper the hook
    /// uses (can't call run_stream end-to-end without a real server) so the
    /// marker content is computed and canonicalized exactly as in production.
    #[test]
    fn origin_marker_written_in_session_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);

        let session_dir = repo
            .join(".tracevault")
            .join("sessions")
            .join("test-sess-001");
        std::fs::create_dir_all(&session_dir).unwrap();

        let hook_cwd = &repo;
        let worktree_top = crate::paths::worktree_toplevel(hook_cwd);
        let _ = std::fs::write(session_dir.join("origin"), &worktree_top);

        let origin_content = std::fs::read_to_string(session_dir.join("origin")).unwrap();
        let expected = repo.canonicalize().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            origin_content.trim(),
            expected.as_str(),
            "origin marker must contain the canonicalized worktree toplevel"
        );
    }
}
