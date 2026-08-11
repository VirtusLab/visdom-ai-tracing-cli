use crate::api_client::{ApiClient, CheckPoliciesRequest, SessionCheckData};
use crate::credentials::resolve_credentials;
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

/// One ref being pushed, as described by git's pre-push stdin protocol.
#[derive(Debug, PartialEq)]
pub(crate) struct PushRef {
    pub local_sha: String,
    pub remote_sha: String,
}

fn is_zero_sha(sha: &str) -> bool {
    !sha.is_empty() && sha.chars().all(|c| c == '0')
}

/// Parse git's pre-push stdin. Malformed lines are skipped rather than
/// failing the push: this is an enrichment, and a parse error must never be
/// the reason a developer cannot push. Branch deletions (all-zero LOCAL sha)
/// are dropped — no content is being pushed for them.
pub(crate) fn parse_push_refs(stdin: &str) -> Vec<PushRef> {
    stdin
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let _local_ref = parts.next()?;
            let local_sha = parts.next()?;
            let _remote_ref = parts.next()?;
            let remote_sha = parts.next()?;
            if is_zero_sha(local_sha) {
                return None;
            }
            Some(PushRef {
                local_sha: local_sha.to_string(),
                remote_sha: remote_sha.to_string(),
            })
        })
        .collect()
}

/// Files changed by the refs being pushed, repo-relative as git emits them.
/// Over-inclusion is the safe direction: a larger set only means less pruning
/// of AI-touched paths, never a missed violation.
fn changed_files_for_refs(project_root: &Path, refs: &[PushRef]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for r in refs {
        // A new branch has no remote sha to diff from. `git diff --name-only
        // <sha>` (one argument) diffs the working tree against that commit,
        // which is wrong here, so name the empty tree explicitly.
        let base = if is_zero_sha(&r.remote_sha) {
            match git_empty_tree_sha(project_root) {
                Some(t) => t,
                None => continue,
            }
        } else {
            r.remote_sha.clone()
        };
        let out_bytes = Command::new("git")
            .args(["diff", "--name-only", &format!("{base}..{}", r.local_sha)])
            .current_dir(project_root)
            .output();
        let Ok(o) = out_bytes else { continue };
        if !o.status.success() {
            continue;
        }
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let p = line.trim();
            if !p.is_empty() && seen.insert(p.to_string()) {
                out.push(p.to_string());
            }
        }
    }
    out
}

/// The well-known empty tree object, resolved via git so it is correct for
/// both sha1 and sha256 repositories rather than hardcoded.
fn git_empty_tree_sha(project_root: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["hash-object", "-t", "tree", "/dev/null"])
        .current_dir(project_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// How long to wait for git's pre-push stdin to close before falling back to
/// `@{upstream}..HEAD`. A pre-push hook writes its ref lines and closes the
/// pipe immediately, so this bound is never hit in normal operation — it
/// exists only to protect against a stalled non-TTY pipe (see
/// `resolve_changed_paths` and `read_stdin_with_timeout`).
const STDIN_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Run `f` on a detached background thread and wait up to `timeout` for it
/// to produce a value, returning `None` on timeout.
///
/// `f` is not cancelled on timeout: a blocked read has no way to be aborted
/// from the outside, so the thread is deliberately leaked and we let the
/// process exit around it rather than wait for it to unblock.
///
/// Generic over the read so the timeout mechanism can be exercised in tests
/// without touching the process's real, global stdin — see
/// `read_stdin_with_timeout`, which is the thin, untested wrapper that calls
/// this with `std::io::stdin().read_to_string(..)`.
fn read_with_timeout<F>(f: F, timeout: std::time::Duration) -> Option<String>
where
    F: FnOnce() -> Option<String> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(timeout).ok().flatten()
}

