//! The one-time step a newer CLI runs on an existing install (VIS-1109).
//!
//! Nothing runs when the CLI binary is replaced, so `session-start` calls
//! [`run`] once per Claude Code session. It compares this binary's version
//! with what [`install_state`](crate::install_state) recorded, and when the
//! binary is newer:
//!
//! - **per hook location** (the session's repo, and the global
//!   `~/.claude/settings.json`): re-merges TraceVault's hooks into each Claude
//!   Code settings file that already holds them, so hooks a newer `init` adds
//!   (e.g. `SessionEnd`, VIS-1108) reach existing installs. Repos the user
//!   never opens again keep their old hook set.
//! - **once per machine**: turns Claude Code telemetry on, unless the user
//!   opted out, another exporter is set up, or Claude Code is set to export
//!   content (prompt text etc.), and returns the one-line notice for the user.
//!
//! Everything here is best-effort: it never fails the hook. Errors go to
//! stderr, and a step that could not finish is retried by the next session.
//!
//! Per-repo work a later version needs to do once (VIS-1108's catch-up sweep)
//! goes into [`upgrade_hook_location`], after the hooks are re-merged.

use crate::commands::claude_telemetry::{self, Environment, InstallOutcome, TelemetryState};
use crate::install_state::{self, InstallState, RepoState};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The notice shown when the upgrade step turned telemetry on.
pub const TELEMETRY_NOTICE: &str = "AI Tracing: Claude Code cost telemetry is now on for new \
     sessions (`tracevault telemetry off` to disable)";

/// Where the step reads and writes; [`Paths::current`] for real use, temp
/// dirs in tests.
pub struct Paths {
    /// `~/.config/tracevault`: the install state lives here.
    pub config_root: PathBuf,
    /// `~/.claude`: global hooks and the telemetry settings.
    pub claude_dir: PathBuf,
    /// The session's repository (primary checkout), if it runs in one.
    pub project_root: Option<PathBuf>,
    /// The linked worktree the session runs in, when it is not the primary
    /// checkout: Claude Code reads that worktree's `.claude/` settings.
    pub worktree_root: Option<PathBuf>,
}

impl Paths {
    /// This user's, for a session whose working directory is `cwd`. `None`
    /// without a home directory.
    pub fn current(cwd: &Path) -> Option<Self> {
        let project = crate::paths::resolve_project_root(cwd);
        let in_git = project.source == crate::paths::ProjectRootSource::Git;
        let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let worktree = PathBuf::from(crate::paths::worktree_toplevel(cwd));
        let worktree_root =
            (in_git && canonical(&worktree) != canonical(&project.root)).then_some(worktree);
        Some(Self {
            config_root: crate::config::tv_config_root(),
            claude_dir: dirs::home_dir()?.join(".claude"),
            project_root: Some(project.root),
            worktree_root,
        })
    }
}

/// Run the upgrade step for `version` (this binary's, in production). The
/// server telemetry is exported to comes from `server_url`, called only when
/// telemetry is about to be turned on. Returns the notice for the user, if
/// there is one. Never panics on I/O; never fails.
pub fn run(
    paths: &Paths,
    version: &str,
    env: &Environment,
    server_url: impl FnOnce() -> Result<String, String>,
) -> Option<String> {
    let Some(_lock) = install_state::Lock::try_take(&paths.config_root) else {
        // Another session is upgrading right now; this one leaves it be.
        return None;
    };
    let mut state = install_state::load_in(&paths.config_root);
    let before = state.clone();

    let mut claude_hooks_found = false;
    for location in hook_locations(paths) {
        claude_hooks_found |= upgrade_hook_location(&mut state, &location, version);
    }

    let notice = if claude_hooks_found
        && install_state::is_newer(version, state.configured_version.as_deref())
    {
        upgrade_machine(&mut state, paths, version, env, server_url)
    } else {
        None
    };

    if state != before {
        if let Err(e) = install_state::save_in(&paths.config_root, &state) {
            eprintln!("tracevault: cannot record the upgrade step: {e}");
        }
    }
    notice
}

