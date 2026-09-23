use crate::api_client::ApiClient;
use crate::credentials::resolve_credentials;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use tracevault_protocol::streaming::StreamEventRequest;

/// Shared spelling of the offline-queue filenames. These are the ONLY place
/// the shape is written down: `commands::stream` builds names from them and
/// this module parses names with them, so the writer and the reader cannot
/// drift apart. Drift would be silent — a name this module fails to classify
/// is skipped, stranding the queue while `status` keeps counting it.
pub(crate) const QUEUE_PREFIX: &str = "pending-";
/// Repo-less queues. Note this EXTENDS `QUEUE_PREFIX`, which is what makes an
/// older CLI skip these files rather than mis-drain them.
pub(crate) const PROJECT_QUEUE_PREFIX: &str = "pending-project-";
pub(crate) const QUEUE_SUFFIX: &str = ".jsonl";

/// Extract the repo id from a per-repo pending queue filename
/// (`pending-<repo_id>.jsonl`). Returns None for anything else (e.g. a legacy
/// `pending.jsonl`).
///
/// Note this also matches a repo-less `pending-project-<uuid>.jsonl` and
/// yields `project-<uuid>`, which is not a UUID. That is deliberate and is
/// what keeps an older CLI from mis-draining those queues; callers here must
/// try [`project_id_from_pending_filename`] FIRST.
pub(crate) fn repo_id_from_pending_filename(name: &str) -> Option<&str> {
    name.strip_prefix(QUEUE_PREFIX)?
        .strip_suffix(QUEUE_SUFFIX)
        .filter(|s| !s.is_empty())
}

/// Extract the project id from a repo-less pending queue filename
/// (`pending-project-<project_id>.jsonl`), written by the stream hook when a
/// session has a project binding but no usable repo binding.
pub(crate) fn project_id_from_pending_filename(name: &str) -> Option<&str> {
    name.strip_prefix(PROJECT_QUEUE_PREFIX)?
        .strip_suffix(QUEUE_SUFFIX)
        .filter(|s| !s.is_empty())
}

/// Which endpoint a pending queue file drains to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QueueTarget {
    Repo(String),
    Project(uuid::Uuid),
}

/// Classify one directory entry's filename into a queue target. Project
/// queues are tested first: `pending-project-<uuid>.jsonl` also satisfies the
/// repo pattern (yielding a non-UUID `project-<uuid>`), so checking repo
/// first would reject it outright and strand the queue.
pub(crate) fn queue_target_from_filename(name: &str) -> Option<QueueTarget> {
    if let Some(pid) = project_id_from_pending_filename(name) {
        return uuid::Uuid::parse_str(pid).ok().map(QueueTarget::Project);
    }
    let repo_id = repo_id_from_pending_filename(name)?;
    // Validate repo_id is a UUID (guards against corrupted/hand-created files)
    uuid::Uuid::parse_str(repo_id).ok()?;
    Some(QueueTarget::Repo(repo_id.to_string()))
}

