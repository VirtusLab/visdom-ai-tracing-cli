//! Per-session workspace-mode state: which repo a roaming/detached Claude Code
//! session is currently bound to. Persisted outside any repo so it survives a
//! session that changes directories. Set by `tracevault repo switch`
//! (sub-plan B); read by `stream`/commands as a resolution fallback.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A resolved binding to a registered repo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoBinding {
    pub repo_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<uuid::Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codebase_name: Option<String>,
    pub updated_at: String,
}

/// A resolved binding to a registered project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectBinding {
    pub project_id: String,
    pub project_name: String,
    pub updated_at: String,
    /// RFC3339 instant after which the FORCE lapses (the binding itself
    /// survives). Only ever set by `project switch --project-attribution
    /// explicit`, which persists to disk and can therefore be forgotten.
    /// Environment-provided force is exempt: it is re-asserted at every launch
    /// by construction, so it cannot go stale and needs no expiry.
    /// Interpreted by exactly one function, [`force_status`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forced_until: Option<String>,
}

/// What a [`ProjectBinding::forced_until`] currently says about its force.
///
/// Lives here, beside the field it interprets, because both readers of that
/// field are in `commands::` and this module depends on neither — so one
/// place knows the stored format and both can reach it with no cycle.
///
/// There used to be two readers and no shared type:
/// `commands::stream::attribution_mode` folded the question into a bool while
/// `commands::project::format_force_line` did its own RFC3339 parse and
/// `Utc::now()` comparison to choose between its active / lapsed / unreadable
/// wordings — and a test existed purely to pin that the two independently
/// written predicates agreed on the unparseable case, which is itself the
/// evidence they could have drifted. That test now pins one implementation
/// rather than reconciling two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForceStatus {
    /// Parsed, and still in the future: the force applies.
    Live(chrono::DateTime<chrono::Utc>),
    /// Parsed, but already past — the FORCE lapsed; the binding survives.
    Lapsed(chrono::DateTime<chrono::Utc>),
    /// Not RFC3339 at all (a hand-edited or corrupted state file).
    Unreadable,
}

/// Classify a stored `forced_until` — see [`ForceStatus`].
///
/// [`ForceStatus::Unreadable`] is the fail-SAFE direction, and both callers
/// depend on it: "can't tell" means NOT in force, never `expect()`-worthy.
/// Not sending an `explicit` header IS the fallback, so ingest never starts
/// failing because a force was forgotten or a timestamp got mangled.
///
/// A parsed instant is normalised to UTC so both arms can carry one type.
/// `project switch` only ever writes `Utc::now() + …`, so in practice this
/// changes nothing that is rendered back to the operator.
pub fn force_status(forced_until: &str) -> ForceStatus {
    match chrono::DateTime::parse_from_rfc3339(forced_until) {
        Ok(until) => {
            let until = until.with_timezone(&chrono::Utc);
            if until > chrono::Utc::now() {
                ForceStatus::Live(until)
            } else {
                ForceStatus::Lapsed(until)
            }
        }
        Err(_) => ForceStatus::Unreadable,
    }
}

