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
fn display_prefix(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// What happened to one queued event on its way back to the server.
#[derive(Debug)]
pub(crate) enum QueuedSend {
    Sent,
    /// 413 even after truncation — dropped, never retried.
    TooLarge,
    /// Transient (5xx, network, 401, ...) — re-enqueue and retry later.
    Failed(String),
    /// The server deterministically refused the declared capture project
    /// (400/403/404/409 on the project-scoped send). The rest of the queue
    /// cannot succeed against the same binding, and sending the event
    /// anywhere else would re-attribute it, so the caller stops draining.
    Refused {
        pid: uuid::Uuid,
        kind: crate::commands::stream::ClientErrorKind,
    },
}

/// The capture project a queue's events are sent under, resolved exactly as
/// the stream hook resolves it: the session's state (`session_state::load`,
/// keyed by the session directory's name) plus the worktree toplevel the hook
/// recorded in the session dir's `origin` marker (see `run_stream`). Only
/// repo queues consult it — a repo-less queue is already keyed by its project.
///
/// Without this, `flush` would drain a repo queue to the repo-scoped endpoint
/// and let the server deduce a project, re-attributing the very events the
/// hook queued because the server REFUSED the declared project.
fn queue_capture_project(session_dir: &Path, target: &QueueTarget) -> Option<uuid::Uuid> {
    if !matches!(target, QueueTarget::Repo(_)) {
        return None;
    }
    let session_id = session_dir.file_name()?.to_str()?;
    let session = crate::session_state::load(session_id);
    // Trimmed like the other `origin` readers (`check`, `verification_phase`):
    // the hook writes no trailing newline today, but a hand-edited marker
    // must not silently miss the subagent worktree override.
    let worktree = fs::read_to_string(session_dir.join("origin"))
        .ok()
        .map(|s| s.trim().to_string());
    crate::commands::stream::capture_project(&session, worktree.as_deref())
}

/// Send one queued event to the endpoint its queue and the session's capture
/// project select: a repo queue with a capture project goes to the
/// project-scoped endpoint carrying the repo id, one without goes to the
/// repo-scoped endpoint (the server deduces). A refused project-scoped send
/// is reported as [`QueuedSend::Refused`] and is never retried at the
/// repo-scoped endpoint — that would be the silent re-attribution VIS-316
/// removes. A repo-less queue drains to the project endpoint with no repo id,
/// the same shape the stream hook buffered it as, and keeps its old rules.
pub(crate) async fn send_queued_event(
    client: &ApiClient,
    target: &QueueTarget,
    capture_pid: Option<uuid::Uuid>,
    event: &StreamEventRequest,
) -> QueuedSend {
    let (sent, refusable_pid) = match (target, capture_pid) {
        (QueueTarget::Repo(repo_id), None) => (client.stream_event(repo_id, event).await, None),
        (QueueTarget::Repo(repo_id), Some(pid)) => (
            client
                .stream_event_for_project(pid, Some(repo_id), event)
                .await,
            Some(pid),
        ),
        (QueueTarget::Project(pid), _) => (
            client.stream_event_for_project(*pid, None, event).await,
            None,
        ),
    };
    let e = match sent {
        Ok(_) => return QueuedSend::Sent,
        Err(e) => e,
    };
    // Refusal is checked BEFORE the 413 rule: that rule is a bare substring
    // match, and a refusal's body can name a UUID that contains "413".
    if let Some(pid) = refusable_pid {
        if let Some(kind) = crate::commands::stream::deterministic_client_error_kind(e.as_ref()) {
            return QueuedSend::Refused { pid, kind };
        }
    }
    let err_str = e.to_string();
    if err_str.contains("413") {
        QueuedSend::TooLarge
    } else {
        QueuedSend::Failed(err_str)
    }
}

/// Drain one queue file: send each event, re-enqueue whatever must be
/// retried (in order, to the SAME file), and return `(sent, failed)`. A
/// refusal stops the drain: its line is printed once, and the refused event
/// plus every event not yet attempted are re-enqueued and counted failed.
async fn drain_queue(
    client: &ApiClient,
    pending_path: &Path,
    target: &QueueTarget,
    capture_pid: Option<uuid::Uuid>,
) -> Result<(u64, u64), Box<dyn std::error::Error>> {
    let events = drain_pending(pending_path)?;
    if events.is_empty() {
        return Ok((0, 0));
    }

    let event_total = events.len();
    let mut sent = 0u64;
    let mut failed = 0u64;
    let mut failed_events: Vec<StreamEventRequest> = Vec::new();

    // Label the progress and warning lines with the session directory the
    // queue lives in. It is the same id the hook stamped on every event here
    // (`run_stream` writes the queue under `.tracevault/sessions/<id>/`), but
    // taken from the path rather than the payload: the label is a directory
    // name, not data read out of the event.
    let queue_dir_name = pending_path
        .parent()
        .and_then(|d| d.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("?");

    let mut events = events.into_iter().enumerate();
    while let Some((i, mut event)) = events.next() {
        eprint!(
            "\r  Session {} — event {}/{} ...",
            display_prefix(queue_dir_name),
            i + 1,
            event_total
        );
        event.truncate_large_fields();
        match send_queued_event(client, target, capture_pid, &event).await {
            QueuedSend::Sent => sent += 1,
            QueuedSend::TooLarge => {
                // Payload too large even after truncation — drop it.
                eprintln!();
                eprintln!(
                    "  Warning: dropped event (session {queue_dir_name}) — still too large after truncation"
                );
                failed += 1;
            }
            QueuedSend::Failed(e) => {
                eprintln!();
                eprintln!("  Warning: failed to send event (session {queue_dir_name}): {e}");
                failed_events.push(event);
                failed += 1;
            }
            QueuedSend::Refused { pid, kind } => {
                eprintln!();
                eprintln!("{}", crate::commands::stream::refused_error(pid, &kind));
                failed_events.push(event);
                failed_events.extend(events.by_ref().map(|(_, e)| e));
                // Everything not sent is failed: earlier failures, the refused
                // event, and the untried rest.
                failed = event_total as u64 - sent;
                break;
            }
        }
    }
    eprintln!();

    // Re-enqueue retryable events (not 413s) to the SAME per-target file.
    if !failed_events.is_empty() {
        append_pending(pending_path, &failed_events)?;
    }
    Ok((sent, failed))
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
        let session_dir = session_entry.path();
        // Collect (path, QueueTarget) pairs for every pending queue in this
        // session directory before draining, to keep the borrow/async loop
        // below simple.
        let pending_queues = pending_queues_in(&session_dir, bound_repo_id.as_deref())?;

        for (pending_path, target) in pending_queues {
            // Resolved once per queue, not per event.
            let capture_pid = queue_capture_project(&session_dir, &target);
            let (sent, failed) = drain_queue(&client, &pending_path, &target, capture_pid).await?;
            total_sent += sent;
            total_failed += failed;
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
    use super::{display_prefix, pending_queues_in, repo_id_from_pending_filename, QueueTarget};
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

    // ── display_prefix: display-only truncation, no panic on short ids ─────

    #[test]
    fn display_prefix_truncates_long_id() {
        assert_eq!(
            display_prefix("0190a1b2-cccc-dddd-eeee-ffffffffffff"),
            "0190a1b2"
        );
    }

    #[test]
    fn display_prefix_returns_whole_string_when_shorter_than_8() {
        assert_eq!(display_prefix("x"), "x");
    }

    #[test]
    fn display_prefix_handles_empty_string() {
        assert_eq!(display_prefix(""), "");
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

/// VIS-316: `flush` must drain a repo queue under the session's capture
/// project, never re-attributing a refused event via the repo-scoped endpoint.
#[cfg(test)]
mod send_tests {
    use super::*;
    use crate::test_helpers::{http_json, lock_env_mutation, spawn_seq, EnvVarGuard, RECV_TIMEOUT};
    use std::time::Duration;
    use tracevault_protocol::streaming::StreamEventType;

    const REPO: &str = "11111111-1111-1111-1111-111111111111";

    fn event(n: u128) -> StreamEventRequest {
        StreamEventRequest {
            protocol_version: 1,
            tool: Some("claude-code".to_string()),
            event_type: StreamEventType::ToolUse,
            session_id: "sess-flush".into(),
            timestamp: chrono::Utc::now(),
            hook_event_name: Some("PostToolUse".into()),
            tool_name: None,
            tool_use_id: None,
            tool_input: None,
            tool_response: None,
            tool_is_error: None,
            event_index: None,
            event_uuid: Some(uuid::Uuid::from_u128(n)),
            transcript_lines: None,
            transcript_offset: None,
            model: None,
            cwd: None,
            final_stats: None,
            flow_id: None,
            labels: None,
            params: None,
        }
    }

    fn ok() -> String {
        let body = serde_json::to_string(&tracevault_protocol::streaming::StreamEventResponse {
            session_db_id: uuid::Uuid::nil(),
            event_db_id: Some(uuid::Uuid::nil()),
            status: "accepted".to_string(),
        })
        .unwrap();
        http_json("200 OK", &body)
    }

    fn bad_request() -> String {
        http_json(
            "400 Bad Request",
            r#"{"error":"repo is not a member of project"}"#,
        )
    }

    /// The request line of a captured request (`spawn_seq` renders
    /// `"<request line> | <headers> | <body>"`).
    fn request_line(captured: &str) -> &str {
        captured.split(" | ").next().unwrap_or_default()
    }

    /// Event uuids held by a queue file, in order; every line must parse.
    fn queued_uuids(path: &Path) -> Vec<uuid::Uuid> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str::<StreamEventRequest>(l)
                    .expect("every queued line parses")
                    .event_uuid
                    .unwrap()
            })
            .collect()
    }

    #[tokio::test]
    async fn repo_queue_with_capture_project_goes_to_the_project_endpoint() {
        let (base, rx) = spawn_seq(vec![ok()]);
        let client = ApiClient::new(&base, Some("tok"));
        let pid = uuid::Uuid::from_u128(0xA);

        let got = send_queued_event(
            &client,
            &QueueTarget::Repo(REPO.into()),
            Some(pid),
            &event(1),
        )
        .await;
        assert!(matches!(got, QueuedSend::Sent), "{got:?}");

        let captured = rx.recv_timeout(RECV_TIMEOUT).expect("no request captured");
        assert!(
            request_line(&captured).starts_with(&format!(
                "POST /api/v1/projects/{pid}/stream?repo_id={REPO} "
            )),
            "got: {captured}"
        );
    }

    #[tokio::test]
    async fn repo_queue_without_capture_project_goes_to_the_repo_endpoint() {
        let (base, rx) = spawn_seq(vec![ok()]);
        let client = ApiClient::new(&base, Some("tok"));

        let got =
            send_queued_event(&client, &QueueTarget::Repo(REPO.into()), None, &event(1)).await;
        assert!(matches!(got, QueuedSend::Sent), "{got:?}");

        let captured = rx.recv_timeout(RECV_TIMEOUT).expect("no request captured");
        assert!(
            request_line(&captured).starts_with(&format!("POST /api/v1/repos/{REPO}/stream ")),
            "got: {captured}"
        );
    }

    /// A 400 on the project-scoped send is a refusal: exactly one request (a
    /// spare 200 is staged so a repo-scoped retry WOULD be captured), and the
    /// outcome says so, which is what makes `drain_queue` re-enqueue it.
    #[tokio::test]
    async fn refused_project_send_makes_one_request_and_is_classified_refused() {
        use crate::commands::stream::ClientErrorKind;

        let (base, rx) = spawn_seq(vec![bad_request(), ok()]);
        let client = ApiClient::new(&base, Some("tok"));
        let pid = uuid::Uuid::from_u128(0xB);

        let got = send_queued_event(
            &client,
            &QueueTarget::Repo(REPO.into()),
            Some(pid),
            &event(1),
        )
        .await;
        assert!(
            matches!(
                got,
                QueuedSend::Refused { pid: p, kind: ClientErrorKind::Scoping } if p == pid
            ),
            "{got:?}"
        );

        let first = rx.recv_timeout(RECV_TIMEOUT).expect("no request captured");
        assert!(
            request_line(&first).contains(&format!("/projects/{pid}/stream?repo_id={REPO}")),
            "got: {first}"
        );
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "a refused event must not be re-sent to the repo-scoped endpoint"
        );
    }

    /// A transient failure keeps today's per-event retry: `Failed`, not
    /// `Refused`, so the drain goes on to the next event.
    #[tokio::test]
    async fn transient_project_send_failure_is_not_a_refusal() {
        let (base, _rx) = spawn_seq(vec![http_json("503 Service Unavailable", "{}")]);
        let client = ApiClient::new(&base, Some("tok"));

        let got = send_queued_event(
            &client,
            &QueueTarget::Repo(REPO.into()),
            Some(uuid::Uuid::from_u128(0xC)),
            &event(1),
        )
        .await;
        assert!(matches!(got, QueuedSend::Failed(_)), "{got:?}");
    }

    /// A refusal mid-queue stops the drain: the events already sent stay
    /// sent, and the refused event plus every untried one are re-enqueued in
    /// their original order and counted failed. The third event is never
    /// attempted (the spare response stays unclaimed).
    #[tokio::test]
    async fn drain_queue_stops_at_a_refusal_and_requeues_the_rest_in_order() {
        let (base, rx) = spawn_seq(vec![ok(), bad_request(), ok()]);
        let client = ApiClient::new(&base, Some("tok"));
        let pid = uuid::Uuid::from_u128(0xD);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("pending-{REPO}.jsonl"));
        append_pending(&path, &[event(1), event(2), event(3)]).unwrap();

        let (sent, failed) =
            drain_queue(&client, &path, &QueueTarget::Repo(REPO.into()), Some(pid))
                .await
                .unwrap();

        assert_eq!((sent, failed), (1, 2));
        assert_eq!(
            queued_uuids(&path),
            vec![uuid::Uuid::from_u128(2), uuid::Uuid::from_u128(3)]
        );
        for _ in 0..2 {
            let captured = rx.recv_timeout(RECV_TIMEOUT).expect("request missing");
            assert!(
                request_line(&captured).contains(&format!("/projects/{pid}/stream?repo_id=")),
                "got: {captured}"
            );
        }
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "no request after the refusal"
        );
    }

    /// End to end through `run_flush`: the capture project comes from the
    /// session state the hook reads (`XDG_STATE_HOME`, keyed by the session
    /// directory's name), and a refusal leaves every event in the queue.
    #[tokio::test]
    async fn run_flush_applies_the_session_capture_project_and_keeps_refused_events() {
        let _env_lock = lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let mut guard = EnvVarGuard::new();
        guard.set("XDG_STATE_HOME", tmp.path().join("state"));
        guard.set("XDG_CONFIG_HOME", tmp.path().join("config"));
        guard.set("HOME", tmp.path());

        let (base, rx) = spawn_seq(vec![bad_request(), ok()]);
        guard.set("TRACEVAULT_SERVER_URL", &base);
        guard.set("TRACEVAULT_API_KEY", "tvk_test");

        let session_id = "flush-session-1";
        let pid = uuid::Uuid::from_u128(0xE);
        crate::session_state::save(
            session_id,
            &crate::session_state::SessionState {
                active_project: Some(crate::session_state::ProjectBinding {
                    project_id: pid.to_string(),
                    project_name: "p".into(),
                    updated_at: String::new(),
                }),
                ..Default::default()
            },
        )
        .unwrap();

        let session_dir = root.join(".tracevault").join("sessions").join(session_id);
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("origin"),
            root.to_string_lossy().as_bytes(),
        )
        .unwrap();
        let queue = session_dir.join(format!("pending-{REPO}.jsonl"));
        append_pending(&queue, &[event(1), event(2)]).unwrap();

        run_flush(&root).await.unwrap();

        let first = rx.recv_timeout(RECV_TIMEOUT).expect("no request captured");
        assert!(
            request_line(&first).starts_with(&format!(
                "POST /api/v1/projects/{pid}/stream?repo_id={REPO} "
            )),
            "got: {first}"
        );
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "exactly one request: no repo-scoped fallback, no second event"
        );
        assert_eq!(
            queued_uuids(&queue),
            vec![uuid::Uuid::from_u128(1), uuid::Uuid::from_u128(2)]
        );
    }
}