/// Read all of stdin, giving up after `timeout`.
///
/// `read_to_string` blocks until EOF, and a non-TTY pipe whose writer never
/// closes never sends one. A git pre-push hook always closes the pipe, so
/// this bound is not hit in normal operation — but `tracevault check` sits in
/// the push path, and hanging there with no output is far worse than losing
/// the push-diff optimisation.
fn read_stdin_with_timeout(timeout: std::time::Duration) -> Option<String> {
    use std::io::Read;
    read_with_timeout(
        || {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).ok().map(|_| buf)
        },
        timeout,
    )
}

/// Files in the push being attempted, or `None` if it cannot be determined.
///
/// Order: git's pre-push stdin (authoritative — correct for new branches,
/// force pushes, and multi-ref pushes), then `@{upstream}..HEAD`, then give
/// up. `None` is safe: the server falls back to evaluating the raw AI-touched
/// set, which over-blocks rather than under-blocks.
fn resolve_changed_paths(project_root: &Path) -> Option<Vec<String>> {
    use std::io::IsTerminal;

    // `tracevault check` is also run by hand. Reading stdin unconditionally
    // would block waiting for EOF, hanging the terminal. That guard alone is
    // incomplete, though: a non-TTY pipe whose writer never closes (e.g. a
    // stalled hook, a misbehaving wrapper script) also never sends EOF, so
    // the read is additionally bounded in time.
    if !std::io::stdin().is_terminal() {
        if let Some(buf) = read_stdin_with_timeout(STDIN_READ_TIMEOUT) {
            let refs = parse_push_refs(&buf);
            if !refs.is_empty() {
                let files = changed_files_for_refs(project_root, &refs);
                if !files.is_empty() {
                    return Some(files);
                }
            }
        }
    }

    let out = Command::new("git")
        .args(["diff", "--name-only", "@{upstream}..HEAD"])
        .current_dir(project_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let files: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if files.is_empty() {
        None
    } else {
        Some(files)
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
    let (server_url, credential) = resolve_credentials(project_root)?;

    let server_url = server_url
        .ok_or("No server URL configured. Run `tracevault login --server-url=<url>` to set one.")?;

    if credential.is_none() {
        return Err(
            "Not logged in. Run `tracevault login --server-url=<server_url>` to authenticate."
                .into(),
        );
    }

    let client = ApiClient::with_credential(&server_url, credential);

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
        Err(ResolveRepoByNameError::ListFailed(e)) => {
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
    let changed_paths = resolve_changed_paths(cwd);
    let result = client
        .check_policies(
            &repo.id,
            CheckPoliciesRequest {
                sessions,
                commit_sha,
                changed_paths,
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
/// the common shapes and surface a one-line action. A 404 gets its own
/// version-mismatch hint rather than the generic fallback, since the calls
/// that route through here have no legitimate domain reason to 404 — see
/// the 404 branch below.
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
    // 404 — the route itself wasn't found. Neither call that reaches this
    // function (`list_repos`, `check_policies`) has a legitimate domain
    // reason to 404 — a missing repo is already turned into its own message
    // by `resolve_repo_by_name`'s `NotFound` variant before it gets here —
    // so a 404 this far down most plausibly means this server build doesn't
    // recognize the route at all: a CLI/server version mismatch (e.g. this
    // CLI shipped ahead of a server still on the old routes).
    if lower.contains("404") {
        return with_action(
            raw,
            "This server may not recognize this endpoint yet — this can happen when the CLI is newer than the server. Confirm the server has been upgraded, or downgrade the CLI to match it.",
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
    fn connectivity_message_flags_404_as_version_mismatch() {
        // Simulates an old-server-404 skew: a route the CLI expects to exist
        // (e.g. `Failed to list repos (404 Not Found)`) doesn't exist yet on
        // this server build. Must hint at a version mismatch, not just
        // report a bare 404 with no next step.
        let m = connectivity_message("Failed to list repos (404 Not Found): ");
        assert!(
            m.to_lowercase().contains("newer than the server")
                || m.to_lowercase().contains("version mismatch"),
            "404 should hint at a possible CLI/server version mismatch; got: {m}"
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

#[cfg(test)]
mod push_ref_tests {
    use super::{is_zero_sha, parse_push_refs};

    #[test]
    fn parses_a_single_ref_line() {
        let refs = parse_push_refs("refs/heads/main abc123 refs/heads/main def456\n");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].local_sha, "abc123");
        assert_eq!(refs[0].remote_sha, "def456");
    }

    #[test]
    fn parses_multiple_refs() {
        let refs = parse_push_refs(
            "refs/heads/a 111 refs/heads/a 222\nrefs/heads/b 333 refs/heads/b 444\n",
        );
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn parses_a_single_ref_line_without_trailing_newline() {
        // This is the exact shape the generated pre-push hook produces: it
        // captures git's stdin with `$(cat)` (which strips trailing
        // newlines) and replays it with `printf '%s'` (which adds none
        // back), so the real input to `check` has no trailing newline on
        // its final line. Here that unterminated line is the *only* line.
        // Do not delete this as a duplicate of `parses_a_single_ref_line`;
        // it pins a different, easy-to-break input shape.
        let refs = parse_push_refs("refs/heads/main abc123 refs/heads/main def456");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].local_sha, "abc123");
        assert_eq!(refs[0].remote_sha, "def456");
    }

    #[test]
    fn parses_multiple_refs_when_last_line_has_no_trailing_newline() {
        // Same hook-stdin shape as above, but with two refs: only the final
        // line is unterminated, matching a real multi-ref push through
        // `$(cat)` + `printf '%s'`.
        let refs =
            parse_push_refs("refs/heads/a 111 refs/heads/a 222\nrefs/heads/b 333 refs/heads/b 444");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].local_sha, "111");
        assert_eq!(refs[1].local_sha, "333");
    }

    #[test]
    fn skips_branch_deletions() {
        let zeros = "0".repeat(40);
        let line = format!("(delete) {zeros} refs/heads/gone abc123\n");
        assert!(parse_push_refs(&line).is_empty());
    }

    #[test]
    fn keeps_new_branches_whose_remote_sha_is_zero() {
        let zeros = "0".repeat(40);
        let line = format!("refs/heads/new abc123 refs/heads/new {zeros}\n");
        let refs = parse_push_refs(&line);
        assert_eq!(refs.len(), 1);
        assert!(is_zero_sha(&refs[0].remote_sha));
    }

    #[test]
    fn skips_malformed_lines_without_panicking() {
        assert!(parse_push_refs("garbage\n\nalso garbage here\n").is_empty());
    }

    #[test]
    fn zero_sha_detection_handles_sha256_length() {
        assert!(is_zero_sha(&"0".repeat(64)));
        assert!(!is_zero_sha("0000000000000000000000000000000000000001"));
        assert!(!is_zero_sha(""));
    }
}

/// Real-git coverage for the functions `resolve_changed_paths` composes:
/// `changed_files_for_refs` (the two-dot range and the empty-tree base for
/// new branches) and `git_empty_tree_sha`.
///
/// `resolve_changed_paths` itself is deliberately NOT driven directly here.
/// It reads `std::io::stdin()` when stdin is not a terminal, and `cargo
/// test` runs every test in one process sharing that real, global stdin —
/// there is no per-test way to inject or close it without invasive,
/// process-wide fd surgery, and a test that consumes it would make unrelated
/// tests in the same binary flaky. Testing the two git-backed branches it
/// falls back to (below) is the safe substitute; the stdin-parsing branch
/// itself is already covered without touching stdin by `push_ref_tests`
/// (`parse_push_refs`) plus `two_dot_range_returns_only_the_newer_commits_files`
/// and `new_branch_zero_remote_sha_resolves_via_empty_tree` below, which
/// together exercise every step `resolve_changed_paths` performs once it has
/// a parsed `Vec<PushRef>` in hand. The timeout mechanism that bounds the
/// stdin read (`read_with_timeout`) is factored generically over the reading
/// closure precisely so it can be tested without touching stdin at all —
/// see `read_with_timeout_tests` below.
#[cfg(test)]
mod git_diff_tests {
    use super::{changed_files_for_refs, git_empty_tree_sha, PushRef};
    use crate::test_helpers::init_git_repo;
    use std::path::Path;
    use std::process::Command;

    /// Write `name` with `content` in `dir`, stage it, and commit it,
    /// returning the new commit's sha. Assumes `init_git_repo` already
    /// configured a local `user.name`/`user.email` in `dir`.
    fn commit_file(dir: &Path, name: &str, content: &str) -> String {
        std::fs::write(dir.join(name), content).unwrap();
        let ok = Command::new("git")
            .args(["-C", &dir.to_string_lossy(), "add", name])
            .status()
            .expect("git add failed")
            .success();
        assert!(ok, "git add must succeed");
        let ok = Command::new("git")
            .args([
                "-C",
                &dir.to_string_lossy(),
                "commit",
                "-m",
                &format!("add {name}"),
            ])
            .status()
            .expect("git commit failed")
            .success();
        assert!(ok, "git commit must succeed");
        head_sha(dir)
    }

    fn head_sha(dir: &Path) -> String {
        let out = Command::new("git")
            .args(["-C", &dir.to_string_lossy(), "rev-parse", "HEAD"])
            .output()
            .expect("git rev-parse failed");
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn two_dot_range_returns_only_the_newer_commits_files() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path()); // empty "init" commit

        let sha1 = commit_file(tmp.path(), "a.txt", "a");
        let sha2 = commit_file(tmp.path(), "b.txt", "b");

        let refs = vec![PushRef {
            local_sha: sha2,
            remote_sha: sha1,
        }];
        let files = changed_files_for_refs(tmp.path(), &refs);

        // Must be exactly the file the newer commit touched. A single-arg
        // `git diff --name-only <sha>` would instead diff against the
        // working tree and silently pull in every tracked file (here, also
        // "a.txt") — this is the mistake this test exists to catch.
        assert_eq!(files, vec!["b.txt".to_string()]);
    }

    #[test]
    fn new_branch_zero_remote_sha_resolves_via_empty_tree() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());

        commit_file(tmp.path(), "a.txt", "a");
        let sha2 = commit_file(tmp.path(), "b.txt", "b");

        let refs = vec![PushRef {
            local_sha: sha2,
            remote_sha: "0".repeat(40),
        }];
        let mut files = changed_files_for_refs(tmp.path(), &refs);
        files.sort();

        // A brand-new branch has no remote sha to diff from; the base must
        // resolve to the empty tree, so the full branch history shows up.
        assert_eq!(files, vec!["a.txt".to_string(), "b.txt".to_string()]);
    }

    #[test]
    fn identical_shas_yield_an_empty_file_list() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        let sha = commit_file(tmp.path(), "a.txt", "a");

        let refs = vec![PushRef {
            local_sha: sha.clone(),
            remote_sha: sha,
        }];
        let files = changed_files_for_refs(tmp.path(), &refs);

        assert!(
            files.is_empty(),
            "a no-op push (same local and remote sha) must yield no files, got {files:?}"
        );
    }

    #[test]
    fn git_empty_tree_sha_is_the_well_known_constant() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());

        let empty_tree =
            git_empty_tree_sha(tmp.path()).expect("git hash-object must resolve the empty tree");
        // The well-known empty-tree object id for sha1 repositories (git's
        // default) — asserted against the literal, not just "non-empty", so
        // a regression that resolves some other tree can't slip through.
        assert_eq!(empty_tree, "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
    }

    /// Not a call into `resolve_changed_paths` (see the module doc comment
    /// above for why) — this drives the exact git invocation its
    /// `@{{upstream}}` fallback branch makes, in a repo with no upstream
    /// configured, confirming that branch fails cleanly (non-zero exit) and
    /// so `resolve_changed_paths` would correctly return `None` rather than
    /// erroring or panicking.
    #[test]
    fn upstream_fallback_diff_fails_cleanly_with_no_upstream_configured() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        commit_file(tmp.path(), "a.txt", "a");

        let out = Command::new("git")
            .args(["diff", "--name-only", "@{upstream}..HEAD"])
            .current_dir(tmp.path())
            .output()
            .expect("git diff must run");

        assert!(
            !out.status.success(),
            "no upstream is configured, so this must fail rather than \
             silently resolve to something else"
        );
    }

    /// Companion to the test above: with an upstream configured but no diff
    /// between it and `HEAD`, the same invocation must succeed with empty
    /// output — the shape `resolve_changed_paths` treats as `None`.
    #[test]
    fn upstream_fallback_diff_is_empty_when_upstream_matches_head() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        commit_file(tmp.path(), "a.txt", "a");

        let branch_out = Command::new("git")
            .args(["symbolic-ref", "--short", "HEAD"])
            .current_dir(tmp.path())
            .output()
            .expect("git symbolic-ref must run");
        assert!(branch_out.status.success());
        let branch = String::from_utf8_lossy(&branch_out.stdout)
            .trim()
            .to_string();

        // Track a same-commit local branch as upstream, so `@{upstream}`
        // resolves but the diff against it is empty — without needing an
        // actual remote.
        for args in [
            vec!["branch", "tracking", &branch],
            vec!["branch", "--set-upstream-to=tracking", &branch],
        ] {
            let ok = Command::new("git")
                .args(&args)
                .current_dir(tmp.path())
                .status()
                .expect("git command must run")
                .success();
            assert!(ok, "git {args:?} must succeed");
        }

        let out = Command::new("git")
            .args(["diff", "--name-only", "@{upstream}..HEAD"])
            .current_dir(tmp.path())
            .output()
            .expect("git diff must run");

        assert!(out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stdout).trim().is_empty(),
            "identical upstream/HEAD must diff to no files"
        );
    }
}

