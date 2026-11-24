//! `init` carries the files an earlier release kept under `.tracevault/` and
//! `tracevault/` over to `.visdom/` and `visdom/` (see [`crate::layout`]).
//!
//! Everything here is a rename of a file nothing newer has replaced: a file
//! that already exists in the new place wins and its legacy twin is left
//! alone. Failures are reported and never stop `init` — the legacy files keep
//! being read where they are.

use std::fs;
use std::path::{Path, PathBuf};

use crate::layout;

/// Move `from` to `to` unless `to` exists. `Some(line)` describes what
/// happened when anything did.
fn carry(from: &Path, to: &Path) -> Option<String> {
    if !from.exists() || to.exists() {
        return None;
    }
    let moved = to
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| fs::rename(from, to));
    Some(match moved {
        Ok(()) => format!("Moved {} to {}", from.display(), to.display()),
        Err(e) => format!(
            "Could not move {} to {} ({e}); it is still read where it is",
            from.display(),
            to.display()
        ),
    })
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
        if !from.is_dir() || !from.join(".pushed").exists() || to.exists() {
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
/// `tracing.toml`), the user context, the default repo and project and the
/// install state.
pub fn carry_over_user_in(user_dir: &Path) -> Vec<String> {
    let Some(legacy) = layout::legacy_sibling(user_dir).filter(|dir| dir.is_dir()) else {
        return vec![];
    };
    let mut lines = Vec::new();
    // A config that names its context file by path keeps naming it: the file
    // is left where that path says.
    let named_context: Option<PathBuf> = crate::config::try_load_user_config_in(user_dir)
        .ok()
        .flatten()
        .and_then(|config| config.user_context)
        .and_then(|context| match context {
            crate::config::UserContext::Path(path) => Some(PathBuf::from(path)),
            crate::config::UserContext::Full { path, .. } => path.map(PathBuf::from),
            crate::config::UserContext::Toggle(_) => None,
        });
    lines.extend(carry(
        &legacy.join(layout::LEGACY_TRACING_CONFIG),
        &user_dir.join(layout::TRACING_CONFIG),
    ));
    for name in [
        "credentials.json",
        "context.json",
        "default_repo.toml",
        "user_project.toml",
        "install-state.json",
    ] {
        let from = legacy.join(name);
        if name == "context.json" && named_context.as_deref() == Some(from.as_path()) {
            continue;
        }
        lines.extend(carry(&from, &user_dir.join(name)));
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
        assert!(user.join("context.json").exists());
        assert!(!legacy.exists(), "{lines:?}");
    }

    #[test]
    fn a_context_file_the_config_names_by_path_is_left_there() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("tracevault");
        let user = dir.path().join("visdom");
        let context = legacy.join("context.json");
        write(&context, "{}");
        write(
            &legacy.join("config.toml"),
            &format!("user_context = {:?}\n", context.display().to_string()),
        );

        carry_over_user_in(&user);

        assert!(context.exists());
        assert!(!user.join("context.json").exists());
        assert!(user.join("tracing.toml").exists());
    }
}
