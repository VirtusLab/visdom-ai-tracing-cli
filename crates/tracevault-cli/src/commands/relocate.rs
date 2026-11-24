//! `init` carries the files an earlier release kept under `.tracevault/` and
//! `tracevault/` over to `.visdom/` and `visdom/` (see [`crate::layout`]).
//!
//! Everything here is a rename of a file nothing newer has replaced: a file
//! that already exists in the new place wins and its legacy twin is left
//! alone. Failures are reported and never stop `init` — the legacy files keep
//! being read where they are.

use std::fs;
use std::path::Path;

use crate::layout;

/// Move `from` to `to` unless `to` exists. `Some(line)` describes what
/// happened when anything did.
fn carry(from: &Path, to: &Path) -> Option<String> {
    if !from.exists() || to.exists() {
        return None;
    }
    // A link is somebody's arrangement, and following one out of the
    // directory is not this step's business: it stays, and is still read.
    if is_symlink(from) {
        return Some(format!(
            "{} is a symbolic link; it is left where it is and still read there",
            from.display()
        ));
    }
    let moved = to
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| move_without_replacing(from, to));
    Some(match moved {
        Ok(()) => format!("Moved {} to {}", from.display(), to.display()),
        Err(e) => format!(
            "Could not move {} to {} ({e}); it is still read where it is",
            from.display(),
            to.display()
        ),
    })
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

/// Put a complete copy of the file `from` at `to`, never over something
/// that is or appears there (a `login` or a context edit running beside this
/// `init`): the copy is written and synced under a temporary name in `to`'s
/// directory — created owner-only, then given `from`'s mode — and takes the
/// name `to` only once whole, in one step that fails when the name is taken.
/// A copy that fails part-way leaves nothing behind, so the legacy file keeps
/// being read. Returns what was copied.
fn publish_copy(from: &Path, to: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Write as _;
    let content = fs::read(from)?;
    let dir = to
        .parent()
        .ok_or_else(|| std::io::Error::other("no directory to move into"))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".carried-over.")
        .tempfile_in(dir)?;
    tmp.write_all(&content)?;
    tmp.as_file()
        .set_permissions(fs::metadata(from)?.permissions())?;
    tmp.as_file().sync_all()?;
    tmp.persist_noclobber(to).map_err(|e| e.error)?;
    Ok(content)
}

/// Move `from` to `to`, never over something that appeared at `to` in the
/// meantime. A directory is renamed, which fails on a non-empty one. A file
/// is [`publish_copy`]-ed and then removed — unless it changed while it was
/// being copied (an earlier `tracevault` refreshing the login it still
/// reads): then the copy is withdrawn and the file stays, still read where
/// it is, for the next `init` to move.
fn move_without_replacing(from: &Path, to: &Path) -> std::io::Result<()> {
    if fs::symlink_metadata(from)?.is_dir() {
        return fs::rename(from, to);
    }
    let copied = publish_copy(from, to)?;
    if fs::read(from)? != copied {
        fs::remove_file(to)?;
        return Err(std::io::Error::other("it changed while being moved"));
    }
    fs::remove_file(from)
}

/// Remove `dir` when nothing is left in it but the files named in `ignorable`.
fn remove_if_spent(dir: &Path, ignorable: &[&str]) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    let names: Vec<_> = entries.flatten().map(|e| e.file_name()).collect();
    if names
        .iter()
        .any(|name| !ignorable.iter().any(|i| name.to_str() == Some(i)))
    {
        return false;
    }
    for name in names {
        let _ = fs::remove_file(dir.join(name));
    }
    fs::remove_dir(dir).is_ok()
}