/// Classify a session dir's pending queue files. Returns
/// `(path, QueueTarget)` pairs: per-repo and repo-less project files each
/// carry their own id (see [`queue_target_from_filename`]), and the legacy
/// `pending.jsonl` is attributed to `bound_repo_id` (skipped entirely when
/// None). Files whose embedded id is not a valid UUID are excluded.
fn pending_queues_in(
    session_dir: &Path,
    bound_repo_id: Option<&str>,
) -> std::io::Result<Vec<(std::path::PathBuf, QueueTarget)>> {
    let mut pending_queues: Vec<(std::path::PathBuf, QueueTarget)> = fs::read_dir(session_dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?.to_string();
            Some((path, queue_target_from_filename(&name)?))
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
            pending_queues.push((legacy_path, QueueTarget::Repo(repo_id.to_string())));
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
    let (server_url, credential) = resolve_credentials(project_root)?;
    let server_url = server_url.ok_or("server_url not configured")?;
    let client = ApiClient::with_credential(&server_url, credential);

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
        // Collect (path, QueueTarget) pairs for every pending queue in this
        // session directory before draining, to keep the borrow/async loop
        // below simple.
        let pending_queues = pending_queues_in(&session_entry.path(), bound_repo_id.as_deref())?;

        for (pending_path, target) in pending_queues {
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
                let sent = match &target {
                    QueueTarget::Repo(repo_id) => client.stream_event(repo_id, &event).await,
                    // Repo-less queue: drain to the project endpoint with no
                    // repo_id, the same shape the stream hook buffered it as.
                    // The attribution mode is re-derived from THIS process's
                    // environment/disk state at flush time, same as the live
                    // send path — the buffered event itself doesn't carry the
                    // mode it was originally captured under.
                    QueueTarget::Project(pid) => {
                        client
                            .stream_event_for_project(
                                *pid,
                                None,
                                crate::commands::stream::attribution_mode(),
                                &event,
                            )
                            .await
                    }
                };
                match sent {
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
    use super::{pending_queues_in, repo_id_from_pending_filename, short_session_id, QueueTarget};
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
        fs::write(
            dir.path()
                .join("pending-0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.jsonl"),
            "",
        )
        .unwrap();
        fs::write(
            dir.path()
                .join("pending-550e8400-e29b-41d4-a716-446655440000.jsonl"),
            "",
        )
        .unwrap();
        fs::write(dir.path().join("pending.jsonl"), "").unwrap();
        fs::write(dir.path().join("notes.txt"), "").unwrap();
        fs::write(dir.path().join("pending-.jsonl"), "").unwrap();
        fs::write(dir.path().join("pending-not-a-uuid.jsonl"), "").unwrap();
        // A repo-less session's queue, written by the stream hook when only a
        // project binding resolved.
        fs::write(
            dir.path()
                .join("pending-project-018f0000-0000-7000-8000-000000000abc.jsonl"),
            "",
        )
        .unwrap();
        dir
    }

    /// Render a classified target as a sortable label so the assertions below
    /// can compare whole queue sets, and so a repo queue and a project queue
    /// that happened to share an id would still be distinguishable.
    fn label(t: &QueueTarget) -> String {
        match t {
            QueueTarget::Repo(id) => id.clone(),
            QueueTarget::Project(id) => format!("project:{id}"),
        }
    }

    fn sorted_labels(got: &[(std::path::PathBuf, QueueTarget)]) -> Vec<String> {
        let mut v: Vec<String> = got.iter().map(|(_, t)| label(t)).collect();
        v.sort();
        v
    }

    #[test]
    fn pending_queues_in_with_bound_repo_includes_legacy() {
        let dir = setup_session_dir_with_pending_files();
        let got =
            pending_queues_in(dir.path(), Some("550e8400-e29b-41d4-a716-446655440001")).unwrap();
        assert_eq!(
            sorted_labels(&got),
            vec![
                "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b",
                "550e8400-e29b-41d4-a716-446655440000",
                "550e8400-e29b-41d4-a716-446655440001",
                "project:018f0000-0000-7000-8000-000000000abc",
            ]
        );

        let legacy = got
            .iter()
            .find(|(_, t)| matches!(t, QueueTarget::Repo(id) if id == "550e8400-e29b-41d4-a716-446655440001"))
            .unwrap();
        assert_eq!(legacy.0, dir.path().join("pending.jsonl"));
    }

    #[test]
    fn pending_queues_in_without_bound_repo_skips_legacy() {
        let dir = setup_session_dir_with_pending_files();
        let got = pending_queues_in(dir.path(), None).unwrap();
        assert_eq!(
            sorted_labels(&got),
            vec![
                "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b",
                "550e8400-e29b-41d4-a716-446655440000",
                "project:018f0000-0000-7000-8000-000000000abc",
            ]
        );
    }

    #[test]
    fn pending_queues_in_ignores_unrelated_and_malformed_names() {
        let dir = setup_session_dir_with_pending_files();
        let got =
            pending_queues_in(dir.path(), Some("550e8400-e29b-41d4-a716-446655440001")).unwrap();

        assert!(!got.iter().any(|(p, _)| p.ends_with("notes.txt")));
        assert!(!got.iter().any(|(p, _)| p.ends_with("pending-.jsonl")));
        // Non-UUID repo_ids are silently skipped (guards against corrupted files)
        assert!(!got
            .iter()
            .any(|(p, _)| p.ends_with("pending-not-a-uuid.jsonl")));
    }

    #[test]
    fn pending_queues_in_validates_uuid_in_per_repo_files() {
        let dir = setup_session_dir_with_pending_files();
        let got = pending_queues_in(dir.path(), None).unwrap();

        // Valid UUIDs are included
        assert!(got
            .iter()
            .any(|(p, _)| p.ends_with("pending-0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.jsonl")));
        assert!(got
            .iter()
            .any(|(p, _)| p.ends_with("pending-550e8400-e29b-41d4-a716-446655440000.jsonl")));
        // Non-UUID files are NOT included
        assert!(!got
            .iter()
            .any(|(p, _)| p.ends_with("pending-not-a-uuid.jsonl")));
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

#[cfg(test)]
mod queue_target_tests {
    use super::*;

    /// A repo-less queue must classify as a PROJECT target. The repo pattern
    /// also matches this filename (yielding `project-<uuid>`), so ordering
    /// inside `queue_target_from_filename` is what makes this pass.
    #[test]
    fn project_queue_classifies_as_project_not_repo() {
        let pid = uuid::Uuid::new_v4();
        assert_eq!(
            queue_target_from_filename(&format!("pending-project-{pid}.jsonl")),
            Some(QueueTarget::Project(pid))
        );
    }

    #[test]
    fn repo_queue_still_classifies_as_repo() {
        let repo = uuid::Uuid::new_v4();
        assert_eq!(
            queue_target_from_filename(&format!("pending-{repo}.jsonl")),
            Some(QueueTarget::Repo(repo.to_string()))
        );
    }

    /// The writer (`Attribution::pending_file_name`, in `commands::stream`)
    /// and the reader (`queue_target_from_filename`, here) are the two halves
    /// of one format that live in different modules. Nothing else fails if
    /// they diverge — an unparsable name is silently skipped, stranding the
    /// queue while `status` still counts it — so pin the round trip.
    #[test]
    fn every_attribution_round_trips_through_its_queue_filename() {
        use crate::commands::stream::Attribution;

        let repo = uuid::Uuid::new_v4();
        assert_eq!(
            queue_target_from_filename(
                &Attribution::Repo {
                    repo_id: repo.to_string(),
                    project: Some(uuid::Uuid::new_v4()),
                }
                .pending_file_name()
            ),
            Some(QueueTarget::Repo(repo.to_string())),
            "a repo queue must classify back to the same repo"
        );

        let pid = uuid::Uuid::new_v4();
        assert_eq!(
            queue_target_from_filename(
                &Attribution::ProjectOnly { project_id: pid }.pending_file_name()
            ),
            Some(QueueTarget::Project(pid)),
            "a repo-less queue must classify back to the same project, NOT to a repo"
        );
    }

    /// Traversal-shaped names are rejected by the UUID parse on both arms.
    /// `pending_queues_in` builds its paths from `fs::read_dir`, which never
    /// yields `.` or `..` as entries, so a traversal component cannot reach
    /// here in the first place — this pins the second line of defence, the
    /// one that would still hold if the source of the names ever changed.
    #[test]
    fn traversal_shaped_names_are_rejected() {
        for name in [
            "pending-../../etc/passwd.jsonl",
            "pending-project-../../etc/passwd.jsonl",
            "pending-..%2f..%2fetc.jsonl",
            "pending-/etc/passwd.jsonl",
        ] {
            assert_eq!(queue_target_from_filename(name), None, "accepted: {name}");
        }
    }

    /// Non-UUID ids are rejected on both arms — these names can only come
    /// from a corrupted or hand-created file.
    #[test]
    fn non_uuid_ids_are_rejected() {
        assert_eq!(queue_target_from_filename("pending-nope.jsonl"), None);
        assert_eq!(
            queue_target_from_filename("pending-project-nope.jsonl"),
            None
        );
        assert_eq!(queue_target_from_filename("pending-.jsonl"), None);
        assert_eq!(queue_target_from_filename("pending.jsonl"), None);
    }
}
