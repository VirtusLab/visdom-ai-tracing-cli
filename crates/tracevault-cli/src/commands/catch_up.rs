//! The catch-up sweep for Claude Code's `cost-state` report (VIS-1108).
//!
//! Claude Code writes a session's cost report, a `cost-state` transcript
//! line, at exit, after `Stop`; the `SessionEnd` hook sends it. When that
//! hook failed or timed out, or was not installed yet (sessions that ended
//! before the upgrade that adds it), the line stays on disk unsent.
//! `session-start` runs [`sweep`] for its repository: a tracked session
//! (`.tracevault/sessions/*`) whose main transcript has unsent lines, and
//! whose unsent lines hold a `cost-state`, gets them sent the way its
//! `SessionEnd` hook would have. Unsent lines without one are left to the
//! session's own hooks: it may still be running.
//!
//! Only main transcripts are read: subagent files never hold a `cost-state`.
//!
//! The sweep must never slow a session start much or fail it, so it is
//! capped: transcripts written in the last [`MAX_AGE`], the [`MAX_SCANNED`]
//! most recent of them read, at most [`MAX_SESSIONS`] sent per run, at most
//! [`MAX_UNSENT_BYTES`] unsent per session, and no file read or request
//! started after its [`deadline`]. What a run leaves is picked up by the next
//! session start, while it is within the caps. Errors go to stderr.
//!
//! It does not run when `TRACEVAULT_PROJECT` or
//! `TRACEVAULT_PROJECT_ATTRIBUTION` is set: they attribute what this process
//! sends, and an earlier session's process may have had other values. For the
//! same reason a session is sent only under the attribution its own hooks
//! recorded (see [`stream::catch_up_may_send`]).

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::commands::stream::{self, SessionTarget};

/// How far back the sweep looks: Claude Code deletes transcripts older than
/// this by default (`cleanupPeriodDays`), so older sessions are mostly gone.
pub const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Sessions sent per run, so one session start sends at most this many.
pub const MAX_SESSIONS: usize = 10;

/// Sessions with unsent lines read per run, most recent first. Most have no
/// report (killed sessions, or sessions still running), and they stay
/// unsent, so without this cap every session start would read them all.
pub const MAX_SCANNED: usize = 50;

/// A session with more unsent than this lost much more than its report
/// (every hook of it failed), and reading it on every session start would
/// cost too much; the sweep leaves it.
pub const MAX_UNSENT_BYTES: u64 = 8 * 1024 * 1024;

/// The sweep's own time budget, counted from its start.
pub const BUDGET: Duration = Duration::from_secs(3);

/// The latest the sweep may start anything, counted from the start of the
/// `SessionStart` hook: its 10 s timeout, less a request's in-flight grace
/// and a second for the rest. A hook killed at its timeout fails.
const HOOK_LATEST: Duration = Duration::from_secs(10)
    .saturating_sub(stream::IN_FLIGHT_GRACE)
    .saturating_sub(Duration::from_secs(1));

/// When a sweep starting `now`, in a hook that started at `hook_started`,
/// must stop starting work: [`BUDGET`] from now, but never past
/// [`HOOK_LATEST`] into the hook.
pub fn deadline(hook_started: Instant, now: Instant) -> Instant {
    (now + BUDGET).min(hook_started + HOOK_LATEST)
}

/// The cost-state line's type, as Claude Code writes it.
const MARKER: &[u8] = br#""cost-state""#;

/// A session the sweep sends the rest of.
#[derive(Debug, PartialEq, Eq)]
pub struct Candidate {
    pub session_id: String,
    pub session_dir: PathBuf,
    pub transcript: PathBuf,
    /// The checkout the session ran in, or the project root when that
    /// checkout is gone (e.g. a removed worktree).
    pub worktree_top: String,
}

