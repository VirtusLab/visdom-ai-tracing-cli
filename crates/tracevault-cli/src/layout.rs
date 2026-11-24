//! Where this CLI keeps its files, and where releases before the `visdom`
//! naming kept them.
//!
//! Everything new is written under `.visdom/` (in a repository or workspace
//! folder) and `visdom/` (in the user's config and state directories). The
//! older `.tracevault/` and `tracevault/` locations are still **read**: each
//! file is looked for in the new place first, then in the old one, so an
//! installation made by an earlier release keeps working without a migration
//! step. Nothing is moved on its own — an earlier `tracevault` still on `PATH`
//! keeps finding its files — except by `init`, which carries the old files
//! over (see `commands::relocate`).

use std::path::{Path, PathBuf};

/// The directory in a repository (or workspace folder).
pub const REPO_DIR: &str = ".visdom";
/// What [`REPO_DIR`] was called before.
pub const LEGACY_REPO_DIR: &str = ".tracevault";
/// The directory under the user's config and state directories.
pub const USER_DIR: &str = "visdom";
/// What [`USER_DIR`] was called before.
pub const LEGACY_USER_DIR: &str = "tracevault";
/// The tracing config's file name. `config.toml` in the same directories is
/// the `visdom` launcher's own (agent, tracing mode, gateway).
pub const TRACING_CONFIG: &str = "tracing.toml";
/// The tracing config's file name in the legacy directories.
pub const LEGACY_TRACING_CONFIG: &str = "config.toml";

/// `<root>/.visdom`.
pub fn repo_dir(root: &Path) -> PathBuf {
    root.join(REPO_DIR)
}

/// `<root>/.tracevault`.
pub fn legacy_repo_dir(root: &Path) -> PathBuf {
    root.join(LEGACY_REPO_DIR)
}

/// Whether `root` holds this CLI's directory under either name.
pub fn has_repo_dir(root: &Path) -> bool {
    repo_dir(root).is_dir() || legacy_repo_dir(root).is_dir()
}

/// Whether `root` has been set up for tracing: a legacy `.tracevault/`, or a
/// `.visdom/` with more in it than the `config.toml` a team commits for the
/// `visdom` launcher — a checkout that only carries that file has not been.
pub fn is_set_up(root: &Path) -> bool {
    let new = repo_dir(root);
    legacy_repo_dir(root).is_dir()
        || std::fs::read_dir(new).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name() != "config.toml")
        })
}

/// `new` unless only `legacy` exists: the path to read, or to keep using for
/// something that is already there.
pub fn existing_or_new(new: PathBuf, legacy: PathBuf) -> PathBuf {
    if !new.exists() && legacy.exists() {
        legacy
    } else {
        new
    }
}

/// `rel` inside `root`'s directory: under `.visdom/` unless it exists only
/// under `.tracevault/`.
pub fn repo_path(root: &Path, rel: &str) -> PathBuf {
    existing_or_new(repo_dir(root).join(rel), legacy_repo_dir(root).join(rel))
}

/// Where `root`'s tracing config is written: `.visdom/tracing.toml`.
pub fn tracing_config(root: &Path) -> PathBuf {
    repo_dir(root).join(TRACING_CONFIG)
}

/// `root`'s tracing config as an earlier release wrote it:
/// `.tracevault/config.toml`.
pub fn legacy_tracing_config(root: &Path) -> PathBuf {
    legacy_repo_dir(root).join(LEGACY_TRACING_CONFIG)
}

/// `root`'s tracing config to read: the new file, else the legacy one.
pub fn existing_tracing_config(root: &Path) -> PathBuf {
    existing_or_new(tracing_config(root), legacy_tracing_config(root))
}

/// Where new sessions under `root` are kept: `.visdom/sessions`.
pub fn sessions_dir(root: &Path) -> PathBuf {
    repo_dir(root).join("sessions")
}

/// `.tracevault/sessions` under `root`.
pub fn legacy_sessions_dir(root: &Path) -> PathBuf {
    legacy_repo_dir(root).join("sessions")
}

/// Every sessions directory under `root` that exists, the new one first.
pub fn existing_sessions_dirs(root: &Path) -> Vec<PathBuf> {
    [sessions_dir(root), legacy_sessions_dir(root)]
        .into_iter()
        .filter(|dir| dir.is_dir())
        .collect()
}

/// The directory of session `id` under `root`: `.visdom/sessions/<id>`, or
/// the session's `.tracevault/sessions/<id>` when an earlier release started
/// it there — a session keeps one directory for its whole life.
pub fn session_dir(root: &Path, id: &str) -> PathBuf {
    existing_or_new(
        sessions_dir(root).join(id),
        legacy_sessions_dir(root).join(id),
    )
}

/// The project root a session directory (`<root>/.visdom/sessions/<id>`, or
/// the legacy spelling) lives under. `None` when the layout does not match.
pub fn session_dir_root(session_dir: &Path) -> Option<&Path> {
    let sessions = session_dir.parent()?;
    let dir = sessions.parent()?;
    let name = dir.file_name()?;
    if sessions.file_name()? != "sessions" || (name != REPO_DIR && name != LEGACY_REPO_DIR) {
        return None;
    }
    dir.parent()
}

/// The user-level directory under `base` (a config or state directory).
pub fn user_dir_in(base: &Path) -> PathBuf {
    base.join(USER_DIR)
}

