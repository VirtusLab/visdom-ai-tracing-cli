use std::fs;
use tempfile::TempDir;
use tracevault_cli::agent::Agent;
use tracevault_cli::commands::init::ClaudeSettingsTarget;
use tracevault_cli::config::UserContext;

fn tmp_git_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    tmp
}

#[tokio::test]
async fn init_fails_without_git() {
    let tmp = TempDir::new().unwrap();
    let result = tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Shared),
        false,
        UserContext::Toggle(true),
        Agent::ClaudeCode,
    )
    .await;
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Not a git repository"));
}

#[tokio::test]
async fn init_creates_tracevault_config() {
    let tmp = tmp_git_repo();
    let config_path = tmp.path().join(".tracevault").join("config.toml");

    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Shared),
        false,
        UserContext::Toggle(true),
        Agent::ClaudeCode,
    )
    .await
    .unwrap();

    assert!(config_path.exists());
    let content = fs::read_to_string(&config_path).unwrap();
    assert!(content.contains("claude-code"));
}

#[tokio::test]
async fn init_creates_directory_structure() {
    let tmp = tmp_git_repo();

    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Shared),
        false,
        UserContext::Toggle(true),
        Agent::ClaudeCode,
    )
    .await
    .unwrap();

    assert!(tmp.path().join(".tracevault").exists());
    assert!(tmp.path().join(".tracevault/sessions").exists());
    assert!(tmp.path().join(".tracevault/cache").exists());

    let gitignore = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
    assert!(gitignore.contains(".tracevault/"));
    assert!(gitignore.contains(".claude/settings.json"));
    // Only the settings file init actually wrote is gitignored. init never
    // touched settings.local.json (Shared target), so it must not be added.
    assert!(!gitignore.contains(".claude/settings.local.json"));
}

#[tokio::test]
async fn init_installs_claude_hooks() {
    let tmp = tmp_git_repo();

    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Shared),
        false,
        UserContext::Toggle(true),
        Agent::ClaudeCode,
    )
    .await
    .unwrap();

    let settings_path = tmp.path().join(".claude/settings.json");
    assert!(settings_path.exists());

    let content = fs::read_to_string(&settings_path).unwrap();
    let settings: serde_json::Value = serde_json::from_str(&content).unwrap();
    let hooks = settings.get("hooks").unwrap();
    assert!(hooks.get("PreToolUse").is_some());
    assert!(hooks.get("PostToolUse").is_some());
    assert!(hooks.get("Notification").is_some());
}

#[tokio::test]
async fn init_installs_session_hooks() {
    let tmp = tmp_git_repo();

    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Shared),
        false,
        UserContext::Toggle(true),
        Agent::ClaudeCode,
    )
    .await
    .unwrap();

    let settings_path = tmp.path().join(".claude/settings.json");
    assert!(settings_path.exists());

    let content = fs::read_to_string(&settings_path).unwrap();
    let settings: serde_json::Value = serde_json::from_str(&content).unwrap();
    let hooks = settings.get("hooks").unwrap();

    // Per-repo init installs SessionStart hook (exports session ID and injects policies)
    assert!(
        hooks.get("SessionStart").is_some(),
        "missing SessionStart hook"
    );
    assert!(
        hooks["SessionStart"].is_array(),
        "SessionStart should be an array"
    );
    let session_start_entry = &hooks["SessionStart"][0];
    let session_start_cmd = session_start_entry
        .get("hooks")
        .and_then(|h| h.get(0))
        .and_then(|h| h.get("command"))
        .and_then(|c| c.as_str());
    assert_eq!(
        session_start_cmd,
        Some("tracevault session-start"),
        "SessionStart hook should run 'tracevault session-start'"
    );

    // Per-repo init installs UserPromptSubmit hook (re-injects policies when repo changes)
    assert!(
        hooks.get("UserPromptSubmit").is_some(),
        "missing UserPromptSubmit hook"
    );
    assert!(
        hooks["UserPromptSubmit"].is_array(),
        "UserPromptSubmit should be an array"
    );
    let user_prompt_entry = &hooks["UserPromptSubmit"][0];
    let user_prompt_cmd = user_prompt_entry
        .get("hooks")
        .and_then(|h| h.get(0))
        .and_then(|h| h.get("command"))
        .and_then(|c| c.as_str());
    assert_eq!(
        user_prompt_cmd,
        Some("tracevault user-prompt"),
        "UserPromptSubmit hook should run 'tracevault user-prompt'"
    );
}

