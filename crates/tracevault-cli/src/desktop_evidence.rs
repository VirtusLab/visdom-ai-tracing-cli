//! Evidence that a Claude Desktop session was traced.
//!
//! Configuration checks say what should happen; only an event the server
//! accepted says what did. `visdom` notes when it opened the desktop app, and
//! the event send path notes the first accepted event of each desktop
//! session. `visdom doctor` reads both. The file is local
//! (`<config dir>/visdom/desktop-evidence.json`); nothing of it is sent.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE: &str = "desktop-evidence.json";

/// What Claude Desktop sets `CLAUDE_CODE_ENTRYPOINT` to in the Claude Code it
/// starts for a Code-tab session (seen with app 6.28899, Claude Code 2.1.330).
pub const DESKTOP_ENTRYPOINT: &str = "claude-desktop";

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// When `visdom` last opened the app (Unix seconds), and on which folder.
    pub opened_at: Option<i64>,
    pub opened_dir: Option<PathBuf>,
    /// When the server first accepted an event of the latest desktop session.
    pub traced_at: Option<i64>,
    pub traced_session: Option<String>,
}

pub fn path_in(config_root: &Path) -> PathBuf {
    crate::layout::user_dir_in(config_root).join(FILE)
}

/// The evidence so far: nothing when there is no file, an error naming the
/// file when it cannot be read as one.
pub fn load(config_root: &Path) -> Result<Evidence, String> {
    let path = path_in(config_root);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| format!("{} cannot be read: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Evidence::default()),
        Err(e) => Err(format!("{} cannot be read: {e}", path.display())),
    }
}

/// Written whole beside the file and moved over it, so a reader never sees
/// half of one.
fn save(config_root: &Path, evidence: &Evidence) -> std::io::Result<()> {
    let path = path_in(config_root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(
        &tmp,
        serde_json::to_vec(evidence).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// `visdom` opened the app on `dir`. A file that cannot be read is replaced.
pub fn record_opened(config_root: &Path, dir: &Path, now: i64) -> std::io::Result<()> {
    let mut evidence = load(config_root).unwrap_or_default();
    evidence.opened_at = Some(now);
    evidence.opened_dir = Some(dir.to_path_buf());
    save(config_root, &evidence)
}

/// The server accepted an event of desktop session `session_id`. Written once
/// per session: later events of the same one change nothing.
pub fn record_traced(config_root: &Path, session_id: &str, now: i64) -> std::io::Result<()> {
    let mut evidence = match load(config_root) {
        Ok(evidence) if evidence.traced_session.as_deref() == Some(session_id) => return Ok(()),
        Ok(evidence) => evidence,
        Err(_) => Evidence::default(),
    };
    evidence.traced_at = Some(now);
    evidence.traced_session = Some(session_id.to_string());
    save(config_root, &evidence)
}

/// Whether `CLAUDE_CODE_ENTRYPOINT` is that of a Claude Desktop session.
pub fn is_desktop_session(entrypoint: Option<&str>) -> bool {
    entrypoint == Some(DESKTOP_ENTRYPOINT)
}

/// Called where the server has accepted an event: note it when this process
/// is a hook of a desktop session. Never fails and never waits.
pub fn note_accepted_event(session_id: &str) {
    if !is_desktop_session(std::env::var("CLAUDE_CODE_ENTRYPOINT").ok().as_deref()) {
        return;
    }
    let Some(config_root) = dirs::config_dir() else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let _ = record_traced(&config_root, session_id, now);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_then_tracing_round_trips() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(load(root.path()), Ok(Evidence::default()));
        record_opened(root.path(), Path::new("/repo"), 10).unwrap();
        record_traced(root.path(), "s1", 20).unwrap();
        assert_eq!(
            load(root.path()),
            Ok(Evidence {
                opened_at: Some(10),
                opened_dir: Some(PathBuf::from("/repo")),
                traced_at: Some(20),
                traced_session: Some("s1".into()),
            })
        );
        // Opening again keeps what was traced.
        record_opened(root.path(), Path::new("/other"), 30).unwrap();
        let evidence = load(root.path()).unwrap();
        assert_eq!(
            (evidence.opened_at, evidence.traced_at),
            (Some(30), Some(20))
        );
    }

    #[test]
    fn the_same_session_is_written_once() {
        let root = tempfile::tempdir().unwrap();
        record_traced(root.path(), "s1", 10).unwrap();
        let written = std::fs::metadata(path_in(root.path()))
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        record_traced(root.path(), "s1", 99).unwrap();
        assert_eq!(load(root.path()).unwrap().traced_at, Some(10));
        assert_eq!(
            std::fs::metadata(path_in(root.path()))
                .unwrap()
                .modified()
                .unwrap(),
            written
        );
        // Another session is news.
        record_traced(root.path(), "s2", 99).unwrap();
        assert_eq!(load(root.path()).unwrap().traced_at, Some(99));
    }

    #[test]
    fn a_corrupt_evidence_file_is_an_error_until_it_is_written_again() {
        let root = tempfile::tempdir().unwrap();
        let path = path_in(root.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        let error = load(root.path()).unwrap_err();
        assert!(error.contains(&path.display().to_string()), "{error}");
        record_traced(root.path(), "s1", 5).unwrap();
        assert_eq!(
            load(root.path()).unwrap().traced_session.as_deref(),
            Some("s1")
        );
    }

    #[test]
    fn only_the_code_tab_counts_as_desktop() {
        assert!(is_desktop_session(Some(DESKTOP_ENTRYPOINT)));
        assert!(!is_desktop_session(Some("cli")));
        assert!(!is_desktop_session(None));
    }
}
