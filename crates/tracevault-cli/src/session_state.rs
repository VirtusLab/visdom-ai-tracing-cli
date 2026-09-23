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
}

/// `$XDG_STATE_HOME/tracevault/sessions` or `~/.local/state/tracevault/sessions`.
pub fn sessions_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))?;
    Some(base.join("tracevault").join("sessions"))
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
    let tmp_path = sessions_dir.join(format!("{session_id}.toml.tmp"));
    std::fs::write(&tmp_path, toml::to_string(state)?)?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

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
