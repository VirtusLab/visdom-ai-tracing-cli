//! User-level default repo binding: a session-independent binding any Claude
//! Code session inherits when nothing more specific resolves. Set by
//! `tracevault repo switch --user` (or a no-session `repo switch`); read by
//! `stream`/`repo status` as the lowest-precedence tier. Lets a container bind
//! its repo *before* Claude launches — no session id required.

use std::path::{Path, PathBuf};

use crate::session_state::RepoBinding;

/// `dirs::config_dir()/tracevault/default_repo.toml` — beside `credentials.json`.
pub fn default_repo_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("tracevault").join("default_repo.toml"))
}

fn load_from(path: &Path) -> Option<RepoBinding> {
    let s = std::fs::read_to_string(path).ok()?;
    toml::from_str(&s).ok()
}

fn save_to(path: &Path, binding: &RepoBinding) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Atomic write: temp file in the same dir, then rename over the target so a
    // crash or concurrent reader never sees a partial file (mirrors session_state).
    let tmp = path.with_file_name(format!("default_repo.toml.{}.tmp", std::process::id()));
    std::fs::write(&tmp, toml::to_string(binding)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn clear_at(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Load the user-level default binding; `None` if unset or malformed.
pub fn load() -> Option<RepoBinding> {
    load_from(&default_repo_path()?)
}

/// Persist the user-level default binding, creating the config dir if needed.
pub fn save(binding: &RepoBinding) -> Result<(), Box<dyn std::error::Error>> {
    let path = default_repo_path().ok_or("cannot determine user config dir")?;
    save_to(&path, binding)
}

/// Remove the user-level default binding (ok if already absent).
pub fn clear() -> Result<(), Box<dyn std::error::Error>> {
    let path = default_repo_path().ok_or("cannot determine user config dir")?;
    clear_at(&path)?;
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
    fn save_to_then_load_from_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("default_repo.toml");
        let b = binding("r1");
        save_to(&path, &b).unwrap();
        assert_eq!(load_from(&path), Some(b));
    }

    #[test]
    fn load_from_missing_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(load_from(&tmp.path().join("nope.toml")), None);
    }

    #[test]
    fn load_from_malformed_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("default_repo.toml");
        std::fs::write(&path, "this is not valid toml : : :").unwrap();
        assert_eq!(load_from(&path), None);
    }

    #[test]
    fn clear_at_removes_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("default_repo.toml");
        save_to(&path, &binding("r1")).unwrap();
        assert!(path.exists());
        clear_at(&path).unwrap();
        assert!(!path.exists());
        // Second clear on an absent file is still Ok.
        clear_at(&path).unwrap();
        assert_eq!(load_from(&path), None);
    }

    #[test]
    fn save_to_leaves_no_tmp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("default_repo.toml");
        save_to(&path, &binding("r1")).unwrap();
        let leftover = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!leftover, "temp file must be renamed away");
    }
}
