use std::fs;
use tempfile::TempDir;
use tracevault_cli::commands::init::ClaudeSettingsTarget;

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
}

#[tokio::test]
async fn init_installs_git_pre_push_hook() {
    let tmp = tmp_git_repo();

    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Shared),
        false,
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
    )
    .await
    .unwrap();
    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Shared),
        false,
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
    )
    .await
    .unwrap();
    tracevault_cli::commands::init::init_in_directory(
        tmp.path(),
        None,
        Some(ClaudeSettingsTarget::Shared),
        false,
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