/// Carry `root`'s `.tracevault/` over to `.visdom/`: the tracing config (as
/// `tracing.toml`), the context files, the cache and every session that has
/// been pushed. A session not pushed yet may still be running, and its hooks
/// keep its directory where it is, so it stays — and is still read there.
pub fn carry_over_repo(root: &Path) -> Vec<String> {
    let legacy = layout::legacy_repo_dir(root);
    if !legacy.is_dir() {
        return vec![];
    }
    let new = layout::repo_dir(root);
    let mut lines = Vec::new();
    lines.extend(carry(
        &layout::legacy_tracing_config(root),
        &layout::tracing_config(root),
    ));
    for name in ["context.json", "worktrees", "cache"] {
        lines.extend(carry(&legacy.join(name), &new.join(name)));
    }

    let legacy_sessions = layout::legacy_sessions_dir(root);
    let (mut moved, mut left) = (0usize, 0usize);
    for entry in fs::read_dir(&legacy_sessions)
        .into_iter()
        .flatten()
        .flatten()
    {
        let from = entry.path();
        let to = layout::sessions_dir(root).join(entry.file_name());
        if is_symlink(&from) || !from.is_dir() || !from.join(".pushed").exists() || to.exists() {
            left += 1;
            continue;
        }
        match fs::create_dir_all(layout::sessions_dir(root)).and_then(|()| fs::rename(&from, &to)) {
            Ok(()) => moved += 1,
            Err(_) => left += 1,
        }
    }
    if moved > 0 {
        lines.push(format!(
            "Moved {moved} pushed session(s) from {} to {}",
            legacy_sessions.display(),
            layout::sessions_dir(root).display()
        ));
    }
    if left > 0 {
        lines.push(format!(
            "{left} session(s) not pushed yet stay in {} and are still read there",
            legacy_sessions.display()
        ));
    }

    if new.is_dir() {
        let _ = crate::config::TracevaultConfig::ensure_gitignore(&new);
    }
    remove_if_spent(&legacy_sessions, &[]);
    if remove_if_spent(&legacy, &[".gitignore"]) {
        lines.push(format!("Removed the empty {}", legacy.display()));
    }
    lines
}

/// Carry the user-level `tracevault/` directory beside `user_dir` (the
/// `visdom/` config directory) over to it: the login, the tracing config (as
/// `tracing.toml`), the default repo and project and the install state, and
/// a copy of the user context.
pub fn carry_over_user_in(user_dir: &Path) -> Vec<String> {
    let Some(legacy) = layout::legacy_sibling(user_dir).filter(|dir| dir.is_dir()) else {
        return vec![];
    };
    let mut lines = Vec::new();
    lines.extend(carry(
        &legacy.join(layout::LEGACY_TRACING_CONFIG),
        &user_dir.join(layout::TRACING_CONFIG),
    ));
    for name in [
        "credentials.json",
        "default_repo.toml",
        "user_project.toml",
        "install-state.json",
    ] {
        lines.extend(carry(&legacy.join(name), &user_dir.join(name)));
    }
    // The user context is copied, not moved: a config — the user's, or any
    // repository's, which cannot be found from here — may name the old file
    // by its path, and must keep finding it.
    let (from, to) = (legacy.join("context.json"), user_dir.join("context.json"));
    if from.exists() && !to.exists() {
        lines.push(
            match fs::create_dir_all(user_dir).and_then(|()| publish_copy(&from, &to)) {
                Ok(_) => format!(
                    "Copied {} to {} (the old file is left for configs that name it by path)",
                    from.display(),
                    to.display()
                ),
                Err(e) => format!(
                    "Could not copy {} to {} ({e}); it is still read where it is",
                    from.display(),
                    to.display()
                ),
            },
        );
    }
    if remove_if_spent(&legacy, &["install-state.lock"]) {
        lines.push(format!("Removed the empty {}", legacy.display()));
    }
    lines
}