/// A place TraceVault's Claude Code hooks may be installed: a repo
/// (`.claude/settings.json` / `settings.local.json`) or the user's global
/// settings. `key` names it in the install state.
struct HookLocation {
    key: String,
    candidates: Vec<PathBuf>,
}

fn hook_locations(paths: &Paths) -> Vec<HookLocation> {
    let mut out = vec![HookLocation {
        key: paths.claude_dir.display().to_string(),
        candidates: vec![paths.claude_dir.join("settings.json")],
    }];
    // The primary checkout, where `init` installs, and a linked worktree the
    // session runs in, whose own `.claude/` Claude Code reads there.
    for root in paths.project_root.iter().chain(&paths.worktree_root) {
        // A session in the home directory itself would name ~/.claude twice.
        let key = root.display().to_string();
        if root.join(".claude") != paths.claude_dir && out.iter().all(|l| l.key != key) {
            out.push(HookLocation {
                key,
                candidates: ["settings.json", "settings.local.json"]
                    .iter()
                    .map(|f| root.join(".claude").join(f))
                    .collect(),
            });
        }
    }
    out
}

/// The `files` that hold TraceVault hooks; `Err` when one of them cannot be
/// read or parsed, so the location is not recorded as having none.
fn files_with_hooks<'a>(files: impl IntoIterator<Item = &'a PathBuf>) -> Result<Vec<PathBuf>, ()> {
    let mut found = Vec::new();
    let mut failed = false;
    for file in files {
        match crate::commands::init::has_tracevault_hooks(file) {
            Ok(true) => found.push(file.clone()),
            Ok(false) => {}
            Err(e) => {
                eprintln!(
                    "tracevault: cannot check the hooks in {}: {e}",
                    file.display()
                );
                failed = true;
            }
        }
    }
    if failed {
        Err(())
    } else {
        Ok(found)
    }
}

/// Re-merge the hooks at `location` once per version, in every candidate
/// file that holds our commands (both can, e.g. after a checkout brought in a
/// shared `settings.json`). The files found are recorded, for the next
/// sessions of this version (whether the location is installed) and for
/// `status`. Returns whether `location` holds TraceVault's Claude Code hooks.
fn upgrade_hook_location(state: &mut InstallState, location: &HookLocation, version: &str) -> bool {
    let recorded = state.repos.get(&location.key);
    let due = install_state::is_newer(version, recorded.map(|r| r.version.as_str()));
    let was_installed = recorded.is_some_and(|r| !r.hook_files.is_empty());
    if !due {
        return was_installed;
    }
    // A file that cannot be read may hold our hooks: retry next session, and
    // count the location as installed if it was before.
    let Ok(files) = files_with_hooks(&location.candidates) else {
        return was_installed;
    };
    let mut done = true;
    for file in &files {
        if let Err(e) = crate::commands::init::refresh_claude_hooks(file) {
            eprintln!(
                "tracevault: cannot update the hooks in {}: {e}",
                file.display()
            );
            done = false;
        }
    }
    // VIS-1108: the catch-up sweep of this repo's sessions runs here.
    if done {
        state.repos.insert(
            location.key.clone(),
            RepoState {
                version: version.to_string(),
                hook_files: files.clone(),
            },
        );
    }
    !files.is_empty()
}