/// Coverage for the generic timeout mechanism behind `read_stdin_with_timeout`.
///
/// These drive `read_with_timeout` directly with an injected closure instead
/// of the real `stdin()` — see the doc comment on `git_diff_tests` above for
/// why touching the process's real, global stdin from a test is unsafe here.
/// `read_stdin_with_timeout` itself is left thin and untested; once the
/// generic helper is proven to time out and to pass through a prompt result,
/// there is nothing stdin-specific left to verify.
#[cfg(test)]
mod read_with_timeout_tests {
    use super::read_with_timeout;
    use std::time::Duration;

    #[test]
    fn returns_the_value_when_it_arrives_before_the_deadline() {
        let got = read_with_timeout(|| Some("hi".to_string()), Duration::from_secs(1));
        assert_eq!(got, Some("hi".to_string()));
    }

    #[test]
    fn returns_none_when_the_closure_itself_returns_none() {
        let got = read_with_timeout(|| None, Duration::from_secs(1));
        assert_eq!(got, None);
    }

    #[test]
    fn times_out_instead_of_hanging_on_a_closure_that_never_returns() {
        // The stalled-pipe case in miniature: the closure blocks well past
        // the deadline (in the real bug, forever). Margins are kept wide so
        // this cannot flake on a loaded CI machine — a 50ms timeout that
        // must return within 1s, against a closure that sleeps for 10s.
        let start = std::time::Instant::now();
        let got = read_with_timeout(
            || {
                std::thread::sleep(Duration::from_secs(10));
                Some("too late".to_string())
            },
            Duration::from_millis(50),
        );
        assert_eq!(got, None, "must give up rather than wait for the closure");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "must return promptly on timeout, not block on the stalled closure; took {:?}",
            start.elapsed()
        );
    }
}