/// The sessions under `project_root` whose unsent transcript lines hold a
/// `cost-state`, most recently written first, within the caps; no transcript
/// is read after `deadline`. The current session is left out: its own hooks
/// send its lines.
pub fn candidates(
    project_root: &Path,
    current_session: &str,
    now: SystemTime,
    deadline: Instant,
) -> Vec<Candidate> {
    let sessions = project_root.join(".tracevault").join("sessions");
    let Ok(entries) = std::fs::read_dir(&sessions) else {
        return vec![];
    };
    // `.tracevault/sessions` is never pruned: even listing it is bounded by
    // the deadline.
    let mut unsent: Vec<(SystemTime, String, PathBuf, PathBuf, u64)> = entries
        .take_while(|_| Instant::now() < deadline)
        .flatten()
        .filter_map(|entry| {
            let session_id = entry.file_name().to_str()?.to_string();
            if session_id == current_session
                || !crate::session_state::is_safe_session_id(&session_id)
            {
                return None;
            }
            let session_dir = entry.path();
            let transcript = transcript_path(&session_dir)?;
            let offset = stream_offset(&session_dir)?;
            let meta = std::fs::metadata(&transcript).ok()?;
            let modified = meta.modified().ok()?;
            let recent = now
                .duration_since(modified)
                .map_or(true, |age| age <= MAX_AGE);
            let pending = meta.len().checked_sub(offset)?;
            (recent && pending > 0 && pending <= MAX_UNSENT_BYTES).then_some((
                modified,
                session_id,
                session_dir,
                transcript,
                offset,
            ))
        })
        .collect();
    unsent.sort_by_key(|u| std::cmp::Reverse(u.0));
    unsent
        .into_iter()
        .take(MAX_SCANNED)
        .take_while(|_| Instant::now() < deadline)
        .filter(|(_, _, _, transcript, offset)| unsent_has_cost_state(transcript, *offset))
        .take(MAX_SESSIONS)
        .map(|(_, session_id, session_dir, transcript, _)| Candidate {
            worktree_top: worktree_top(&session_dir, project_root),
            session_id,
            session_dir,
            transcript,
        })
        .collect()
}

/// Send the rest of every [`candidates`] session of `project_root`, as its
/// `SessionEnd` hook would, starting nothing after `deadline`. Stops at the
/// first session the server did not take in full: the rest wait for the
/// next session start.
pub async fn sweep(project_root: PathBuf, current_session: String, deadline: Instant) {
    if env_sets_attribution(|name| std::env::var_os(name)) {
        return;
    }
    for c in candidates(&project_root, &current_session, SystemTime::now(), deadline) {
        if Instant::now() >= deadline {
            break;
        }
        let event = tracevault_protocol::hooks::HookEvent {
            session_id: c.session_id.clone(),
            transcript_path: c.transcript.to_string_lossy().into_owned(),
            cwd: c.worktree_top.clone(),
            permission_mode: None,
            hook_event_name: "SessionEnd".to_string(),
            tool_name: None,
            tool_input: None,
            tool_response: None,
            tool_use_id: None,
            source: None,
            transcript_records: None,
        };
        let target = SessionTarget {
            project_root: project_root.clone(),
            session_dir: c.session_dir,
            worktree_top: c.worktree_top,
        };
        let sent = stream::stream_event(
            event,
            "session-end",
            crate::agent::Agent::ClaudeCode,
            &target,
            deadline,
            stream::Sender::CatchUp,
        )
        .await;
        match sent {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                eprintln!(
                    "tracevault: cannot send the end of session {}: {e}",
                    c.session_id
                );
                break;
            }
        }
    }
}