/// Turn Claude Code telemetry on for this machine, unless the user opted out,
/// it is already exported elsewhere, or content logging is on. Records the
/// version once settled; leaves it unrecorded (so the next session retries)
/// when the server is unknown or the settings could not be written.
fn upgrade_machine(
    state: &mut InstallState,
    paths: &Paths,
    version: &str,
    env: &Environment,
    server_url: impl FnOnce() -> Result<String, String>,
) -> Option<String> {
    let settled = |state: &mut InstallState| state.configured_version = Some(version.to_string());
    if state.claude_telemetry_opt_out {
        settled(state);
        return None;
    }
    let settings = match read_settings(&paths.claude_dir.join("settings.json")) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tracevault: Claude Code telemetry not turned on: {e}");
            return None;
        }
    };
    let was_off = match claude_telemetry::inspect(&settings, env, None) {
        TelemetryState::Foreign { .. } => {
            settled(state);
            return None;
        }
        TelemetryState::Off => true,
        TelemetryState::Ours { .. } => false,
    };
    if was_off {
        let content = claude_telemetry::content_logging_settings(&settings, env);
        if !content.is_empty() {
            eprintln!(
                "tracevault: Claude Code telemetry left off: it is set to export content ({}); \
                 run `tracevault telemetry on` to send it to TraceVault anyway",
                content.join("; ")
            );
            settled(state);
            return None;
        }
    }
    let url = match server_url() {
        Ok(url) => url,
        Err(e) => {
            eprintln!("tracevault: Claude Code telemetry not turned on: {e}");
            return None;
        }
    };
    let interval_ours = claude_telemetry::interval_added_by_install(&paths.claude_dir);
    match claude_telemetry::install(&paths.claude_dir, &url, env) {
        Ok(InstallOutcome::Configured) => {
            settled(state);
            if interval_ours.is_some() {
                state.telemetry_added_interval = interval_ours;
            }
            if was_off {
                state.telemetry_auto_enabled = Some(version.to_string());
                Some(TELEMETRY_NOTICE.to_string())
            } else {
                None
            }
        }
        Ok(InstallOutcome::Foreign(_)) => {
            settled(state);
            None
        }
        Err(e) => {
            eprintln!("tracevault: Claude Code telemetry not turned on: {e}");
            None
        }
    }
}

