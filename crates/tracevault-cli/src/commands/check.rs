use crate::api_client::{resolve_credentials, ApiClient, CheckPoliciesRequest, SessionCheckData};
use crate::resolution::{resolve_repo_by_name, ResolveRepoByNameError};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::Command;

fn git_head_sha(project_root: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(project_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

fn collect_session_data(session_dir: &Path) -> Option<SessionCheckData> {
    let session_id = session_dir.file_name()?.to_string_lossy().to_string();

    // Read events.jsonl for files_modified
    let events_path = session_dir.join("events.jsonl");
    let mut files_modified = Vec::new();
    let mut files_seen = HashSet::new();

    if events_path.exists() {
        if let Ok(content) = fs::read_to_string(&events_path) {
            for line in content.lines() {
                let event: serde_json::Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(path) = event
                    .get("tool_input")
                    .and_then(|v| v.get("file_path"))
                    .and_then(|v| v.as_str())
                {
                    if files_seen.insert(path.to_string()) {
                        files_modified.push(path.to_string());
                    }
                }
            }
        }
    }

    // Read transcript for tool_calls
    let meta_path = session_dir.join("metadata.json");
    let metadata: Option<serde_json::Value> = meta_path
        .exists()
        .then(|| fs::read_to_string(&meta_path).ok())
        .flatten()
        .and_then(|c| serde_json::from_str(&c).ok());

    let transcript_path = metadata
        .as_ref()
        .and_then(|m| m.get("transcript_path"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let mut tool_calls_map: std::collections::HashMap<String, i32> =
        std::collections::HashMap::new();
    let mut total_tool_calls: i32 = 0;

    if let Some(path) = &transcript_path {
        if let Ok(content) = fs::read_to_string(path) {
            for line in content.lines() {
                let entry: serde_json::Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if entry.get("type").and_then(|v| v.as_str()) == Some("assistant") {
                    if let Some(content_arr) = entry
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_array())
                    {
                        for block in content_arr {
                            if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                                if let Some(name) = block.get("name").and_then(|v| v.as_str()) {
                                    *tool_calls_map.entry(name.to_string()).or_insert(0) += 1;
                                    total_tool_calls += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let tool_calls = if tool_calls_map.is_empty() {
        None
    } else {
        serde_json::to_value(&tool_calls_map).ok()
    };

    Some(SessionCheckData {
        session_id,
        tool_calls,
        files_modified: if files_modified.is_empty() {
            None
        } else {
            Some(files_modified)
        },
        total_tool_calls: if total_tool_calls > 0 {
            Some(total_tool_calls)
        } else {
            None
        },
    })
}

/// Select the session directories that belong to the current worktree.
/// Keeps sessions whose `origin` marker == `worktree_top` OR that have no
/// marker (legacy/unmarked — kept conservatively). Marked sessions from other
/// worktrees are excluded.
///
/// Safety rail: if filtering would drop EVERY session while sessions exist,
/// return all of them with `fell_back = true` — never under-enforce by sending
/// zero sessions for a push that has unpushed work.
///
/// `pub` so the integration test in Task 7 can drive it.
pub fn select_worktree_sessions(
    session_dirs: Vec<std::path::PathBuf>,
    worktree_top: &str,
) -> (Vec<std::path::PathBuf>, bool) {
    let belongs = |dir: &std::path::Path| -> bool {
        match fs::read_to_string(dir.join("origin")) {
            Ok(s) => s.trim() == worktree_top, // marked: keep only if mine
            // Only a genuinely-absent marker counts as unmarked/legacy (keep).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            // Marker present but unreadable (permissions/IO): we can't prove it's
            // ours, so exclude it rather than risk cross-worktree interference.
            // The safety rail below still prevents under-enforcement if this
            // drops everything.
            Err(_) => false,
        }
    };
    let filtered: Vec<_> = session_dirs
        .iter()
        .filter(|d| belongs(d))
        .cloned()
        .collect();
    if filtered.is_empty() && !session_dirs.is_empty() {
        (session_dirs, true)
    } else {
        (filtered, false)
    }
}

/// Check unpushed sessions against server policies.
///
/// `project_root` — the git-resolved PRIMARY worktree root (from
///   `paths::resolve_project_root`). Used to load config/credentials, locate
///   `.tracevault/sessions/`, and resolve the server-registered repo name
///   (the primary checkout's basename; a sibling worktree's own directory
///   basename would not match the registered repo).
/// `cwd` — the ACTUAL working directory where the CLI was invoked. Used for
///   git *state* (`HEAD`) so the reported `commit_sha` is the commit being
///   pushed from the invoking worktree, not the primary's unrelated HEAD.
pub async fn check_policies(
    project_root: &Path,
    cwd: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let (server_url, token) = resolve_credentials(project_root);

    let server_url = server_url
        .ok_or("No server URL configured. Run `tracevault login --server-url=<url>` to set one.")?;

    if token.is_none() {
        return Err(
            "Not logged in. Run `tracevault login --server-url=<server_url>` to authenticate."
                .into(),
        );
    }

    let client = ApiClient::new(&server_url, token.as_deref());

    // Resolve repo_id by name.
    //
    // Connectivity errors here (auth expired, server down, network
    // unreachable) propagate so the pre-push hook exits non-zero — if a
    // repo is opted into TraceVault, every push must be evaluated, full
    // stop. Letting pushes slip when TV is unreachable would defeat the
    // point of enforcement. We attach an actionable next step to each
    // error so the user (or agent) knows the recovery command without
    // guessing — see `connectivity_message` below.
    let repo = match resolve_repo_by_name(&client, project_root).await {
        Ok(r) => r,
        Err(ResolveRepoByNameError::Network(e)) => {
            return Err(connectivity_message(&e.to_string()).into());
        }
        Err(ResolveRepoByNameError::NotFound { repo_name }) => {
            return Err(format!(
                "Repo '{repo_name}' not found on server. Run `tracevault sync` first."
            )
            .into());
        }
    };

    // Collect unpushed session dirs from the shared primary .tracevault/.
    let sessions_dir = project_root.join(".tracevault").join("sessions");
    let mut unpushed_dirs: Vec<std::path::PathBuf> = Vec::new();
    if sessions_dir.exists() {
        for entry in fs::read_dir(&sessions_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let session_dir = entry.path();
            if session_dir.join(".pushed").exists() {
                continue;
            }
            unpushed_dirs.push(session_dir);
        }
    }

    // Filter to the worktree that is actually pushing, so a push is not
    // evaluated against sessions belonging to other worktrees.
    let worktree_top = crate::paths::worktree_toplevel(cwd);
    let (selected_dirs, fell_back) = select_worktree_sessions(unpushed_dirs, &worktree_top);
    if fell_back {
        eprintln!(
            "Warning: no unpushed session matched this worktree by origin marker — \
             checking all unpushed sessions to avoid skipping policy enforcement."
        );
    }

    let mut sessions = Vec::new();
    for session_dir in &selected_dirs {
        if let Some(data) = collect_session_data(session_dir) {
            sessions.push(data);
        }
    }

    if sessions.is_empty() {
        println!("No unpushed sessions to check.");
        return Ok(());
    }

    println!("Checking {} session(s) against policies...", sessions.len());

    // HEAD comes from the invoking worktree (`cwd`), not the primary root —
    // the commit being pushed lives on the current worktree's branch.
    let commit_sha = git_head_sha(cwd);
    let result = client
        .check_policies(
            &repo.id,
            CheckPoliciesRequest {
                sessions,
                commit_sha,
            },
        )
        .await
        .map_err(|e| connectivity_message(&e.to_string()))?;

    // Print results
    for r in &result.results {
        let icon = match r.result.as_str() {
            "pass" => "\x1b[32m✓\x1b[0m",                             // green
            "fail" if r.action == "block_push" => "\x1b[31m✗\x1b[0m", // red
            "fail" => "\x1b[33m!\x1b[0m",                             // yellow
            _ => " ",
        };
        println!(
            "  {} [{}] {} — {}",
            icon, r.severity, r.rule_name, r.details
        );
    }

    if result.blocked {
        eprintln!("\n\x1b[31mPolicy check failed: push blocked.\x1b[0m");
        std::process::exit(1);
    } else if result.passed {
        println!("\n\x1b[32mAll policy checks passed.\x1b[0m");
    } else {
        println!("\n\x1b[33mPolicy warnings found (push not blocked).\x1b[0m");
    }

    Ok(())
}

/// Wrap an opaque API-client error string with the most useful next step.
/// Today the api_client surfaces errors as `"Stream failed (401 ...)"` or
/// `"Server returned 500 Internal Server Error: ..."` strings; we sniff for
/// the common shapes and surface a one-line action.
fn connectivity_message(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();

    // 401 — token rejected. An expired session is the most common cause of
    // a surprise blocked push, so point straight at the refresh command.
    if lower.contains("401") || lower.contains("unauthorized") {
        return with_action(
            raw,
            "Session token may be expired. Run `tracevault login --server-url=<server_url>` to refresh.",
        );
    }
    // 403 — authenticated but not allowed. Re-login won't help; the token
    // itself is not authorized for this repo's policies.
    if lower.contains("403") || lower.contains("forbidden") {
        return with_action(
            raw,
            "Your token is not authorized for this repo's policies. Confirm the token/service account has access and rerun `tracevault login`.",
        );
    }
    // 5xx — server-side fault. Checked before the transport keywords below
    // so a `504 Gateway Timeout` reads as a server issue, not a local one.
    if is_server_error(&lower) {
        return with_action(
            raw,
            "TraceVault server returned an error. The team has likely been paged; retry shortly.",
        );
    }
    // Transport-level failure with no HTTP status — DNS, refused, timeout.
    // ("connect" also covers "connection refused"/"connection reset".)
    if lower.contains("dns")
        || lower.contains("connect")
        || lower.contains("timed out")
        || lower.contains("timeout")
    {
        return with_action(
            raw,
            "Could not reach the TraceVault server. Check network and `server_url` in .tracevault/config.toml.",
        );
    }
    // Unrecognized — surface the raw error verbatim, no invented advice.
    format!("Policy check could not run: {raw}.")
}

/// Format the standard "could not run" line with an actionable next step.
fn with_action(raw: &str, action: &str) -> String {
    format!("Policy check could not run: {raw}.\n  → {action}")
}

/// Heuristic for a 5xx server-side failure. Matches the textual status
/// (`internal`, `server error`) and the concrete 5xx codes so a bare
/// `503 Service Unavailable` — which carries neither phrase — is still
/// recognized.
fn is_server_error(lower: &str) -> bool {
    lower.contains("internal")
        || lower.contains("server error")
        || ["500", "501", "502", "503", "504"]
            .iter()
            .any(|code| lower.contains(code))
}

#[cfg(test)]
mod worktree_tests {
    use super::select_worktree_sessions;
    use std::path::PathBuf;

    fn seed(tmp: &std::path::Path, id: &str, origin: Option<&str>) -> PathBuf {
        let d = tmp.join(id);
        std::fs::create_dir_all(&d).unwrap();
        if let Some(o) = origin {
            std::fs::write(d.join("origin"), o).unwrap();
        }
        d
    }

    #[test]
    fn keeps_current_worktree_and_unmarked_excludes_other() {
        let tmp = tempfile::tempdir().unwrap();
        let mine = seed(tmp.path(), "mine", Some("/wt/here"));
        let other = seed(tmp.path(), "other", Some("/wt/there"));
        let legacy = seed(tmp.path(), "legacy", None);

        let (kept, fell_back) = select_worktree_sessions(
            vec![mine.clone(), other.clone(), legacy.clone()],
            "/wt/here",
        );

        assert!(!fell_back);
        assert!(kept.contains(&mine), "current worktree session kept");
        assert!(
            kept.contains(&legacy),
            "unmarked legacy session kept (conservative)"
        );
        assert!(
            !kept.contains(&other),
            "other worktree's marked session excluded"
        );
    }

    #[test]
    fn excludes_session_with_unreadable_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let mine = seed(tmp.path(), "mine", Some("/wt/here"));
        // A present-but-unreadable marker: make `origin` a directory so
        // read_to_string fails with a non-NotFound error.
        let bad = tmp.path().join("bad");
        std::fs::create_dir_all(bad.join("origin")).unwrap();

        let (kept, fell_back) =
            select_worktree_sessions(vec![mine.clone(), bad.clone()], "/wt/here");

        assert!(!fell_back);
        assert!(kept.contains(&mine), "current worktree session kept");
        assert!(
            !kept.contains(&bad),
            "session with a present-but-unreadable marker must be excluded, not kept as legacy"
        );
    }

    #[test]
    fn falls_back_to_all_when_nothing_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let a = seed(tmp.path(), "a", Some("/wt/there"));
        let b = seed(tmp.path(), "b", Some("/wt/elsewhere"));

        let (kept, fell_back) = select_worktree_sessions(vec![a.clone(), b.clone()], "/wt/here");

        assert!(fell_back, "must flag fallback when filter matches nothing");
        assert_eq!(
            kept.len(),
            2,
            "never under-enforce: send all rather than zero"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::connectivity_message;

    #[test]
    fn connectivity_message_suggests_login_on_401() {
        let m = connectivity_message("Stream failed (401 Unauthorized): bad token");
        assert!(
            m.contains("tracevault login"),
            "401 errors must surface the login hint; got: {m}"
        );
    }

    #[test]
    fn connectivity_message_suggests_login_on_unauthorized_text() {
        let m = connectivity_message("Server returned: unauthorized request");
        assert!(m.contains("tracevault login"));
    }

    #[test]
    fn connectivity_message_suggests_network_check_on_dns_error() {
        let m = connectivity_message("error sending request for url: dns error");
        assert!(
            m.to_lowercase().contains("network") || m.to_lowercase().contains("server_url"),
            "DNS errors must surface a network hint; got: {m}"
        );
    }

    #[test]
    fn connectivity_message_suggests_network_check_on_connection_refused() {
        let m = connectivity_message("connection refused");
        assert!(m.to_lowercase().contains("network"));
    }

    #[test]
    fn connectivity_message_falls_back_when_unrecognized() {
        let m = connectivity_message("some weird new error shape we have not seen before");
        // No suggestion — but the raw error must still be in the message so
        // the user can debug.
        assert!(m.contains("some weird new error shape"));
        assert!(!m.contains("→")); // no action arrow
    }

    #[test]
    fn connectivity_message_does_not_collide_on_403() {
        let m = connectivity_message("403 Forbidden");
        // 403 should point at token authorization, NOT a re-login.
        assert!(
            m.to_lowercase().contains("authorized"),
            "403 should mention authorization; got: {m}"
        );
    }

    #[test]
    fn connectivity_message_flags_500_as_server_error() {
        let m = connectivity_message("Server returned 500 Internal Server Error: oops");
        assert!(
            m.to_lowercase().contains("server returned an error"),
            "500 should surface the server-error hint; got: {m}"
        );
    }

    #[test]
    fn connectivity_message_flags_503_without_internal_text() {
        // `503 Service Unavailable` carries neither "internal" nor
        // "server error" — the bare code must still be recognized.
        let m = connectivity_message("Server returned 503 Service Unavailable: ");
        assert!(
            m.to_lowercase().contains("server returned an error"),
            "503 should surface the server-error hint; got: {m}"
        );
    }

    #[test]
    fn connectivity_message_treats_504_as_server_not_network() {
        // A gateway timeout is a server-side fault even though it contains
        // the word "timeout"; it must not be misreported as a local network
        // problem.
        let m = connectivity_message("Server returned 504 Gateway Timeout: ");
        assert!(
            m.to_lowercase().contains("server returned an error"),
            "504 should be treated as a server error; got: {m}"
        );
    }
}