/// Session-level active binding plus per-worktree subagent overrides
/// (keyed by the subagent's git worktree toplevel path — design §7).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SessionState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<RepoBinding>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub subagents: HashMap<String, RepoBinding>,
    /// The repo id whose policies were last injected into a hook response for
    /// this session (sub-plan C). Used by `UserPromptSubmit` to avoid
    /// re-injecting unchanged context on every prompt. `#[serde(default)]`
    /// keeps old on-disk session files (without this field) loadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_injected_repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_project: Option<ProjectBinding>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub subagent_projects: HashMap<String, ProjectBinding>,
    /// Origin remote URLs this session already looked up without binding
    /// (unregistered, or ambiguous), so `auto_bind` asks the server once per
    /// remote, not once per hook event — and says so once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_bind_checked: Vec<String>,
    /// Set by `repo reset`: the session stays unbound instead of being bound
    /// again from its checkout's remote on the next hook event.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_bind_off: bool,
    /// The directory holding the `.tracevault/` this session keeps its
    /// session dir (`.stream_offset`, pending queues, `origin`) in, recorded
    /// once by the `SessionStart` hook from where the session started
    /// (VIS-1257). Claude Code's hook `cwd` follows the shell's `cd`, so
    /// resolving it per event split a session that moved from an umbrella
    /// dir into a child repo across two session dirs, each re-sending the
    /// transcript from its own offset. Absent for sessions started before
    /// this was recorded, and for agents without a `SessionStart` hook: those
    /// keep resolving from each event's `cwd`. Read through [`storage_root`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_root: Option<String>,
    /// What each checkout this session touched resolves to for a project-only
    /// session's events (VIS-1258), keyed by the checkout's git toplevel, so
    /// the server is asked once per checkout and not once per event. Read and
    /// written only through `crate::inferred_repo`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub inferred_repos: std::collections::BTreeMap<String, InferredRepo>,
}

/// One [`SessionState::inferred_repos`] entry: the repo a checkout resolved to
/// within `project_id`, or none. A `None` repo is remembered only until
/// `retry_after` (RFC3339), so a checkout registered or added to the project
/// later is picked up without asking on every event. A found repo has no
/// expiry: it is a fact about the server that changes only by a re-register.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InferredRepo {
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoBinding>,
}

/// The session's recorded [`SessionState::storage_root`], if that directory
/// still exists. Its `.tracevault/` may not exist yet: a git root is recorded
/// whether or not it has one, and the stream hook creates it on first use,
/// exactly as it would for an event whose `cwd` resolved there.
pub fn storage_root(state: &SessionState) -> Option<PathBuf> {
    let root = PathBuf::from(state.storage_root.as_deref()?);
    root.is_dir().then_some(root)
}

/// Inside an anchored session dir: every worktree toplevel the session ran
/// an event in, one per line. The `origin` marker names only the latest, and
/// `check` / `verify-start` in a checkout must still find a session that
/// worked there and then moved on — as they did when the session kept a
/// second dir in that checkout.
pub const WORKTREES_FILE: &str = "worktrees";

/// Add `worktree_top` to `session_dir`'s [`WORKTREES_FILE`] unless listed.
pub fn record_worktree(session_dir: &std::path::Path, worktree_top: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let path = session_dir.join(WORKTREES_FILE);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        if existing.lines().any(|l| l == worktree_top) {
            return Ok(());
        }
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{worktree_top}")
}

/// Whether the session in `session_dir` ran an event in `worktree_top`: its
/// `origin` marker names it, or its [`WORKTREES_FILE`] lists it.
pub fn ran_in(session_dir: &std::path::Path, worktree_top: &str) -> bool {
    let names = |file: &str, all_lines: bool| {
        std::fs::read_to_string(session_dir.join(file))
            .map(|s| {
                if all_lines {
                    s.lines().any(|l| l == worktree_top)
                } else {
                    s.trim() == worktree_top
                }
            })
            .unwrap_or(false)
    };
    names("origin", false) || names(WORKTREES_FILE, true)
}

/// The dirs of sessions anchored OUTSIDE `local_sessions_dir` (see
/// [`SessionState::storage_root`]) that ran an event in `worktree_top`, read
/// from the per-session state files in `state_dir`. A session started in an
/// umbrella dir keeps its dir there, so the checkout it worked in has to look
/// it up this way. Sorted; unreadable entries are skipped.
pub fn anchored_session_dirs_in(
    state_dir: &std::path::Path,
    local_sessions_dir: &std::path::Path,
    worktree_top: &str,
) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(state_dir) else {
        return vec![];
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name();
            let id = name.to_str()?.strip_suffix(".toml")?.to_string();
            let root = storage_root(&load_in(state_dir, &id))?;
            let dir = root.join(".tracevault").join("sessions").join(&id);
            (dir.parent() != Some(local_sessions_dir) && dir.is_dir() && ran_in(&dir, worktree_top))
                .then_some(dir)
        })
        .collect();
    dirs.sort();
    dirs
}