#[tokio::test]
async fn init_merges_into_existing_settings() {
    let tmp = tmp_git_repo();

    // Pre-existing settings.json with other config
    let claude_dir = tmp.path().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(claude_dir.join("settings.json"), r#"{"model": "opus"}"#).unwrap();

    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Shared),
        false,
        UserContext::Toggle(true),
        Agent::ClaudeCode,
    )
    .await
    .unwrap();

    let content = fs::read_to_string(claude_dir.join("settings.json")).unwrap();
    let settings: serde_json::Value = serde_json::from_str(&content).unwrap();

    // Hooks were added
    assert!(settings.get("hooks").is_some());
    // Existing config preserved
    assert_eq!(settings.get("model").unwrap(), "opus");
}

#[test]
fn tracevault_hooks_has_pre_post_and_notification() {
    let hooks = tracevault_cli::commands::init::tracevault_hooks();
    assert!(hooks.get("PreToolUse").is_some());
    assert!(hooks.get("PostToolUse").is_some());
    assert!(hooks.get("Notification").is_some());
    assert!(hooks.get("SessionStart").is_some());
    assert!(hooks.get("UserPromptSubmit").is_some());
}

#[tokio::test]
async fn init_installs_git_pre_push_hook() {
    let tmp = tmp_git_repo();

    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Shared),
        false,
        UserContext::Toggle(true),
        Agent::ClaudeCode,
    )
    .await
    .unwrap();

    let hook_path = tmp.path().join(".git/hooks/pre-push");
    assert!(hook_path.exists());

    let content = fs::read_to_string(&hook_path).unwrap();
    assert!(content.contains("#!/bin/sh"));
    assert!(content.contains("# tracevault:enforce"));
    assert!(content.contains("tracevault sync"));
    assert!(content.contains("tracevault check"));
    assert!(!content.contains("tracevault push"));
}

#[tokio::test]
async fn init_preserves_existing_pre_push_hook() {
    let tmp = tmp_git_repo();

    // Create existing hook
    let hooks_dir = tmp.path().join(".git/hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    fs::write(
        hooks_dir.join("pre-push"),
        "#!/bin/sh\necho 'existing hook'\n",
    )
    .unwrap();

    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Shared),
        false,
        UserContext::Toggle(true),
        Agent::ClaudeCode,
    )
    .await
    .unwrap();

    let content = fs::read_to_string(hooks_dir.join("pre-push")).unwrap();
    // Existing content preserved
    assert!(content.contains("echo 'existing hook'"));
    // Tracevault appended
    assert!(content.contains("# tracevault:enforce"));
    assert!(content.contains("tracevault check"));
    assert!(!content.contains("tracevault push"));
}

#[tokio::test]
async fn init_does_not_duplicate_hook_on_reinit() {
    let tmp = tmp_git_repo();

    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Shared),
        false,
        UserContext::Toggle(true),
        Agent::ClaudeCode,
    )
    .await
    .unwrap();
    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Shared),
        false,
        UserContext::Toggle(true),
        Agent::ClaudeCode,
    )
    .await
    .unwrap();

    let content = fs::read_to_string(tmp.path().join(".git/hooks/pre-push")).unwrap();
    let marker_count = content.matches("# tracevault:enforce").count();
    assert_eq!(
        marker_count, 1,
        "Marker should appear exactly once, found {marker_count}"
    );
}

#[tokio::test]
async fn init_installs_post_commit_hook() {
    let tmp = tmp_git_repo();

    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Shared),
        false,
        UserContext::Toggle(true),
        Agent::ClaudeCode,
    )
    .await
    .unwrap();

    let hook_path = tmp.path().join(".git/hooks/post-commit");
    assert!(hook_path.exists());

    let content = fs::read_to_string(&hook_path).unwrap();
    assert!(content.contains("#!/bin/sh"));
    assert!(content.contains("# tracevault:post-commit"));
    assert!(content.contains("tracevault commit-push 2>/dev/null &"));
}

