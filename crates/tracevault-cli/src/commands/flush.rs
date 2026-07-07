use crate::api_client::{resolve_credentials, ApiClient};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use tracevault_protocol::streaming::StreamEventRequest;

/// Extract the repo id from a per-repo pending queue filename
/// (`pending-<repo_id>.jsonl`). Returns None for anything else (e.g. a legacy
/// `pending.jsonl`).
pub(crate) fn repo_id_from_pending_filename(name: &str) -> Option<&str> {
    name.strip_prefix("pending-")?
        .strip_suffix(".jsonl")
        .filter(|s| !s.is_empty())
}

/// Classify a session dir's pending queue files. Returns (path, repo_id) pairs;
/// per-repo files carry their own id, the legacy `pending.jsonl` is attributed
/// to `bound_repo_id` (skipped entirely when None).
fn pending_queues_in(
    session_dir: &Path,
    bound_repo_id: Option<&str>,
) -> std::io::Result<Vec<(std::path::PathBuf, String)>> {
    let mut pending_queues: Vec<(std::path::PathBuf, String)> = fs::read_dir(session_dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?.to_string();
            let repo_id = repo_id_from_pending_filename(&name)?.to_string();
            Some((path, repo_id))
        })
        .collect();

    // Back-compat: releases before per-repo queue files existed wrote a
    // single `pending.jsonl`, implicitly attributed to the bound
    // `config.repo_id`. `status` counts these, so `flush` must be able to
    // drain them or they're stuck forever. If there's no bound repo_id,
    // skip it — there's nothing to attribute it to (best-effort).
    let legacy_path = session_dir.join("pending.jsonl");
    if legacy_path.exists() {
        if let Some(repo_id) = bound_repo_id {
            pending_queues.push((legacy_path, repo_id.to_string()));
        }
    }

    Ok(pending_queues)
}

/// The progress line uses a short prefix of the session id for display; guards
/// against a prior `[..8]` panic on session ids shorter than 8 bytes.
fn short_session_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

pub async fn run_flush(project_root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let org_slug = crate::resolution::org_slug_for(project_root)
        .ok_or("no org configured (set TRACEVAULT_ORG_SLUG, log in, or run in a bound repo)")?;

    let (server_url, token) = resolve_credentials(project_root);
    let server_url = server_url.ok_or("server_url not configured")?;
    let client = ApiClient::new(&server_url, token.as_deref());

    // Lenient config load: used only to attribute the legacy `pending.jsonl`
    // (written by releases before per-repo queue files existed) to the bound
    // repo id, if any. Best-effort — a missing/malformed config just means
    // any legacy queue can't be attributed and is left in place.
    let bound_repo_id = crate::config::TracevaultConfig::load(project_root).and_then(|c| c.repo_id);

    let sessions_dir = project_root.join(".tracevault").join("sessions");
    if !sessions_dir.exists() {
        println!("No sessions directory found. Nothing to flush.");
        return Ok(());
    }

    let mut total_sent = 0u64;
    let mut total_failed = 0u64;

    let session_entries: Vec<_> = fs::read_dir(&sessions_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();

    for session_entry in session_entries {
        // Collect (path, repo_id) pairs for every pending queue in this
        // session directory before draining, to keep the borrow/async loop
        // below simple.
        let pending_queues = pending_queues_in(&session_entry.path(), bound_repo_id.as_deref())?;

        for (pending_path, repo_id) in pending_queues {
            let events = drain_pending(&pending_path)?;
            if events.is_empty() {
                continue;
            }

            let event_total = events.len();
            let mut failed_events: Vec<StreamEventRequest> = Vec::new();

            for (i, mut event) in events.into_iter().enumerate() {
                eprint!(
                    "\r  Session {} — event {}/{} ...",
                    short_session_id(&event.session_id),
                    i + 1,
                    event_total
                );
                event.truncate_large_fields();
                match client.stream_event(&org_slug, &repo_id, &event).await {
                    Ok(_) => {
                        total_sent += 1;
                    }
                    Err(e) => {
                        eprintln!();
                        let err_str = e.to_string();
                        if err_str.contains("413") {
                            // Payload too large even after truncation — drop it.
                            eprintln!(
                                "  Warning: dropped event (session {}) — still too large after truncation",
                                event.session_id
                            );
                            total_failed += 1;
                        } else {
                            eprintln!(
                                "  Warning: failed to send event (session {}): {e}",
                                event.session_id
                            );
                            failed_events.push(event);
                            total_failed += 1;
                        }
                    }
                }
            }
            eprintln!();

            // Re-enqueue transiently failed events (not 413s) to the SAME
            // per-repo file.
            if !failed_events.is_empty() {
                append_pending(&pending_path, &failed_events)?;
            }
        }
    }

    println!("Flush complete: {total_sent} sent, {total_failed} failed");
    Ok(())
}

/// Read and remove all events from a pending.jsonl file.
fn drain_pending(path: &Path) -> Result<Vec<StreamEventRequest>, Box<dyn std::error::Error>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<StreamEventRequest>(trimmed) {
            Ok(event) => events.push(event),
            Err(e) => {
                eprintln!("Warning: skipping malformed pending event: {e}");
            }
        }
    }

    // Truncate the file after reading
    fs::write(path, "")?;

    Ok(events)
}