/// [`carry_over_user_in`] the user's own config directory.
pub fn carry_over_user() -> Vec<String> {
    carry_over_user_in(&crate::config::tv_config_root())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn a_repository_is_carried_over_and_its_empty_legacy_directory_removed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".tracevault/config.toml"), "repo_id = \"r\"\n");
        write(&root.join(".tracevault/context.json"), "{}");
        write(&root.join(".tracevault/.gitignore"), "sessions/\n");
        write(&root.join(".tracevault/sessions/done/.pushed"), "");
        write(&root.join(".tracevault/sessions/done/events.jsonl"), "e\n");

        let lines = carry_over_repo(root);

        assert_eq!(
            fs::read_to_string(root.join(".visdom/tracing.toml")).unwrap(),
            "repo_id = \"r\"\n"
        );
        assert!(root.join(".visdom/context.json").exists());
        assert!(root.join(".visdom/sessions/done/events.jsonl").exists());
        assert!(root.join(".visdom/.gitignore").exists());
        assert!(!root.join(".tracevault").exists(), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("tracing.toml")),
            "{lines:?}"
        );
        // Nothing left to do the second time.
        assert!(carry_over_repo(root).is_empty());
    }

    #[test]
    fn a_session_not_pushed_yet_stays_where_its_hooks_keep_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".tracevault/sessions/live/events.jsonl"), "e\n");

        let lines = carry_over_repo(root);

        assert!(root.join(".tracevault/sessions/live/events.jsonl").exists());
        assert!(!root.join(".visdom/sessions/live").exists());
        assert!(
            lines.iter().any(|l| l.contains("not pushed yet")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_file_already_in_the_new_place_wins() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".tracevault/config.toml"), "repo_id = \"old\"\n");
        write(&root.join(".visdom/tracing.toml"), "repo_id = \"new\"\n");

        carry_over_repo(root);

        assert_eq!(
            fs::read_to_string(root.join(".visdom/tracing.toml")).unwrap(),
            "repo_id = \"new\"\n"
        );
        assert!(root.join(".tracevault/config.toml").exists());
    }

    #[test]
    fn the_user_directory_is_carried_over() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("tracevault");
        let user = dir.path().join("visdom");
        write(&legacy.join("credentials.json"), "{}");
        write(&legacy.join("config.toml"), "user_context = true\n");
        write(&legacy.join("context.json"), "{}");
        write(&legacy.join("default_repo.toml"), "");
        // The launcher's own config is already there and is not touched.
        write(&user.join("config.toml"), "agent = \"codex\"\n");

        let lines = carry_over_user_in(&user);

        assert!(user.join("credentials.json").exists(), "{lines:?}");
        assert_eq!(
            fs::read_to_string(user.join("tracing.toml")).unwrap(),
            "user_context = true\n"
        );
        assert_eq!(
            fs::read_to_string(user.join("config.toml")).unwrap(),
            "agent = \"codex\"\n"
        );
        // The user context is copied: a repository's config may name the
        // old file by path, so it stays, and with it the directory.
        assert!(user.join("context.json").exists());
        assert!(legacy.join("context.json").exists());
        assert!(!legacy.join("credentials.json").exists());
        assert!(lines.iter().any(|l| l.starts_with("Copied ")), "{lines:?}");
        // Nothing left to do the second time.
        assert!(carry_over_user_in(&user).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_login_keeps_its_mode_and_a_link_is_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("tracevault");
        let user = dir.path().join("visdom");
        write(&legacy.join("credentials.json"), "{\"secret\":1}");
        fs::set_permissions(
            legacy.join("credentials.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        // A default repo that is a link to a file somewhere else.
        write(&dir.path().join("elsewhere.toml"), "");
        std::os::unix::fs::symlink(
            dir.path().join("elsewhere.toml"),
            legacy.join("default_repo.toml"),
        )
        .unwrap();

        let lines = carry_over_user_in(&user);

        let moved = user.join("credentials.json");
        assert_eq!(fs::read_to_string(&moved).unwrap(), "{\"secret\":1}");
        assert_eq!(
            fs::metadata(&moved).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!legacy.join("credentials.json").exists());
        assert!(legacy.join("default_repo.toml").exists());
        assert!(!user.join("default_repo.toml").exists());
        assert!(dir.path().join("elsewhere.toml").exists());
        assert!(
            lines.iter().any(|l| l.contains("symbolic link")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_move_never_replaces_what_is_already_there() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("old");
        let to = dir.path().join("new");
        fs::write(&from, "old").unwrap();
        fs::write(&to, "new").unwrap();

        assert!(move_without_replacing(&from, &to).is_err());

        assert_eq!(fs::read_to_string(&to).unwrap(), "new");
        assert_eq!(fs::read_to_string(&from).unwrap(), "old");
    }

    /// Everything in `dir`, to tell that a failed step left nothing behind.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_copy_that_cannot_be_published_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("old");
        fs::write(&from, "old").unwrap();
        let into = dir.path().join("into");
        fs::create_dir_all(&into).unwrap();
        // The name is taken: by a file somebody wrote meanwhile.
        fs::write(into.join("new"), "theirs").unwrap();

        assert!(publish_copy(&from, &into.join("new")).is_err());

        assert_eq!(fs::read_to_string(into.join("new")).unwrap(), "theirs");
        assert_eq!(names(&into), ["new"], "no temporary file may stay");
        // A source that cannot be read publishes nothing either.
        assert!(publish_copy(&dir.path().join("missing"), &into.join("other")).is_err());
        assert_eq!(names(&into), ["new"]);
    }

    #[test]
    fn a_user_context_written_meanwhile_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("tracevault");
        let user = dir.path().join("visdom");
        write(&legacy.join("context.json"), "old");
        write(&user.join("context.json"), "theirs");

        carry_over_user_in(&user);

        assert_eq!(
            fs::read_to_string(user.join("context.json")).unwrap(),
            "theirs"
        );
        assert_eq!(names(&user), ["context.json"]);
    }
}
