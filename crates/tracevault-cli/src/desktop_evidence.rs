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
/// per session and per opening of the app: later events of the same session
/// change nothing, until the app is opened again — a session continued after
/// that is traced anew.
pub fn record_traced(config_root: &Path, session_id: &str, now: i64) -> std::io::Result<()> {
    let mut evidence = match load(config_root) {
        Ok(evidence)
            if evidence.traced_session.as_deref() == Some(session_id)
                && evidence.traced_at >= evidence.opened_at =>
        {
            return Ok(())
        }
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

/// The session this process is a capture hook of, when it is one.
static HOOK_SESSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// This process is the capture hook of `session_id`: only that session's
/// events are evidence about the app this hook runs in. What the same send
/// path delivers for other sessions (an earlier one's queue, the catch-up
/// sweep, `flush`) says nothing about it.
pub fn this_hook_is_for(session_id: &str) {
    let _ = HOOK_SESSION.set(session_id.to_string());
}

/// Whether an accepted event of `session_id` is evidence, in a process whose
/// own hook session is `hook_session` and entrypoint is `entrypoint`.
fn is_evidence(session_id: &str, hook_session: Option<&str>, entrypoint: Option<&str>) -> bool {
    is_desktop_session(entrypoint) && hook_session == Some(session_id)
}

/// Called where the server has accepted an event: note it when this process
/// is the hook of that session, in the desktop app. Never fails, never waits.
pub fn note_accepted_event(session_id: &str) {
    if !is_evidence(
        session_id,
        HOOK_SESSION.get().map(String::as_str),
        std::env::var("CLAUDE_CODE_ENTRYPOINT").ok().as_deref(),
    ) {
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
    fn a_session_continued_after_the_app_is_opened_again_is_traced_anew() {
        let root = tempfile::tempdir().unwrap();
        record_traced(root.path(), "s1", 10).unwrap();
        record_opened(root.path(), Path::new("/repo"), 20).unwrap();
        record_traced(root.path(), "s1", 30).unwrap();
        assert_eq!(load(root.path()).unwrap().traced_at, Some(30));
        // And once more changes nothing again.
        record_traced(root.path(), "s1", 40).unwrap();
        assert_eq!(load(root.path()).unwrap().traced_at, Some(30));
    }

    #[test]
    fn only_the_hooks_own_session_is_evidence() {
        let desktop = Some(DESKTOP_ENTRYPOINT);
        assert!(is_evidence("s1", Some("s1"), desktop));
        // An earlier session's queue, drained by this hook.
        assert!(!is_evidence("s0", Some("s1"), desktop));
        // The catch-up sweep and `flush`: no capture hook at all.
        assert!(!is_evidence("s0", None, desktop));
        assert!(!is_evidence("s1", Some("s1"), Some("cli")));
    }

    #[test]
    fn only_the_code_tab_counts_as_desktop() {
        assert!(is_desktop_session(Some(DESKTOP_ENTRYPOINT)));
        assert!(!is_desktop_session(Some("cli")));
        assert!(!is_desktop_session(None));
    }
}
