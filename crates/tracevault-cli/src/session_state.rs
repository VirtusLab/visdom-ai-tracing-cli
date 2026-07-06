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
    pub org_slug: String,
    pub repo_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_url: Option<String>,
    pub updated_at: String,
}

/// Session-level active binding plus per-worktree subagent overrides
/// (keyed by the subagent's git worktree toplevel path — design §7).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SessionState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<RepoBinding>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub subagents: HashMap<String, RepoBinding>,
}

impl SessionState {
    /// The binding that applies for `worktree_path`: a subagent override for
    /// that worktree if one exists, otherwise the session-level `active`.
    pub fn effective(&self, worktree_path: Option<&str>) -> Option<&RepoBinding> {
        if let Some(wt) = worktree_path {
            if let Some(b) = self.subagents.get(wt) {
                return Some(b);
            }
        }
        self.active.as_ref()
    }
}

/// `$XDG_STATE_HOME/tracevault/sessions` or `~/.local/state/tracevault/sessions`.
pub fn sessions_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))?;
    Some(base.join("tracevault").join("sessions"))
}

fn state_path(session_id: &str) -> Option<PathBuf> {
    Some(sessions_dir()?.join(format!("{session_id}.toml")))
}

/// Load a session's state, returning the default (empty) state if the file is
/// absent or malformed (workspace state is best-effort, never fatal).
pub fn load(session_id: &str) -> SessionState {
    let Some(path) = state_path(session_id) else {
        return SessionState::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).unwrap_or_default(),
        Err(_) => SessionState::default(),
    }
}

/// Persist a session's state, creating the state dir if needed.
pub fn save(session_id: &str, state: &SessionState) -> Result<(), Box<dyn std::error::Error>> {
    let path = state_path(session_id).ok_or("cannot determine state dir")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, toml::to_string(state)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(id: &str) -> RepoBinding {
        RepoBinding {
            org_slug: "org".into(),
            repo_id: id.into(),
            git_url: None,
            updated_at: "t".into(),
        }
    }

    #[test]
    fn effective_prefers_subagent_override_for_worktree() {
        let mut s = SessionState::default();
        s.active = Some(binding("session-repo"));
        s.subagents.insert("/wt/a".into(), binding("subagent-repo"));
        assert_eq!(s.effective(Some("/wt/a")).unwrap().repo_id, "subagent-repo");
        assert_eq!(
            s.effective(Some("/wt/unknown")).unwrap().repo_id,
            "session-repo"
        );
        assert_eq!(s.effective(None).unwrap().repo_id, "session-repo");
    }

    #[test]
    fn save_then_load_round_trips() {
        // Redirect the state dir to a temp location for the test.
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());
        let mut s = SessionState::default();
        s.active = Some(binding("r1"));
        s.subagents.insert("/wt/a".into(), binding("r2"));
        save("sess-1", &s).unwrap();
        let loaded = load("sess-1");
        assert_eq!(loaded, s);
    }

    #[test]
    fn load_missing_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());
        assert_eq!(load("no-such-session"), SessionState::default());
    }
}