/// `~/.claude/settings.json` parsed; a missing file is `{}`.
fn read_settings(path: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            serde_json::from_str(&s).map_err(|e| format!("cannot parse {}: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    const SERVER: &str = "https://tv.example.com";

    struct Fixture {
        _home: tempfile::TempDir,
        paths: Paths,
    }

    /// A home with a repo whose `.claude/<hooks_file>` holds an old hook set
    /// (no SubagentStop, an extra retired event).
    fn fixture(hooks_file: &str) -> Fixture {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        fs::create_dir_all(repo.join(".claude")).unwrap();
        fs::write(
            repo.join(".claude").join(hooks_file),
            json!({"hooks": {
                "Stop": [{"matcher": "", "hooks": [
                    {"type": "command", "command": "tracevault stream --event stop"}]}],
                "PreCompact": [{"matcher": "", "hooks": [
                    {"type": "command", "command": "tracevault stream --event pre-compact"}]}],
                "PostToolUse": [{"matcher": "", "hooks": [
                    {"type": "command", "command": "./my-own-hook.sh"}]}]
            }})
            .to_string(),
        )
        .unwrap();
        let paths = Paths {
            config_root: home.path().join(".config/tracevault"),
            claude_dir: home.path().join(".claude"),
            project_root: Some(repo),
            worktree_root: None,
        };
        Fixture { _home: home, paths }
    }

    fn run_with(paths: &Paths, version: &str, env: &Environment) -> Option<String> {
        run(paths, version, env, || Ok(SERVER.to_string()))
    }

    fn json_at(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    fn user_settings(paths: &Paths) -> Value {
        json_at(&paths.claude_dir.join("settings.json"))
    }

    fn repo_settings(paths: &Paths, file: &str) -> Value {
        json_at(
            &paths
                .project_root
                .as_ref()
                .unwrap()
                .join(".claude")
                .join(file),
        )
    }

    #[test]
    fn existing_install_gets_current_hooks_and_telemetry_with_the_notice_once() {
        let f = fixture("settings.local.json");
        let notice = run_with(&f.paths, "0.30.0", &Environment::default());
        assert_eq!(notice.as_deref(), Some(TELEMETRY_NOTICE));

        let hooks = &repo_settings(&f.paths, "settings.local.json")["hooks"];
        for event in crate::commands::init::tracevault_hooks()
            .as_object()
            .unwrap()
            .keys()
        {
            assert!(hooks[event].is_array(), "{event} installed");
        }
        assert!(hooks.get("PreCompact").is_none(), "retired event removed");
        assert!(
            hooks.to_string().contains("./my-own-hook.sh"),
            "foreign hook kept"
        );
        assert!(
            !f.paths
                .project_root
                .as_ref()
                .unwrap()
                .join(".claude/settings.json")
                .exists(),
            "the other settings file is not created"
        );

        let s = user_settings(&f.paths);
        assert_eq!(
            s["otelHeadersHelper"],
            "tracevault otel-headers --server-url https://tv.example.com"
        );
        assert!(s.get("hooks").is_none(), "global hooks are not installed");

        let state = install_state::load_in(&f.paths.config_root);
        assert_eq!(state.configured_version.as_deref(), Some("0.30.0"));
        assert_eq!(state.telemetry_auto_enabled.as_deref(), Some("0.30.0"));
        let repo_key = f.paths.project_root.as_ref().unwrap().display().to_string();
        assert_eq!(
            state.repos[&repo_key].hook_files,
            vec![f
                .paths
                .project_root
                .as_ref()
                .unwrap()
                .join(".claude/settings.local.json")]
        );

        // The next session: nothing to do, no notice.
        assert_eq!(run_with(&f.paths, "0.30.0", &Environment::default()), None);
        assert!(!f.paths.config_root.join("install-state.lock").exists());
    }

    #[test]
    fn opted_out_machine_stays_off_but_hooks_are_updated() {
        let f = fixture("settings.json");
        install_state::update_in(&f.paths.config_root, |s| {
            s.claude_telemetry_opt_out = true;
            s.configured_version = Some("0.29.0".into());
        })
        .unwrap();
        assert_eq!(run_with(&f.paths, "0.30.0", &Environment::default()), None);
        assert!(!f.paths.claude_dir.join("settings.json").exists());
        assert!(repo_settings(&f.paths, "settings.json")["hooks"]["SessionStart"].is_array());
        assert_eq!(
            install_state::load_in(&f.paths.config_root)
                .configured_version
                .as_deref(),
            Some("0.30.0")
        );
    }

    #[test]
    fn foreign_exporter_is_untouched() {
        let f = fixture("settings.json");
        fs::create_dir_all(&f.paths.claude_dir).unwrap();
        let foreign = json!({"env": {"OTEL_LOGS_EXPORTER": "otlp",
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT": "https://collector.corp/v1/logs"}});
        fs::write(
            f.paths.claude_dir.join("settings.json"),
            foreign.to_string(),
        )
        .unwrap();
        assert_eq!(run_with(&f.paths, "0.30.0", &Environment::default()), None);
        assert_eq!(user_settings(&f.paths), foreign);
        let state = install_state::load_in(&f.paths.config_root);
        assert_eq!(state.configured_version.as_deref(), Some("0.30.0"));
        assert_eq!(state.telemetry_auto_enabled, None);
    }

    #[test]
    fn content_logging_keeps_telemetry_off() {
        let f = fixture("settings.json");
        let env = claude_telemetry::tests::shell("OTEL_LOG_USER_PROMPTS", "1");
        assert_eq!(run_with(&f.paths, "0.30.0", &env), None);
        assert!(!f.paths.claude_dir.join("settings.json").exists());
    }

    #[test]
    fn already_ours_is_refreshed_without_a_notice() {
        let f = fixture("settings.json");
        claude_telemetry::install(&f.paths.claude_dir, SERVER, &Environment::default()).unwrap();
        assert_eq!(run_with(&f.paths, "0.30.0", &Environment::default()), None);
        assert_eq!(
            install_state::load_in(&f.paths.config_root).telemetry_auto_enabled,
            None
        );
    }

    #[test]
    fn not_logged_in_is_retried_by_the_next_session() {
        let f = fixture("settings.json");
        let notice = run(&f.paths, "0.30.0", &Environment::default(), || {
            Err("Not logged in".into())
        });
        assert_eq!(notice, None);
        assert_eq!(
            install_state::load_in(&f.paths.config_root).configured_version,
            None
        );
        assert_eq!(
            run_with(&f.paths, "0.30.0", &Environment::default()).as_deref(),
            Some(TELEMETRY_NOTICE)
        );
    }

    #[test]
    fn without_claude_hooks_nothing_is_done() {
        // E.g. a Codex-only install, whose hooks also run `session-start`.
        let home = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_root: home.path().join(".config/tracevault"),
            claude_dir: home.path().join(".claude"),
            project_root: Some(home.path().join("repo")),
            worktree_root: None,
        };
        assert_eq!(run_with(&paths, "0.30.0", &Environment::default()), None);
        assert!(!paths.claude_dir.join("settings.json").exists());
        assert_eq!(
            install_state::load_in(&paths.config_root).configured_version,
            None
        );
    }

    #[test]
    fn global_hooks_are_refreshed_and_count_as_an_install() {
        let home = tempfile::tempdir().unwrap();
        let claude_dir = home.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(
            claude_dir.join("settings.json"),
            json!({"hooks": {"Stop": [{"matcher": "", "hooks": [
                {"type": "command", "command": "tracevault stream --event stop"}]}]}})
            .to_string(),
        )
        .unwrap();
        let paths = Paths {
            config_root: home.path().join(".config/tracevault"),
            claude_dir,
            project_root: None,
            worktree_root: None,
        };
        assert_eq!(
            run_with(&paths, "0.30.0", &Environment::default()).as_deref(),
            Some(TELEMETRY_NOTICE)
        );
        let s = user_settings(&paths);
        assert!(s["hooks"]["SessionStart"].is_array());
        assert!(s["otelHeadersHelper"].is_string());
    }

    #[test]
    fn a_downgrade_or_the_same_version_does_nothing() {
        let f = fixture("settings.json");
        install_state::update_in(&f.paths.config_root, |s| {
            s.configured_version = Some("0.31.0".into());
            let key = f.paths.project_root.as_ref().unwrap().display().to_string();
            s.repos.insert(
                key,
                RepoState {
                    version: "0.31.0".into(),
                    hook_files: vec![],
                },
            );
        })
        .unwrap();
        let before = fs::read_to_string(
            f.paths
                .project_root
                .as_ref()
                .unwrap()
                .join(".claude/settings.json"),
        )
        .unwrap();
        assert_eq!(run_with(&f.paths, "0.30.0", &Environment::default()), None);
        let after = fs::read_to_string(
            f.paths
                .project_root
                .as_ref()
                .unwrap()
                .join(".claude/settings.json"),
        )
        .unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn a_held_lock_skips_the_step_and_a_stale_one_is_taken_over() {
        let f = fixture("settings.json");
        fs::create_dir_all(&f.paths.config_root).unwrap();
        let lock = f.paths.config_root.join("install-state.lock");
        fs::write(&lock, "").unwrap();
        assert_eq!(run_with(&f.paths, "0.30.0", &Environment::default()), None);
        assert!(!f.paths.claude_dir.join("settings.json").exists());

        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        fs::File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(run_with(&f.paths, "0.30.0", &Environment::default()).is_some());
        assert!(!lock.exists());
    }

    #[test]
    fn malformed_repo_settings_do_not_fail_and_are_retried() {
        let f = fixture("settings.json");
        let file = f
            .paths
            .project_root
            .as_ref()
            .unwrap()
            .join(".claude/settings.json");
        // Holds our hook, but `hooks.Stop` is not an array: the merge refuses.
        fs::write(
            &file,
            r#"{"hooks": {"Stop": {"x": 1}, "SessionStart": [{"hooks": [{"command": "tracevault session-start"}]}]}}"#,
        )
        .unwrap();
        run_with(&f.paths, "0.30.0", &Environment::default());
        let key = f.paths.project_root.as_ref().unwrap().display().to_string();
        assert!(!install_state::load_in(&f.paths.config_root)
            .repos
            .contains_key(&key));
    }

    #[test]
    fn unparseable_settings_are_not_recorded_as_hook_free() {
        let f = fixture("settings.json");
        let file = f
            .paths
            .project_root
            .as_ref()
            .unwrap()
            .join(".claude/settings.json");
        let good = fs::read_to_string(&file).unwrap();
        fs::write(&file, "{ half-written").unwrap();
        assert_eq!(run_with(&f.paths, "0.30.0", &Environment::default()), None);
        let key = f.paths.project_root.as_ref().unwrap().display().to_string();
        assert!(!install_state::load_in(&f.paths.config_root)
            .repos
            .contains_key(&key));

        // Repaired: the next session at the same version does the work.
        fs::write(&file, good).unwrap();
        assert_eq!(
            run_with(&f.paths, "0.30.0", &Environment::default()).as_deref(),
            Some(TELEMETRY_NOTICE)
        );
        assert!(repo_settings(&f.paths, "settings.json")["hooks"]["SessionStart"].is_array());
    }

    #[test]
    fn every_settings_file_with_our_hooks_is_refreshed_not_only_the_recorded_one() {
        let f = fixture("settings.local.json");
        run_with(&f.paths, "0.30.0", &Environment::default());
        // A checkout later brings in a shared settings.json with old hooks.
        let shared = f
            .paths
            .project_root
            .as_ref()
            .unwrap()
            .join(".claude/settings.json");
        fs::write(
            &shared,
            json!({"hooks": {"Stop": [{"matcher": "", "hooks": [
                {"type": "command", "command": "tracevault stream --event stop"}]}]}})
            .to_string(),
        )
        .unwrap();
        run_with(&f.paths, "0.31.0", &Environment::default());
        assert!(json_at(&shared)["hooks"]["SessionStart"].is_array());
        let key = f.paths.project_root.as_ref().unwrap().display().to_string();
        assert_eq!(
            install_state::load_in(&f.paths.config_root).repos[&key]
                .hook_files
                .len(),
            2
        );
    }

    #[test]
    fn an_opt_out_holding_the_lock_wins_over_a_concurrent_upgrade() {
        // `telemetry off` holds the lock while it removes our settings and
        // records the opt-out; an upgrade starting meanwhile does nothing,
        // and every later one sees the opt-out.
        let f = fixture("settings.json");
        claude_telemetry::install(&f.paths.claude_dir, SERVER, &Environment::default()).unwrap();
        let lock = install_state::Lock::try_take(&f.paths.config_root).unwrap();
        assert_eq!(run_with(&f.paths, "0.30.0", &Environment::default()), None);
        assert!(
            claude_telemetry::uninstall(&f.paths.claude_dir, &Environment::default(), None)
                .unwrap()
        );
        install_state::update_in(&f.paths.config_root, |s| s.claude_telemetry_opt_out = true)
            .unwrap();
        drop(lock);

        for version in ["0.30.0", "0.31.0"] {
            assert_eq!(run_with(&f.paths, version, &Environment::default()), None);
        }
        assert_eq!(user_settings(&f.paths), json!({}));
        let state = install_state::load_in(&f.paths.config_root);
        assert!(state.claude_telemetry_opt_out);
        assert_eq!(state.configured_version.as_deref(), Some("0.31.0"));
    }

    #[test]
    fn the_interval_the_upgrade_added_is_recorded_as_ours() {
        let f = fixture("settings.json");
        run_with(&f.paths, "0.30.0", &Environment::default());
        assert_eq!(
            install_state::load_in(&f.paths.config_root).telemetry_added_interval,
            Some(true)
        );
    }

    #[test]
    fn a_linked_worktrees_own_settings_are_refreshed_too() {
        let f = fixture("settings.local.json");
        let worktree = f._home.path().join("wt");
        fs::create_dir_all(worktree.join(".claude")).unwrap();
        fs::write(
            worktree.join(".claude/settings.json"),
            json!({"hooks": {"Stop": [{"matcher": "", "hooks": [
                {"type": "command", "command": "tracevault stream --event stop"}]}]}})
            .to_string(),
        )
        .unwrap();
        let paths = Paths {
            worktree_root: Some(worktree.clone()),
            config_root: f.paths.config_root.clone(),
            claude_dir: f.paths.claude_dir.clone(),
            project_root: f.paths.project_root.clone(),
        };
        run_with(&paths, "0.30.0", &Environment::default());
        let wt = json_at(&worktree.join(".claude/settings.json"));
        assert!(wt["hooks"]["SessionStart"].is_array());
        assert!(repo_settings(&paths, "settings.local.json")["hooks"]["SessionStart"].is_array());
    }
}