/// Append events back to a pending.jsonl file (for re-enqueuing failures).
fn append_pending(
    path: &Path,
    events: &[StreamEventRequest],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;

    for event in events {
        let json = serde_json::to_string(event)?;
        writeln!(file, "{json}")?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{pending_queues_in, repo_id_from_pending_filename, short_session_id};
    use crate::paths::resolve_project_root;
    use crate::test_helpers::{add_worktree, init_git_repo};
    use std::fs;

    #[test]
    fn repo_id_from_pending_filename_extracts_id() {
        assert_eq!(
            repo_id_from_pending_filename("pending-abc.jsonl"),
            Some("abc")
        );
    }

    #[test]
    fn repo_id_from_pending_filename_none_for_legacy_name() {
        assert_eq!(repo_id_from_pending_filename("pending.jsonl"), None);
    }

    #[test]
    fn repo_id_from_pending_filename_none_for_unrelated_name() {
        assert_eq!(repo_id_from_pending_filename("events.jsonl"), None);
        assert_eq!(repo_id_from_pending_filename("pending-abc.txt"), None);
    }

    #[test]
    fn repo_id_from_pending_filename_none_for_empty_id() {
        assert_eq!(repo_id_from_pending_filename("pending-.jsonl"), None);
    }

    /// From a sibling worktree the resolved project root must point at the
    /// PRIMARY checkout, so `flush` looks for sessions under
    /// `<primary>/.tracevault/sessions/` rather than a non-existent sibling dir.
    ///
    /// This test asserts only the resolution layer — draining the queue requires
    /// a live server, so the full end-to-end drain path is not exercised here.
    #[test]
    fn sessions_dir_from_sibling_worktree_is_under_primary() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("sibling-wt");

        fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);
        add_worktree(&repo, &wt);

        // Place a pending.jsonl in the primary sessions dir.
        let session_id = "aabbccdd-0000-0000-0000-000000000001";
        let sessions_dir = repo.join(".tracevault").join("sessions").join(session_id);
        fs::create_dir_all(&sessions_dir).unwrap();
        fs::write(sessions_dir.join("pending.jsonl"), "").unwrap();

        // Simulate what main.rs now does: resolve from the SIBLING worktree cwd.
        let resolved_root = resolve_project_root(&wt).root;
        let expected_sessions = repo
            .canonicalize()
            .unwrap()
            .join(".tracevault")
            .join("sessions");
        let got_sessions = resolved_root
            .canonicalize()
            .unwrap()
            .join(".tracevault")
            .join("sessions");

        assert_eq!(
            got_sessions, expected_sessions,
            "flush must look for sessions under the PRIMARY repo, not the sibling worktree dir"
        );
    }

    // ── pending_queues_in: per-session pending-queue enumeration ─────────────

    fn setup_session_dir_with_pending_files() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pending-a.jsonl"), "").unwrap();
        fs::write(dir.path().join("pending-b.jsonl"), "").unwrap();
        fs::write(dir.path().join("pending.jsonl"), "").unwrap();
        fs::write(dir.path().join("notes.txt"), "").unwrap();
        fs::write(dir.path().join("pending-.jsonl"), "").unwrap();
        dir
    }

    #[test]
    fn pending_queues_in_with_bound_repo_includes_legacy() {
        let dir = setup_session_dir_with_pending_files();
        let mut got = pending_queues_in(dir.path(), Some("cfg")).unwrap();
        got.sort_by(|a, b| a.1.cmp(&b.1));

        let repo_ids: Vec<&str> = got.iter().map(|(_, id)| id.as_str()).collect();
        assert_eq!(repo_ids, vec!["a", "b", "cfg"]);

        let legacy = got.iter().find(|(_, id)| id == "cfg").unwrap();
        assert_eq!(legacy.0, dir.path().join("pending.jsonl"));
    }

    #[test]
    fn pending_queues_in_without_bound_repo_skips_legacy() {
        let dir = setup_session_dir_with_pending_files();
        let mut got = pending_queues_in(dir.path(), None).unwrap();
        got.sort_by(|a, b| a.1.cmp(&b.1));

        let repo_ids: Vec<&str> = got.iter().map(|(_, id)| id.as_str()).collect();
        assert_eq!(repo_ids, vec!["a", "b"]);
    }

    #[test]
    fn pending_queues_in_ignores_unrelated_and_malformed_names() {
        let dir = setup_session_dir_with_pending_files();
        let got = pending_queues_in(dir.path(), Some("cfg")).unwrap();

        assert!(!got.iter().any(|(p, _)| p.ends_with("notes.txt")));
        assert!(!got.iter().any(|(p, _)| p.ends_with("pending-.jsonl")));
    }

    // ── short_session_id: display-only truncation, no panic on short ids ─────

    #[test]
    fn short_session_id_truncates_long_id() {
        assert_eq!(
            short_session_id("0190a1b2-cccc-dddd-eeee-ffffffffffff"),
            "0190a1b2"
        );
    }

    #[test]
    fn short_session_id_returns_whole_string_when_shorter_than_8() {
        assert_eq!(short_session_id("x"), "x");
    }

    #[test]
    fn short_session_id_handles_empty_string() {
        assert_eq!(short_session_id(""), "");
    }
}
