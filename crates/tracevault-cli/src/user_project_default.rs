//! User-level default project binding: a session-independent binding any Claude
//! Code session inherits when nothing more specific resolves. Set by
//! `tracevault project switch --user` (or a no-session `project switch`); read by
//! `project status` as the lowest-precedence tier. (The capture path — `stream`
//! hooks — does not yet consume this; wiring the resolved project into ingest is
//! future work, tracked separately from this binding store.) Lets a container
//! bind its project *before* Claude launches — no session id required.

use std::path::{Path, PathBuf};

use crate::session_state::ProjectBinding;

/// `dirs::config_dir()/tracevault/user_project.toml` — beside `credentials.json`.
pub fn default_project_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("tracevault").join("user_project.toml"))
}

fn load_from(dir: &Path) -> Option<ProjectBinding> {
    let path = dir.join("user_project.toml");
    let s = std::fs::read_to_string(path).ok()?;
    toml::from_str(&s).ok()
}

fn save_in(dir: &Path, binding: &ProjectBinding) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dir)?;
    // Atomic write: temp file in the same dir, then rename over the target so a
    // crash or concurrent reader never sees a partial file (mirrors session_state).
    let path = dir.join("user_project.toml");
    let tmp = dir.join(format!("user_project.toml.{}.tmp", std::process::id()));
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
pub fn load() -> Option<ProjectBinding> {
    let path = default_project_path()?;
    let dir = path.parent()?;
    load_from(dir)
}

/// Persist the user-level default binding, creating the config dir if needed.
pub fn save(binding: &ProjectBinding) -> Result<(), Box<dyn std::error::Error>> {
    let path = default_project_path().ok_or("cannot determine user config dir")?;
    let dir = path.parent().ok_or("cannot determine parent directory")?;
    save_in(dir, binding)
}

/// Remove the user-level default binding (ok if already absent).
#[allow(dead_code)]
pub fn clear() -> Result<(), Box<dyn std::error::Error>> {
    let path = default_project_path().ok_or("cannot determine user config dir")?;
    clear_at(&path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_project_default_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let pb = ProjectBinding {
            project_id: "id".into(),
            project_name: "p".into(),
            updated_at: "t".into(),
        };
        save_in(tmp.path(), &pb).unwrap();
        assert_eq!(load_from(tmp.path()), Some(pb));
    }

    #[test]
    fn user_project_default_absent_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(load_from(tmp.path()), None);
    }

    #[test]
    fn save_then_load_roundtrips_via_real_paths() {
        // Exercises the REAL `save()`/`load()` entry points (not `save_in`/
        // `load_from` directly) so a regression like `load()` looking for
        // `.../user_project.toml/user_project.toml` (passing the file path
        // instead of its parent dir into `load_from`) would be caught.

        // `commands::project`'s `switch_without_session_or_user_flag_skips_
        // codebase_check` test also redirects XDG_CONFIG_HOME (to keep its
        // credential resolution off the developer's real config); both hold
        // this process-wide lock for their duration so they can't interleave.
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let tmp = tempfile::tempdir().unwrap();

        // SAFETY: test-scoped env mutation, restored in a guard so a panic
        // in `save`/`load` still cleans up the process env.
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", tmp.path());

        let pb = ProjectBinding {
            project_id: "id".into(),
            project_name: "p".into(),
            updated_at: "t".into(),
        };
        save(&pb).unwrap();
        assert_eq!(load(), Some(pb));
    }
}
