//! End-to-end dispatch tests for `tracevault init --global`.
//!
//! These run the *compiled binary* (not the library functions directly) so
//! that clap's arg parsing/conflict wiring and the `Cli::Init { global }`
//! dispatch block in `main.rs` are actually exercised, not just
//! `install_global_hooks` in isolation.

use std::process::Command;
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tracevault")
}

/// `tracevault init --global` with `HOME` pointed at an empty tempdir must
/// succeed and install both `.claude/settings.json` (with our hooks) and
/// `.claude/CLAUDE.md`.
///
/// `dirs::home_dir()` reads `$HOME` on Linux, and setting `HOME` via
/// `Command::env` only affects this child process's environment — it's not a
/// global/process-wide mutation, so this is race-free under parallel tests.
/// `init --global` makes no network calls, so the test is fully hermetic.
#[cfg(target_os = "linux")]
#[test]
fn global_install_writes_settings_and_claude_md_under_home() {
    let home = TempDir::new().unwrap();

    let output = Command::new(bin())
        .args(["init", "--global"])
        .env("HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("failed to run tracevault binary");

    assert!(
        output.status.success(),
        "tracevault init --global should succeed; stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let settings_path = home.path().join(".claude").join("settings.json");
    let claude_md_path = home.path().join(".claude").join("CLAUDE.md");

    assert!(settings_path.exists(), "settings.json was not written");
    assert!(claude_md_path.exists(), "CLAUDE.md was not written");

    let settings_content = std::fs::read_to_string(&settings_path).unwrap();
    let settings: serde_json::Value =
        serde_json::from_str(&settings_content).expect("settings.json must parse as JSON");
    assert!(
        settings["hooks"]["SessionStart"].is_array(),
        "hooks.SessionStart missing from installed settings.json: {settings_content}"
    );
    assert!(
        settings["hooks"]["UserPromptSubmit"].is_array(),
        "hooks.UserPromptSubmit missing from installed settings.json: {settings_content}"
    );

    // Success output should mention both files that were written, so the
    // user sees the CLAUDE.md side effect and not just the settings write.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("settings.json"),
        "stdout should mention settings.json: {stdout}"
    );
    assert!(
        stdout.contains("CLAUDE.md"),
        "stdout should mention CLAUDE.md: {stdout}"
    );
}

/// `--global` combined with a per-repo-only flag (e.g. `--server-url`) must
/// be rejected by clap at parse time with a clear conflict error, rather than
/// silently ignoring the extra flag.
#[test]
fn global_with_server_url_is_a_clap_conflict() {
    let tmp = TempDir::new().unwrap();

    let output = Command::new(bin())
        .args(["init", "--global", "--server-url", "http://x"])
        .current_dir(tmp.path())
        .output()
        .expect("failed to run tracevault binary");

    assert!(
        !output.status.success(),
        "init --global --server-url ... must fail, not silently ignore --server-url"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("cannot be used with")
            || stderr.to_lowercase().contains("conflict"),
        "stderr should explain the flag conflict: {stderr}"
    );
    assert!(
        stderr.contains("--global") || stderr.contains("--server-url"),
        "stderr should name the conflicting flags: {stderr}"
    );
}
