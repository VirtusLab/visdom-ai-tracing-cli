//! User-level install state (VIS-1109): what this machine's TraceVault setup
//! was last brought to, so a newer CLI knows to run its upgrade step once
//! (`commands::upgrade`), and the user's Claude Code telemetry opt-out, so
//! nothing ever turns telemetry back on after `--no-claude-telemetry`.
//!
//! Kept in `~/.config/tracevault/install-state.json`, beside `config.toml`:
//! `config.toml` is rewritten from its struct by `init --global`, and a repo's
//! `.tracevault/` does not exist in workspace mode.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallState {
    /// The CLI version that last ran the machine-wide step (telemetry, global
    /// hooks). `None`: an install older than this field, or none at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_version: Option<String>,
    /// The user turned Claude Code telemetry off (`--no-claude-telemetry`,
    /// `tracevault telemetry off`, or no at the `init` prompt).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub claude_telemetry_opt_out: bool,
    /// The version whose upgrade step turned telemetry on without the user
    /// asking, for `status` to say so. Cleared by an explicit on/off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_auto_enabled: Option<String>,
    /// Per repository (primary checkout path): the version that last re-merged
    /// its hooks, and the settings files they were found in.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub repos: BTreeMap<String, RepoState>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoState {
    pub version: String,
    /// Settings files (absolute paths) holding TraceVault's Claude Code hooks.
    #[serde(default)]
    pub hook_files: Vec<PathBuf>,
}

pub fn state_path_in(config_root: &Path) -> PathBuf {
    config_root.join("install-state.json")
}

/// The state under `config_root`; a missing or unreadable file is an empty
/// state (every step then runs once, which is safe: each one is idempotent).
pub fn load_in(config_root: &Path) -> InstallState {
    std::fs::read_to_string(state_path_in(config_root))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Write `state` atomically (temp file + rename), creating `config_root`.
pub fn save_in(config_root: &Path, state: &InstallState) -> std::io::Result<()> {
    std::fs::create_dir_all(config_root)?;
    let path = state_path_in(config_root);
    let tmp = path.with_file_name(format!(
        "install-state.json.{}.{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or_default()
    ));
    let body = serde_json::to_string_pretty(state).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, body)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
}

/// Load, apply `change`, save.
pub fn update_in(
    config_root: &Path,
    change: impl FnOnce(&mut InstallState),
) -> std::io::Result<()> {
    let mut state = load_in(config_root);
    change(&mut state);
    save_in(config_root, &state)
}

/// This binary's version.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `major.minor.patch` of a version string; a pre-release or build suffix is
/// ignored. `None` when it does not parse.
fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.trim().split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let v = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(v)
}

/// Whether `current` is newer than the `recorded` version. Nothing recorded
/// (or something unparseable) counts as older, so the step runs; a recorded
/// version newer than `current` (a downgrade) does not.
pub fn is_newer(current: &str, recorded: Option<&str>) -> bool {
    match (parse(current), recorded.and_then(parse)) {
        (Some(current), Some(recorded)) => current > recorded,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(is_newer("0.30.0", None));
        assert!(is_newer("0.30.0", Some("0.29.1")));
        assert!(is_newer("1.0.0", Some("0.99.99")));
        assert!(is_newer("0.30.0", Some("garbage")));
        assert!(!is_newer("0.30.0", Some("0.30.0")));
        assert!(!is_newer("0.29.1", Some("0.30.0")));
        assert!(!is_newer("0.30.0-rc.1", Some("0.30.0")));
        assert!(is_newer("0.10.0", Some("0.9.9")), "numeric, not lexical");
    }

    #[test]
    fn missing_or_malformed_state_is_empty_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_in(dir.path()), InstallState::default());
        std::fs::write(state_path_in(dir.path()), "{not json").unwrap();
        assert_eq!(load_in(dir.path()), InstallState::default());

        let mut state = InstallState {
            configured_version: Some("0.30.0".into()),
            claude_telemetry_opt_out: true,
            ..Default::default()
        };
        state.repos.insert(
            "/r".into(),
            RepoState {
                version: "0.30.0".into(),
                hook_files: vec!["/r/.claude/settings.local.json".into()],
            },
        );
        save_in(&dir.path().join("nested"), &state).unwrap();
        assert_eq!(load_in(&dir.path().join("nested")), state);
    }

    #[test]
    fn empty_state_serializes_to_an_empty_object() {
        assert_eq!(
            serde_json::to_string(&InstallState::default()).unwrap(),
            "{}"
        );
    }
}