/// [`anchored_session_dirs_in`] the user's state dir.
pub fn anchored_session_dirs(
    local_sessions_dir: &std::path::Path,
    worktree_top: &str,
) -> Vec<PathBuf> {
    sessions_dir()
        .map(|d| anchored_session_dirs_in(&d, local_sessions_dir, worktree_top))
        .unwrap_or_default()
}

/// `<root>/.tracevault/sessions/<session_id>` for `session_id`, where `root`
/// is the session's recorded [`storage_root`], else `fallback_root` (the root
/// resolved from the caller's working directory). Every reader and writer of
/// a session dir goes through this, so a session is found where its hook
/// keeps it.
pub fn session_dir(fallback_root: &std::path::Path, session_id: &str) -> PathBuf {
    let root = storage_root(&load(session_id)).unwrap_or_else(|| fallback_root.to_path_buf());
    root.join(".tracevault").join("sessions").join(session_id)
}

/// Record `root` as the session's [`SessionState::storage_root`] unless one
/// is already recorded: `SessionStart` fires again on resume, `/clear` and
/// compaction, from wherever the shell has moved, and the session dir must
/// not move with it. Returns whether it wrote.
pub fn record_storage_root(
    sessions_dir: &std::path::Path,
    session_id: &str,
    root: &std::path::Path,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut state = load_in(sessions_dir, session_id);
    if state.storage_root.is_some() {
        return Ok(false);
    }
    state.storage_root = Some(root.to_string_lossy().into_owned());
    save_in(sessions_dir, session_id, &state)?;
    Ok(true)
}

/// `$XDG_STATE_HOME/tracevault/sessions` or `~/.local/state/tracevault/sessions`.
pub fn sessions_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))?;
    Some(base.join("tracevault").join("sessions"))
}

/// The session of the agent that runs this command, from its environment:
/// `$TRACEVAULT_SESSION_ID` (exported by the Claude Code / Codex SessionStart
/// hook), else `$AMP_CURRENT_THREAD_ID`.
///
/// Amp's plugin cannot export a variable into Amp's shell (it runs in a
/// separate process), but that shell has the current thread id, and the Amp
/// plugin uses the thread id as the TraceVault session id (VIS-1199). Empty
/// values count as unset.
pub fn env_session_id() -> Option<String> {
    session_id_from_env(
        std::env::var("TRACEVAULT_SESSION_ID").ok(),
        std::env::var("AMP_CURRENT_THREAD_ID").ok(),
    )
}

/// [`env_session_id`] on given values: `tracevault` wins, then `amp_thread`.
pub fn session_id_from_env(
    tracevault: Option<String>,
    amp_thread: Option<String>,
) -> Option<String> {
    tracevault
        .filter(|s| !s.is_empty())
        .or_else(|| amp_thread.filter(|s| !s.is_empty()))
}

/// A session id is used to build the state file path, so it must be a safe
/// filename token — no path separators or `..` that could escape the sessions
/// directory. Claude Code session ids are UUIDs, which satisfy this.
pub(crate) fn is_safe_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn state_path_in(sessions_dir: &std::path::Path, session_id: &str) -> PathBuf {
    sessions_dir.join(format!("{session_id}.toml"))
}

fn load_in(sessions_dir: &std::path::Path, session_id: &str) -> SessionState {
    if !is_safe_session_id(session_id) {
        return SessionState::default();
    }
    match std::fs::read_to_string(state_path_in(sessions_dir, session_id)) {
        Ok(s) => toml::from_str(&s).unwrap_or_default(),
        Err(_) => SessionState::default(),
    }
}