#[tokio::test]
async fn init_does_not_duplicate_post_commit_hook_on_reinit() {
    let tmp = tmp_git_repo();

    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Shared),
        false,
        UserContext::Toggle(true),
        Agent::ClaudeCode,
    )
    .await
    .unwrap();
    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Shared),
        false,
        UserContext::Toggle(true),
        Agent::ClaudeCode,
    )
    .await
    .unwrap();

    let content = fs::read_to_string(tmp.path().join(".git/hooks/post-commit")).unwrap();
    let marker_count = content.matches("# tracevault:post-commit").count();
    assert_eq!(
        marker_count, 1,
        "Post-commit marker should appear exactly once, found {marker_count}"
    );
}

#[tokio::test]
async fn init_local_target_writes_to_settings_local_json() {
    let tmp = tmp_git_repo();

    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Local),
        false,
        UserContext::Toggle(true),
        Agent::ClaudeCode,
    )
    .await
    .unwrap();

    let local_path = tmp.path().join(".claude/settings.local.json");
    let shared_path = tmp.path().join(".claude/settings.json");
    assert!(local_path.exists(), "settings.local.json should exist");
    assert!(
        !shared_path.exists(),
        "settings.json should not be created when local target chosen"
    );

    let content = fs::read_to_string(&local_path).unwrap();
    let settings: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert!(settings.get("hooks").is_some());
}

#[tokio::test]
async fn init_local_target_gitignores_settings_local_json() {
    let tmp = tmp_git_repo();

    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Local),
        false,
        UserContext::Toggle(true),
        Agent::ClaudeCode,
    )
    .await
    .unwrap();

    let gitignore = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
    assert!(gitignore.contains(".claude/settings.local.json"));
    // Only the chosen settings file is gitignored; init didn't touch
    // settings.json (Local target), so it must not be added.
    assert!(!gitignore.contains(".claude/settings.json"));
}

#[tokio::test]
async fn init_local_target_merges_into_existing_settings_local_json() {
    let tmp = tmp_git_repo();

    let claude_dir = tmp.path().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(
        claude_dir.join("settings.local.json"),
        r#"{"model": "opus"}"#,
    )
    .unwrap();

    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Local),
        false,
        UserContext::Toggle(true),
        Agent::ClaudeCode,
    )
    .await
    .unwrap();

    let content = fs::read_to_string(claude_dir.join("settings.local.json")).unwrap();
    let settings: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert!(settings.get("hooks").is_some());
    assert_eq!(settings.get("model").unwrap(), "opus");
}

#[tokio::test]
async fn init_writes_server_url_to_config() {
    let tmp = tmp_git_repo();

    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        Some("https://tv.example.com"),
        Some(ClaudeSettingsTarget::Shared),
        false,
        UserContext::Toggle(true),
        Agent::ClaudeCode,
    )
    .await
    .unwrap();

    let config_path = tmp.path().join(".tracevault/config.toml");
    let content = fs::read_to_string(&config_path).unwrap();
    assert!(content.contains("server_url = \"https://tv.example.com\""));
}

#[tokio::test]
async fn init_no_gitignore_skips_gitignore_update() {
    let tmp = tmp_git_repo();

    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Shared),
        true,
        UserContext::Toggle(true),
        Agent::ClaudeCode,
    )
    .await
    .unwrap();

    // .gitignore should not exist (tmp_git_repo creates a bare repo without one)
    // or should not contain any tracevault entries if it already existed
    let gitignore_path = tmp.path().join(".gitignore");
    if gitignore_path.exists() {
        let content = fs::read_to_string(&gitignore_path).unwrap();
        assert!(
            !content.contains(".tracevault/"),
            ".gitignore should not have been modified with --no-gitignore"
        );
        assert!(!content.contains(".claude/settings.json"));
    }
    // But the rest of init should still work
    assert!(tmp.path().join(".tracevault").exists());
    assert!(tmp.path().join(".claude/settings.json").exists());
}

#[tokio::test]
async fn init_default_writes_user_context_true() {
    let tmp = tmp_git_repo();

    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Shared),
        false,
        UserContext::Toggle(true),
        Agent::ClaudeCode,
    )
    .await
    .unwrap();

    let config_path = tmp.path().join(".tracevault/config.toml");
    let content = fs::read_to_string(&config_path).unwrap();
    assert!(
        content.contains("user_context = true"),
        "expected user_context = true, got: {content}"
    );
}