/// The transcript the session's hooks recorded in its `metadata.json`.
fn transcript_path(session_dir: &Path) -> Option<PathBuf> {
    let meta = std::fs::read_to_string(session_dir.join("metadata.json")).ok()?;
    let meta: serde_json::Value = serde_json::from_str(&meta).ok()?;
    let path = meta.get("transcript_path")?.as_str()?;
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// How far the session's hooks sent its main transcript. A session with no
/// offset never had a send accepted; the sweep leaves it to its own queue.
fn stream_offset(session_dir: &Path) -> Option<u64> {
    std::fs::read_to_string(session_dir.join(".stream_offset"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Whether this process's environment attributes what it sends (see the
/// module doc). `var` reads a variable: the environment, or a test's.
fn env_sets_attribution(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    ["TRACEVAULT_PROJECT", "TRACEVAULT_PROJECT_ATTRIBUTION"]
        .iter()
        .any(|name| var(name).is_some_and(|v| !v.is_empty()))
}

/// Whether the lines past `offset` hold a `cost-state`. Only the lines that
/// contain its type are parsed, so a long tail without one costs one read.
fn unsent_has_cost_state(transcript: &Path, offset: u64) -> bool {
    let mut unsent = Vec::new();
    let read = std::fs::File::open(transcript).and_then(|mut f| {
        f.seek(SeekFrom::Start(offset))?;
        f.take(MAX_UNSENT_BYTES).read_to_end(&mut unsent)
    });
    read.is_ok()
        && unsent
            .split(|b| *b == b'\n')
            .filter(|line| line.windows(MARKER.len()).any(|w| w == MARKER))
            .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
            .any(|l| l.get("type").and_then(|t| t.as_str()) == Some("cost-state"))
}

/// The checkout the session ran in (its `origin` marker), while it exists.
fn worktree_top(session_dir: &Path, project_root: &Path) -> String {
    std::fs::read_to_string(session_dir.join("origin"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && Path::new(s).is_dir())
        .unwrap_or_else(|| project_root.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const COST_STATE: &str = r#"{"type":"cost-state","totalCostUSD":0.12}"#;
    const ASSISTANT: &str = r#"{"type":"assistant","message":{"id":"m1"}}"#;

    struct Repo {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        transcripts: PathBuf,
    }

    fn repo() -> Repo {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let transcripts = tmp.path().join("claude-project");
        fs::create_dir_all(root.join(".tracevault/sessions")).unwrap();
        fs::create_dir_all(&transcripts).unwrap();
        Repo {
            _tmp: tmp,
            root,
            transcripts,
        }
    }

    impl Repo {
        /// A tracked session whose transcript holds `sent` (already sent, the
        /// offset points past them) and then `unsent`.
        fn session(&self, id: &str, sent: &[&str], unsent: &[&str]) -> PathBuf {
            let dir = self.root.join(".tracevault/sessions").join(id);
            fs::create_dir_all(&dir).unwrap();
            let transcript = self.transcripts.join(format!("{id}.jsonl"));
            let sent: String = sent.iter().map(|l| format!("{l}\n")).collect();
            let unsent: String = unsent.iter().map(|l| format!("{l}\n")).collect();
            fs::write(&transcript, format!("{sent}{unsent}")).unwrap();
            fs::write(dir.join(".stream_offset"), sent.len().to_string()).unwrap();
            fs::write(
                dir.join("metadata.json"),
                serde_json::json!({"transcript_path": transcript}).to_string(),
            )
            .unwrap();
            transcript
        }

        fn ids(&self, current: &str, now: SystemTime) -> Vec<String> {
            candidates(&self.root, current, now, later())
                .into_iter()
                .map(|c| c.session_id)
                .collect()
        }
    }

    /// A deadline no test reaches.
    fn later() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    fn touch(path: &Path, modified: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    }

    #[test]
    fn an_unsent_cost_state_makes_a_candidate() {
        let r = repo();
        let transcript = r.session("s1", &[ASSISTANT], &[COST_STATE]);
        let found = candidates(&r.root, "other", SystemTime::now(), later());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, "s1");
        assert_eq!(found[0].transcript, transcript);
        assert_eq!(
            found[0].worktree_top,
            r.root.to_string_lossy(),
            "no origin marker: the project root"
        );
    }

    #[test]
    fn lines_after_the_cost_state_do_not_hide_it() {
        let r = repo();
        r.session("s1", &[ASSISTANT], &[COST_STATE, ASSISTANT]);
        assert_eq!(r.ids("other", SystemTime::now()), ["s1"]);
    }

    #[test]
    fn unsent_lines_without_a_cost_state_are_left_to_the_session() {
        let r = repo();
        r.session("running", &[ASSISTANT], &[ASSISTANT]);
        assert!(r.ids("other", SystemTime::now()).is_empty());
    }

    #[test]
    fn a_cost_state_already_sent_is_not_sent_again() {
        let r = repo();
        r.session("done", &[ASSISTANT, COST_STATE], &[]);
        r.session("resumed", &[COST_STATE], &[ASSISTANT]);
        assert!(r.ids("other", SystemTime::now()).is_empty());
    }

    #[test]
    fn the_current_session_is_left_to_its_own_hooks() {
        let r = repo();
        r.session("current", &[], &[COST_STATE]);
        assert!(r.ids("current", SystemTime::now()).is_empty());
    }

    #[test]
    fn subagent_files_are_never_read() {
        let r = repo();
        r.session("s1", &[ASSISTANT], &[ASSISTANT]);
        let subagents = r.transcripts.join("s1").join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        fs::write(subagents.join("agent-a.jsonl"), format!("{COST_STATE}\n")).unwrap();
        assert!(r.ids("other", SystemTime::now()).is_empty());
    }

    #[test]
    fn a_session_without_an_offset_or_transcript_is_skipped() {
        let r = repo();
        r.session("no-offset", &[], &[COST_STATE]);
        fs::remove_file(r.root.join(".tracevault/sessions/no-offset/.stream_offset")).unwrap();
        let gone = r.session("gone", &[], &[COST_STATE]);
        fs::remove_file(gone).unwrap();
        r.session("bad-offset", &[], &[COST_STATE]);
        fs::write(
            r.root
                .join(".tracevault/sessions/bad-offset/.stream_offset"),
            "garbage",
        )
        .unwrap();
        fs::create_dir_all(r.root.join(".tracevault/sessions/no-metadata")).unwrap();
        assert!(r.ids("other", SystemTime::now()).is_empty());
    }

    #[test]
    fn sessions_older_than_the_cap_are_skipped() {
        let r = repo();
        let old = r.session("old", &[], &[COST_STATE]);
        let now = SystemTime::now();
        touch(&old, now - MAX_AGE - Duration::from_secs(60));
        r.session("new", &[], &[COST_STATE]);
        assert_eq!(r.ids("other", now), ["new"]);
    }

    #[test]
    fn at_most_max_sessions_most_recent_first() {
        let r = repo();
        let now = SystemTime::now();
        for i in 0..MAX_SESSIONS + 2 {
            let t = r.session(&format!("s{i:02}"), &[], &[COST_STATE]);
            touch(&t, now - Duration::from_secs(1000 - i as u64));
        }
        let ids = r.ids("other", now);
        assert_eq!(ids.len(), MAX_SESSIONS);
        assert_eq!(ids[0], format!("s{:02}", MAX_SESSIONS + 1), "newest first");
        assert!(!ids.contains(&"s00".to_string()), "oldest left out");
    }

    #[test]
    fn a_session_with_too_much_unsent_is_skipped() {
        let r = repo();
        let big = format!(
            r#"{{"type":"assistant","pad":"{}"}}"#,
            "x".repeat(MAX_UNSENT_BYTES as usize)
        );
        r.session("big", &[], &[&big, COST_STATE]);
        assert!(r.ids("other", SystemTime::now()).is_empty());
    }

    #[test]
    fn an_existing_origin_is_the_worktree_and_a_removed_one_is_not() {
        let r = repo();
        r.session("here", &[], &[COST_STATE]);
        r.session("removed", &[], &[COST_STATE]);
        let worktree = r.transcripts.parent().unwrap().join("wt");
        fs::create_dir_all(&worktree).unwrap();
        let sessions = r.root.join(".tracevault/sessions");
        fs::write(
            sessions.join("here/origin"),
            worktree.to_string_lossy().as_bytes(),
        )
        .unwrap();
        fs::write(sessions.join("removed/origin"), "/nonexistent/worktree").unwrap();
        let found = candidates(&r.root, "other", SystemTime::now(), later());
        let top = |id: &str| {
            found
                .iter()
                .find(|c| c.session_id == id)
                .unwrap()
                .worktree_top
                .clone()
        };
        assert_eq!(top("here"), worktree.to_string_lossy());
        assert_eq!(top("removed"), r.root.to_string_lossy());
    }

    #[test]
    fn no_sessions_dir_means_nothing_to_do() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(candidates(tmp.path(), "x", SystemTime::now(), later()).is_empty());
    }

    #[test]
    fn only_the_most_recent_unsent_sessions_are_read() {
        let r = repo();
        let now = SystemTime::now();
        // The oldest holds a report, behind MAX_SCANNED newer sessions with
        // unsent lines and none (killed, or running).
        let reported = r.session("reported", &[], &[COST_STATE]);
        touch(&reported, now - Duration::from_secs(10_000));
        for i in 0..MAX_SCANNED {
            let t = r.session(&format!("k{i:02}"), &[], &[ASSISTANT]);
            touch(&t, now - Duration::from_secs(1000 - i as u64));
        }
        assert!(r.ids("other", now).is_empty());
    }

    #[test]
    fn nothing_is_read_past_the_deadline() {
        let r = repo();
        r.session("s1", &[], &[COST_STATE]);
        let found = candidates(&r.root, "other", SystemTime::now(), Instant::now());
        assert!(found.is_empty());
    }

    #[test]
    fn the_cost_state_type_must_be_the_line_type() {
        let r = repo();
        // The marker inside text, e.g. a prompt quoting it.
        r.session(
            "quoted",
            &[],
            &[r#"{"type":"user","message":{"content":"what is \"cost-state\"?"}}"#],
        );
        assert!(r.ids("other", SystemTime::now()).is_empty());
    }

    #[test]
    fn the_sweep_stays_inside_the_session_start_timeout() {
        let started = Instant::now();
        // Early in the hook: its own budget.
        assert_eq!(deadline(started, started), started + BUDGET);
        // Late in the hook (slow policy fetch): capped by the hook's start.
        let late = started + Duration::from_secs(8);
        assert_eq!(deadline(started, late), started + HOOK_LATEST);
        assert!(HOOK_LATEST + stream::IN_FLIGHT_GRACE < Duration::from_secs(10));
    }

    #[test]
    fn a_process_scoped_project_stops_the_sweep() {
        use std::ffi::OsString;
        let env = |set: &'static str, value: &'static str| {
            move |name: &str| (name == set).then(|| OsString::from(value))
        };
        assert!(env_sets_attribution(env("TRACEVAULT_PROJECT", "p")));
        assert!(env_sets_attribution(env(
            "TRACEVAULT_PROJECT_ATTRIBUTION",
            "explicit"
        )));
        assert!(!env_sets_attribution(env("TRACEVAULT_PROJECT", "")));
        assert!(!env_sets_attribution(|_: &str| None));
    }
}