fn save_in(
    sessions_dir: &std::path::Path,
    session_id: &str,
    state: &SessionState,
) -> Result<(), Box<dyn std::error::Error>> {
    if !is_safe_session_id(session_id) {
        return Err(format!("invalid session id: {session_id:?}").into());
    }
    std::fs::create_dir_all(sessions_dir)?;
    let final_path = state_path_in(sessions_dir, session_id);
    // Write to a temp file in the same dir, then atomically rename over the
    // target so a crash or a concurrent reader never sees a partial/empty file.
    // The temp name is unique per write: a session's parallel hooks save
    // concurrently, and a shared name let one writer rename the other's
    // half-written file into place.
    let tmp_path = sessions_dir.join(format!("{session_id}.toml.{}.tmp", uuid::Uuid::now_v7()));
    std::fs::write(&tmp_path, toml::to_string(state)?)?;
    std::fs::rename(&tmp_path, &final_path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp_path);
    })?;
    Ok(())
}

/// Load, change and save a session's state while holding an exclusive lock
/// on `<session_id>.lock` beside it, so the session's parallel hooks — each
/// recording what it looked up — do not overwrite each other's changes. The
/// lock is advisory: writers that do not take it (the interactive `switch`
/// commands) are as before. The OS releases it when the holder exits.
pub fn update_in(
    sessions_dir: &std::path::Path,
    session_id: &str,
    change: impl FnOnce(&mut SessionState),
) -> Result<SessionState, Box<dyn std::error::Error>> {
    if !is_safe_session_id(session_id) {
        return Err(format!("invalid session id: {session_id:?}").into());
    }
    std::fs::create_dir_all(sessions_dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(sessions_dir.join(format!("{session_id}.lock")))?;
    lock.lock()?;
    let mut state = load_in(sessions_dir, session_id);
    change(&mut state);
    save_in(sessions_dir, session_id, &state)?;
    Ok(state)
}

/// Load a session's state, returning the default (empty) state if the file is
/// absent or malformed (workspace state is best-effort, never fatal).
pub fn load(session_id: &str) -> SessionState {
    match sessions_dir() {
        Some(dir) => load_in(&dir, session_id),
        None => SessionState::default(),
    }
}

/// Persist a session's state, creating the state dir if needed.
pub fn save(session_id: &str, state: &SessionState) -> Result<(), Box<dyn std::error::Error>> {
    let dir = sessions_dir().ok_or("cannot determine state dir")?;
    save_in(&dir, session_id, state)
}

/// Load a session's state from an explicit sessions dir (test seam / callers
/// that resolve the dir themselves). Absent or malformed → default state.
pub fn load_from(sessions_dir: &std::path::Path, session_id: &str) -> SessionState {
    load_in(sessions_dir, session_id)
}

/// [`save`] into an explicit sessions dir — the write half of [`load_from`].
/// Hooks write through the locking [`update_in`] instead.
#[cfg(test)]
pub fn save_to(
    sessions_dir: &std::path::Path,
    session_id: &str,
    state: &SessionState,
) -> Result<(), Box<dyn std::error::Error>> {
    save_in(sessions_dir, session_id, state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_session_id_prefers_tracevault_then_amp_thread() {
        let id = |tv: Option<&str>, amp: Option<&str>| {
            session_id_from_env(tv.map(String::from), amp.map(String::from))
        };
        assert_eq!(id(Some("sess"), Some("T-1")).as_deref(), Some("sess"));
        assert_eq!(id(None, Some("T-1")).as_deref(), Some("T-1"));
        assert_eq!(id(Some(""), Some("T-1")).as_deref(), Some("T-1"));
        assert_eq!(id(Some(""), Some("")), None);
        assert_eq!(id(None, None), None);
    }

    fn binding(id: &str) -> RepoBinding {
        RepoBinding {
            repo_id: id.into(),
            git_url: None,
            remote_id: None,
            codebase_name: None,
            updated_at: "t".into(),
        }
    }

    /// The single parse-and-compare both `commands::stream::attribution_mode`
    /// and `commands::project::format_force_line` now go through. Each arm is
    /// asserted here once so a change to the shared rule shows up in one
    /// place, not only through its two callers.
    #[test]
    fn force_status_classifies_live_lapsed_and_unreadable() {
        let future = chrono::Utc::now() + chrono::Duration::hours(4);
        assert_eq!(
            force_status(&future.to_rfc3339()),
            ForceStatus::Live(future)
        );
        let past = chrono::Utc::now() - chrono::Duration::hours(1);
        assert_eq!(force_status(&past.to_rfc3339()), ForceStatus::Lapsed(past));
        // Fail-safe: unreadable is NOT in force, and never a panic.
        assert_eq!(force_status("not-a-timestamp"), ForceStatus::Unreadable);
        assert_eq!(force_status(""), ForceStatus::Unreadable);
        // A non-UTC offset is normalised, not rejected.
        assert_eq!(
            force_status("2020-01-01T00:00:00+02:00"),
            ForceStatus::Lapsed(
                chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00+02:00")
                    .unwrap()
                    .with_timezone(&chrono::Utc)
            )
        );
    }

    #[test]
    fn save_in_then_load_in_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let s = SessionState {
            active: Some(binding("r1")),
            subagents: HashMap::from([("/wt/a".to_string(), binding("r2"))]),
            ..Default::default()
        };
        save_in(tmp.path(), "sess-1", &s).unwrap();
        assert_eq!(load_in(tmp.path(), "sess-1"), s);
    }

    #[test]
    fn load_in_missing_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            load_in(tmp.path(), "no-such-session"),
            SessionState::default()
        );
    }

    #[test]
    fn rejects_unsafe_session_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let s = SessionState::default();
        // Path-traversal / separator ids must be refused, never written to disk.
        assert!(save_in(tmp.path(), "../evil", &s).is_err());
        assert!(save_in(tmp.path(), "a/b", &s).is_err());
        assert!(save_in(tmp.path(), "", &s).is_err());
        // load of an unsafe id never touches disk; returns the default.
        assert_eq!(load_in(tmp.path(), "../evil"), SessionState::default());
        // A UUID-style id is accepted.
        assert!(save_in(tmp.path(), "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b", &s).is_ok());
    }

    #[test]
    fn load_from_reads_active_binding() {
        let dir = tempfile::tempdir().unwrap();
        save_in(
            dir.path(),
            "sess-1",
            &SessionState {
                active: Some(binding("r1")),
                ..Default::default()
            },
        )
        .unwrap();
        let st = load_from(dir.path(), "sess-1");
        assert_eq!(st.active.unwrap().repo_id, "r1");
    }

    #[test]
    fn load_from_missing_is_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_from(dir.path(), "nope"), SessionState::default());
    }

    #[test]
    fn save_in_is_atomic_and_leaves_no_tmp() {
        let tmp = tempfile::tempdir().unwrap();
        let s = SessionState {
            active: Some(binding("r1")),
            subagents: HashMap::new(),
            ..Default::default()
        };
        save_in(tmp.path(), "sess-atomic", &s).unwrap();
        assert_eq!(load_in(tmp.path(), "sess-atomic"), s);
        // no stray temp file
        let leftover = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(
            !leftover,
            "temp file should be renamed away, not left behind"
        );
    }

    #[test]
    fn record_storage_root_is_first_write_wins_and_keeps_other_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let s = SessionState {
            active: Some(binding("r1")),
            ..Default::default()
        };
        save_in(tmp.path(), "sess-1", &s).unwrap();

        assert!(
            record_storage_root(tmp.path(), "sess-1", std::path::Path::new("/umbrella")).unwrap()
        );
        assert!(!record_storage_root(
            tmp.path(),
            "sess-1",
            std::path::Path::new("/umbrella/child")
        )
        .unwrap());

        let loaded = load_in(tmp.path(), "sess-1");
        assert_eq!(loaded.storage_root.as_deref(), Some("/umbrella"));
        assert_eq!(loaded.active, s.active);
    }

    #[test]
    fn storage_root_needs_the_dir_but_not_its_tracevault() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = SessionState {
            storage_root: Some(tmp.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        assert_eq!(storage_root(&s), Some(tmp.path().to_path_buf()));
        s.storage_root = Some(tmp.path().join("gone").to_string_lossy().into_owned());
        assert_eq!(storage_root(&s), None);
        s.storage_root = None;
        assert_eq!(storage_root(&s), None);
    }

    #[test]
    fn record_worktree_lists_each_worktree_once_and_ran_in_reads_it() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        assert!(!ran_in(d, "/wt/a"));
        record_worktree(d, "/wt/a").unwrap();
        record_worktree(d, "/wt/b").unwrap();
        record_worktree(d, "/wt/a").unwrap();
        assert_eq!(
            std::fs::read_to_string(d.join(WORKTREES_FILE)).unwrap(),
            "/wt/a\n/wt/b\n"
        );
        assert!(ran_in(d, "/wt/a") && ran_in(d, "/wt/b"));
        assert!(!ran_in(d, "/wt"), "whole lines only");
        std::fs::write(d.join("origin"), "/wt/c").unwrap();
        assert!(ran_in(d, "/wt/c"));
    }

    #[test]
    fn anchored_session_dirs_finds_sessions_kept_elsewhere_that_ran_here() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let umbrella = tmp.path().join("umbrella");
        let repo = tmp.path().join("umbrella/repo");
        let local = repo.join(".tracevault/sessions");
        let anchor = |id: &str, root: &std::path::Path, ran: Option<&str>| {
            let st = SessionState {
                storage_root: Some(root.to_string_lossy().into_owned()),
                ..Default::default()
            };
            save_in(&state, id, &st).unwrap();
            let dir = root.join(".tracevault/sessions").join(id);
            std::fs::create_dir_all(&dir).unwrap();
            if let Some(wt) = ran {
                record_worktree(&dir, wt).unwrap();
            }
            dir
        };
        let worked_here = anchor("here", &umbrella, Some("/repo"));
        anchor("elsewhere", &umbrella, Some("/other"));
        anchor("local", &repo, Some("/repo"));
        save_in(&state, "unanchored", &SessionState::default()).unwrap();

        assert_eq!(
            anchored_session_dirs_in(&state, &local, "/repo"),
            vec![worked_here]
        );
        assert!(anchored_session_dirs_in(&tmp.path().join("none"), &local, "/repo").is_empty());
    }

    #[test]
    fn old_state_files_load_without_a_storage_root() {
        let st: SessionState =
            toml::from_str("[active]\nrepo_id = \"r1\"\nupdated_at = \"t\"\n").unwrap();
        assert_eq!(st.storage_root, None);
        assert!(!toml::to_string(&st).unwrap().contains("storage_root"));
    }

    #[test]
    fn session_state_carries_project_bindings() {
        let tmp = tempfile::tempdir().unwrap();
        let pb = ProjectBinding {
            project_id: "11111111-1111-1111-1111-111111111111".into(),
            project_name: "payments".into(),
            updated_at: "t".into(),
            forced_until: None,
        };
        let mut subagent_projects = HashMap::new();
        subagent_projects.insert("/wt".into(), pb.clone());
        let st = SessionState {
            active_project: Some(pb.clone()),
            subagent_projects,
            ..Default::default()
        };
        save_in(tmp.path(), "sess-1", &st).unwrap();
        let loaded = load_from(tmp.path(), "sess-1");
        assert_eq!(loaded.active_project, Some(pb.clone()));
        assert_eq!(loaded.subagent_projects.get("/wt"), Some(&pb));
    }
}