#[test]
fn cleanup_stale_gsd_registration_removes_tracker_and_inert_entries() {
    let tmp = TempDir::new().unwrap();
    let gsd_home = tmp.path().join(".gsd");
    // Pre-seed a stale tracker (the earlier, incorrect Codex-clone shim) and
    // a hand-written (inert) "tracevault" entry from an older version of
    // this installer, plus an unrelated extension, to prove cleanup removes
    // the stale ones while never dropping a user's other extensions.
    fs::create_dir_all(gsd_home.join("extensions").join("tracevault-tracker")).unwrap();
    fs::write(
        gsd_home.join("extensions").join("registry.json"),
        r#"{"version":1,"entries":{"tracevault-tracker":{"id":"tracevault-tracker","enabled":true,"source":"user"},"tracevault":{"id":"tracevault","enabled":true,"source":"user"},"other-ext":{"id":"other-ext","enabled":true,"source":"bundled"}}}"#,
    )
    .unwrap();

    tracevault_cli::commands::init::cleanup_stale_gsd_registration(&gsd_home).unwrap();

    let reg: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(gsd_home.join("extensions").join("registry.json")).unwrap(),
    )
    .unwrap();
    assert!(
        reg["entries"].get("tracevault-tracker").is_none(),
        "stale tracker entry removed"
    );
    assert!(
        reg["entries"].get("tracevault").is_none(),
        "stale inert tracevault registry entry removed (gsd install manages registration now)"
    );
    assert!(
        !gsd_home
            .join("extensions")
            .join("tracevault-tracker")
            .exists(),
        "stale tracker dir removed"
    );
    // Verify unrelated extension is preserved and unchanged.
    assert!(
        reg["entries"].get("other-ext").is_some(),
        "other-ext should be preserved during cleanup"
    );
    assert_eq!(
        reg["entries"]["other-ext"]["id"],
        serde_json::json!("other-ext")
    );
    assert_eq!(
        reg["entries"]["other-ext"]["enabled"],
        serde_json::json!(true)
    );
    assert_eq!(
        reg["entries"]["other-ext"]["source"],
        serde_json::json!("bundled")
    );
}

#[test]
fn cleanup_stale_gsd_registration_is_noop_when_registry_missing() {
    let tmp = TempDir::new().unwrap();
    let gsd_home = tmp.path().join(".gsd");

    // No pre-existing registry.json (and no extensions/ dir at all): nothing
    // to clean up, so this is a no-op that succeeds without creating one.
    tracevault_cli::commands::init::cleanup_stale_gsd_registration(&gsd_home).unwrap();
    assert!(!gsd_home.join("extensions").join("registry.json").exists());
}

#[test]
fn cleanup_stale_gsd_registration_skips_corrupt_registry_without_overwriting() {
    let tmp = TempDir::new().unwrap();
    let gsd_home = tmp.path().join(".gsd");
    let ext_root = gsd_home.join("extensions");
    fs::create_dir_all(&ext_root).unwrap();
    let reg_path = ext_root.join("registry.json");

    // Seed a stale tracker dir alongside the corrupt registry to prove the
    // corrupt path performs ZERO mutation under ~/.gsd — the registry is
    // validated BEFORE anything is touched, so the tracker dir is NOT deleted.
    let tracker_dir = ext_root.join("tracevault-tracker");
    fs::create_dir_all(&tracker_dir).unwrap();

    // A registry.json that exists but fails to parse must NOT be silently
    // reset to an empty skeleton (that would destroy every other extension
    // the user had registered). Cleanup must skip it — leaving it untouched
    // — rather than error out or overwrite it; a corrupt registry must
    // never block the rest of `install_gsd_extension`.
    let corrupt_content = "{ this is not json";
    fs::write(&reg_path, corrupt_content).unwrap();

    let result = tracevault_cli::commands::init::cleanup_stale_gsd_registration(&gsd_home);
    assert!(
        result.is_ok(),
        "cleanup must not fail the install on a corrupt existing registry"
    );

    let content_after = fs::read_to_string(&reg_path).unwrap();
    assert_eq!(
        content_after, corrupt_content,
        "corrupt registry.json must be left byte-for-byte unchanged"
    );
    assert!(
        tracker_dir.exists(),
        "a corrupt registry must be validated before any fs mutation — the \
         stale tracker dir must NOT be removed (zero mutation under ~/.gsd)"
    );
}

