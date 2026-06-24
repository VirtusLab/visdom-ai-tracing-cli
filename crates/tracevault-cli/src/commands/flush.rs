use crate::api_client::{resolve_credentials, ApiClient};
use crate::config::TracevaultConfig;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use tracevault_protocol::streaming::StreamEventRequest;

pub async fn run_flush(project_root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let config = TracevaultConfig::load(project_root).ok_or("config not found")?;
    let org_slug = config.org_slug.ok_or("org_slug not configured")?;
    let repo_id = config.repo_id.ok_or("repo_id not configured")?;

    let (server_url, token) = resolve_credentials(project_root);
    let server_url = server_url.ok_or("server_url not configured")?;
    let client = ApiClient::new(&server_url, token.as_deref());

    let sessions_dir = project_root.join(".tracevault").join("sessions");
    if !sessions_dir.exists() {
        println!("No sessions directory found. Nothing to flush.");
        return Ok(());
    }

    let mut total_sent = 0u64;
    let mut total_failed = 0u64;

    let entries: Vec<_> = fs::read_dir(&sessions_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();

    for entry in entries {
        let pending_path = entry.path().join("pending.jsonl");
        if !pending_path.exists() {
            continue;
        }

        let events = drain_pending(&pending_path)?;
        if events.is_empty() {
            continue;
        }

        let event_total = events.len();
        let mut failed_events: Vec<StreamEventRequest> = Vec::new();

        for (i, mut event) in events.into_iter().enumerate() {
            eprint!(
                "\r  Session {} — event {}/{} ...",
                &event.session_id[..8],
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

        // Re-enqueue transiently failed events (not 413s)
        if !failed_events.is_empty() {
            append_pending(&pending_path, &failed_events)?;
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
    use crate::paths::resolve_project_root;
    use crate::test_helpers::{add_worktree, init_git_repo};
    use std::fs;

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
}