/// The legacy directory beside a user-level directory: `<base>/tracevault`
/// for `<base>/visdom`. `None` for a directory with another name (an injected
/// one, as tests use), which has no legacy sibling.
pub fn legacy_sibling(user_dir: &Path) -> Option<PathBuf> {
    (user_dir.file_name()? == USER_DIR).then(|| user_dir.with_file_name(LEGACY_USER_DIR))
}

/// `name` inside the user-level directory `user_dir`, or — when it exists
/// only there — `legacy_name` inside the legacy directory beside it.
pub fn user_path_renamed(user_dir: &Path, name: &str, legacy_name: &str) -> PathBuf {
    let new = user_dir.join(name);
    match legacy_sibling(user_dir) {
        Some(legacy) => existing_or_new(new, legacy.join(legacy_name)),
        None => new,
    }
}

/// `name` inside the user-level directory `user_dir`, or inside the legacy
/// directory beside it when it exists only there.
pub fn user_path(user_dir: &Path, name: &str) -> PathBuf {
    user_path_renamed(user_dir, name, name)
}

/// The user's tracing config to read under `user_dir`: `tracing.toml`, else
/// the legacy directory's `config.toml`.
pub fn existing_user_tracing_config(user_dir: &Path) -> PathBuf {
    user_path_renamed(user_dir, TRACING_CONFIG, LEGACY_TRACING_CONFIG)
}

/// `sessions_dir` (a checkout's `.visdom/sessions`) and, when there is one,
/// the `.tracevault/sessions` beside it: every directory to scan for the
/// checkout's sessions. A directory laid out differently is scanned alone.
pub fn with_legacy_sessions(sessions_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![sessions_dir.to_path_buf()];
    if let Some(root) = session_dir_root(&sessions_dir.join("x")) {
        let legacy = legacy_sessions_dir(root);
        if legacy != sessions_dir && legacy.is_dir() {
            dirs.push(legacy);
        }
    }
    dirs
}

/// Every entry of `sessions_dir` and of the legacy directory beside it.
pub fn session_entries(sessions_dir: &Path) -> Vec<std::fs::DirEntry> {
    with_legacy_sessions(sessions_dir)
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .filter_map(|entry| entry.ok())
        .collect()
}

/// The legacy state directory matching a user-level sessions directory:
/// `<state>/tracevault/sessions` for `<state>/visdom/sessions`. `None` for a
/// directory laid out differently (an injected one).
pub fn legacy_user_sessions_dir(sessions_dir: &Path) -> Option<PathBuf> {
    if sessions_dir.file_name()? != "sessions" {
        return None;
    }
    Some(legacy_sibling(sessions_dir.parent()?)?.join("sessions"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn a_new_file_is_preferred_and_a_legacy_one_is_the_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Nothing exists: the new place.
        assert_eq!(
            repo_path(root, "context.json"),
            root.join(".visdom/context.json")
        );
        fs::create_dir_all(root.join(".tracevault")).unwrap();
        fs::write(root.join(".tracevault/context.json"), "{}").unwrap();
        assert_eq!(
            repo_path(root, "context.json"),
            root.join(".tracevault/context.json")
        );
        fs::create_dir_all(root.join(".visdom")).unwrap();
        fs::write(root.join(".visdom/context.json"), "{}").unwrap();
        assert_eq!(
            repo_path(root, "context.json"),
            root.join(".visdom/context.json")
        );
    }

    #[test]
    fn the_tracing_config_is_renamed_between_the_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(
            existing_tracing_config(root),
            root.join(".visdom/tracing.toml")
        );
        fs::create_dir_all(root.join(".tracevault")).unwrap();
        fs::write(root.join(".tracevault/config.toml"), "").unwrap();
        assert_eq!(
            existing_tracing_config(root),
            root.join(".tracevault/config.toml")
        );
        // The launcher's own `.visdom/config.toml` is not the tracing config.
        fs::create_dir_all(root.join(".visdom")).unwrap();
        fs::write(root.join(".visdom/config.toml"), "").unwrap();
        assert_eq!(
            existing_tracing_config(root),
            root.join(".tracevault/config.toml")
        );
    }

    #[test]
    fn a_session_stays_in_the_directory_it_started_in() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".tracevault/sessions/old")).unwrap();
        assert_eq!(
            session_dir(root, "old"),
            root.join(".tracevault/sessions/old")
        );
        assert_eq!(session_dir(root, "new"), root.join(".visdom/sessions/new"));
        assert_eq!(
            existing_sessions_dirs(root),
            vec![root.join(".tracevault/sessions")]
        );
        assert_eq!(
            session_dir_root(&root.join(".visdom/sessions/new")),
            Some(root)
        );
        assert_eq!(
            session_dir_root(&root.join(".tracevault/sessions/old")),
            Some(root)
        );
        assert_eq!(session_dir_root(&root.join("elsewhere/sessions/x")), None);
    }

    #[test]
    fn only_the_real_user_directory_has_a_legacy_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        fs::create_dir_all(base.join("tracevault")).unwrap();
        fs::write(base.join("tracevault/credentials.json"), "{}").unwrap();
        fs::write(base.join("tracevault/config.toml"), "").unwrap();
        let user = user_dir_in(base);
        assert_eq!(
            user_path(&user, "credentials.json"),
            base.join("tracevault/credentials.json")
        );
        assert_eq!(
            existing_user_tracing_config(&user),
            base.join("tracevault/config.toml")
        );
        assert_eq!(
            user_path(&user, "default_repo.toml"),
            base.join("visdom/default_repo.toml")
        );
        // An injected directory is used as it is.
        let injected = base.join("injected");
        assert_eq!(
            user_path(&injected, "credentials.json"),
            injected.join("credentials.json")
        );
    }
}