/// `tracevault init --agent gsd` (repo-local, NOT `--global`) with `HOME`
/// pointed at an empty tempdir. This runs the *compiled binary* (like
/// `global_install_test.rs` does for `--global`) rather than calling
/// `init_in_directory` in-process, specifically so `HOME` can be redirected
/// via `Command::env` (child-process-local, race-free under parallel tests)
/// instead of `std::env::set_var` (process-wide, which would race other
/// tests running concurrently in this same test binary). It also means the
/// `gsd install` shell-out inside `install_gsd_extension` (if `gsd` happens
/// to be on PATH) is sandboxed to this fake `HOME` too.
///
/// The `gsd` binary is not guaranteed to be present in CI, so this only
/// asserts what must hold either way: `init` succeeds and the extension
/// source (all three files) is written to the stable source dir —
/// `install_gsd_extension` must degrade gracefully (warn, still return
/// `Ok`) rather than fail `init` when `gsd install` can't run.
#[cfg(target_os = "linux")]
#[test]
fn init_gsd_repo_local_installs_extension_and_gitignores_gsd_dir() {
    let repo = tmp_git_repo();
    let home = TempDir::new().unwrap();
    let bin = env!("CARGO_BIN_EXE_tracevault");

    let output = std::process::Command::new(bin)
        .args(["init", "--agent", "gsd"])
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .current_dir(repo.path())
        .output()
        .expect("failed to run tracevault binary");

    assert!(
        output.status.success(),
        "tracevault init --agent gsd should succeed (even if `gsd install` itself can't run); stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Extension source written under the (redirected) user config dir, not
    // the repo — this is what `gsd install <dir> -l` registers.
    let source_dir = home.path().join(".config/tracevault/gsd-extension");
    assert!(source_dir.join("index.ts").exists());
    assert!(source_dir.join("package.json").exists());
    assert!(
        source_dir.join("extension-manifest.json").exists(),
        "extension-manifest.json is required for GSD to load the extension"
    );

    // .gitignore has .tracevault/ AND .gsd/ — the latter because, when `gsd`
    // is on PATH, `gsd install <dir> -l` itself writes a project-local
    // `.gsd/settings.json` registration file into the repo (confirmed live:
    // it lists the installed package path). That's GSD's own local-install
    // state, not something a user should commit, so `init` gitignores it
    // preemptively regardless of whether `gsd` happened to run here.
    let gitignore = fs::read_to_string(repo.path().join(".gitignore")).unwrap();
    assert!(gitignore.contains(".tracevault/"));
    assert!(
        gitignore.contains(".gsd/"),
        "gsd's project-local install state must be gitignored: {gitignore}"
    );
    let blank_lines = gitignore.lines().filter(|l| l.is_empty()).count();
    assert_eq!(
        blank_lines, 1,
        "expected exactly the one intentional separator blank line, got: {gitignore:?}"
    );
}

/// `tracevault init --agent opencode` (repo-local). Unlike GSD, the OpenCode
/// plugin is installed project-locally under `<root>/.opencode/plugins/` —
/// no shell-out, no `HOME` redirection needed — so this calls
/// `init_in_directory` in-process like the Codex/Claude cases above.
#[tokio::test]
async fn init_opencode_repo_local_installs_plugin_and_gitignores_opencode_dir() {
    let tmp = tmp_git_repo();

    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        None,
        false,
        UserContext::Toggle(false),
        Agent::OpenCode,
    )
    .await
    .unwrap();

    let plugin_dir = tmp.path().join(".opencode/plugins/tracevault");
    assert!(plugin_dir.join("index.ts").exists());
    assert!(plugin_dir.join("package.json").exists());

    let gitignore = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
    assert!(gitignore.contains(".tracevault/"));
    assert!(
        gitignore.contains(".opencode/"),
        "opencode plugin dir must be gitignored: {gitignore}"
    );
}

#[tokio::test]
async fn init_explicit_user_context_path_written() {
    let tmp = tmp_git_repo();

    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Shared),
        false,
        UserContext::Path("/tmp/my-context.json".to_string()),
        Agent::ClaudeCode,
    )
    .await
    .unwrap();

    let config_path = tmp.path().join(".tracevault/config.toml");
    let content = fs::read_to_string(&config_path).unwrap();
    assert!(
        content.contains("user_context = \"/tmp/my-context.json\""),
        "expected explicit path, got: {content}"
    );
}
