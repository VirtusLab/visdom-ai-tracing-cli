//! End-to-end tests for Claude Code telemetry export (VIS-1081):
//! `tracevault otel-headers` and `tracevault init --global --claude-telemetry`.
//!
//! They run the compiled binary with a cleared environment pointed at a temp
//! HOME, so neither the developer's credentials nor their `OTEL_*` variables
//! leak in. Nothing here makes a network call.

use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

const SERVER: &str = "https://tv.example.com";

fn run(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    run_in(home, home, args, env)
}

/// `stdin` is not a terminal here (`output()` closes it), so an `init`
/// without `--claude-telemetry` never prompts.
fn run_in(cwd: &Path, home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tracevault"));
    cmd.args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .current_dir(cwd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("failed to run tracevault binary")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn write_credentials(home: &Path, body: serde_json::Value) {
    let dir = home.join(".config").join("tracevault");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("credentials.json"), body.to_string()).unwrap();
}

fn headers(o: &Output) -> serde_json::Value {
    assert!(o.status.success(), "stderr={}", stderr(o));
    serde_json::from_str(stdout(o).trim()).expect("otel-headers must print JSON")
}

#[test]
fn otel_headers_prints_the_api_key_from_the_environment() {
    let home = TempDir::new().unwrap();
    let o = run(
        home.path(),
        &["otel-headers", "--server-url", SERVER],
        &[
            ("TRACEVAULT_API_KEY", "tvk_env"),
            ("TRACEVAULT_SERVER_URL", SERVER),
        ],
    );
    assert_eq!(
        headers(&o),
        serde_json::json!({"Authorization": "Bearer tvk_env"})
    );
}

#[test]
fn otel_headers_prints_the_logged_in_session_token() {
    let home = TempDir::new().unwrap();
    write_credentials(
        home.path(),
        serde_json::json!({
            "server_url": SERVER,
            "email": "dev@example.com",
            "auth": {
                "issuer": "https://idp.example.com/realms/x",
                "client_id": "tracevault-cli",
                "refresh_token": "rt",
                "access_token": "at_fresh",
                // Far from expiry: no refresh, so no network call.
                "access_expires_at": 4_000_000_000i64
            }
        }),
    );
    let o = run(home.path(), &["otel-headers", "--server-url", SERVER], &[]);
    assert_eq!(
        headers(&o),
        serde_json::json!({"Authorization": "Bearer at_fresh"})
    );
}

#[test]
fn otel_headers_fails_clearly_when_not_logged_in() {
    let home = TempDir::new().unwrap();
    let o = run(home.path(), &["otel-headers", "--server-url", SERVER], &[]);
    assert!(!o.status.success());
    assert!(stdout(&o).is_empty(), "nothing on stdout: {}", stdout(&o));
    assert!(stderr(&o).contains("Not logged in"), "{}", stderr(&o));
}

#[test]
fn otel_headers_refuses_a_credential_for_another_server() {
    let home = TempDir::new().unwrap();
    write_credentials(
        home.path(),
        serde_json::json!({"server_url": "https://other.example.com", "token": "tvk_other"}),
    );
    let o = run(home.path(), &["otel-headers", "--server-url", SERVER], &[]);
    assert!(!o.status.success());
    assert!(
        !stdout(&o).contains("tvk_other"),
        "token must not be printed"
    );
    assert!(
        stderr(&o).contains("https://other.example.com"),
        "{}",
        stderr(&o)
    );
}

fn settings(home: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(home.join(".claude").join("settings.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[cfg(target_os = "linux")]
#[test]
fn global_init_with_claude_telemetry_writes_user_settings_without_the_key() {
    let home = TempDir::new().unwrap();
    let env = [
        ("TRACEVAULT_API_KEY", "tvk_secret"),
        ("TRACEVAULT_SERVER_URL", SERVER),
    ];
    let o = run(
        home.path(),
        &["init", "--global", "--claude-telemetry"],
        &env,
    );
    assert!(o.status.success(), "stderr={}", stderr(&o));
    assert!(stdout(&o).contains("/otlp/v1/logs"), "{}", stdout(&o));

    let s = settings(home.path());
    assert_eq!(s["env"]["CLAUDE_CODE_ENABLE_TELEMETRY"], "1");
    assert_eq!(s["env"]["OTEL_LOGS_EXPORTER"], "otlp");
    assert_eq!(
        s["env"]["OTEL_EXPORTER_OTLP_LOGS_PROTOCOL"],
        "http/protobuf"
    );
    assert_eq!(
        s["env"]["OTEL_EXPORTER_OTLP_LOGS_ENDPOINT"],
        "https://tv.example.com/otlp/v1/logs"
    );
    assert_eq!(
        s["otelHeadersHelper"],
        "tracevault otel-headers --server-url https://tv.example.com"
    );
    // The hooks are still installed next to it, and no key is written.
    assert!(s["hooks"]["SessionStart"].is_array());
    assert!(!s.to_string().contains("tvk_secret"));

    // Idempotent: a re-run leaves the file byte-identical.
    let before = std::fs::read(home.path().join(".claude/settings.json")).unwrap();
    let o = run(
        home.path(),
        &["init", "--global", "--claude-telemetry"],
        &env,
    );
    assert!(o.status.success(), "stderr={}", stderr(&o));
    let after = std::fs::read(home.path().join(".claude/settings.json")).unwrap();
    assert_eq!(before, after);
}

#[cfg(target_os = "linux")]
#[test]
fn global_init_with_claude_telemetry_fails_when_not_logged_in() {
    let home = TempDir::new().unwrap();
    let o = run(
        home.path(),
        &["init", "--global", "--claude-telemetry"],
        &[],
    );
    assert!(!o.status.success());
    assert!(stderr(&o).contains("Not logged in"), "{}", stderr(&o));
    let s = settings(home.path());
    assert!(s.get("otelHeadersHelper").is_none(), "{s}");
}

#[cfg(target_os = "linux")]
#[test]
fn global_init_leaves_a_foreign_exporter_alone_and_prints_the_collector_route() {
    let home = TempDir::new().unwrap();
    let claude = home.path().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    let foreign = serde_json::json!({
        "env": {
            "OTEL_LOGS_EXPORTER": "otlp",
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT": "https://collector.corp/v1/logs"
        }
    });
    std::fs::write(claude.join("settings.json"), foreign.to_string()).unwrap();

    let o = run(
        home.path(),
        &["init", "--global", "--claude-telemetry"],
        &[
            ("TRACEVAULT_API_KEY", "tvk_secret"),
            ("TRACEVAULT_SERVER_URL", SERVER),
        ],
    );
    assert!(o.status.success(), "stderr={}", stderr(&o));
    assert!(stderr(&o).contains("otlphttp/tracevault"), "{}", stderr(&o));
    assert!(stderr(&o).contains("collector.corp"), "{}", stderr(&o));

    let s = settings(home.path());
    assert_eq!(s["env"], foreign["env"]);
    assert!(s.get("otelHeadersHelper").is_none());
}

/// A git repo with no `origin` remote: `init` installs everything and skips
/// server registration, so it makes no network call.
fn git_repo() -> TempDir {
    let repo = TempDir::new().unwrap();
    let ok = Command::new("git")
        .args(["init", "-q"])
        .current_dir(repo.path())
        .status()
        .unwrap()
        .success();
    assert!(ok, "git init failed");
    repo
}

#[cfg(target_os = "linux")]
#[test]
fn repo_init_with_claude_telemetry_writes_user_level_settings_only() {
    let home = TempDir::new().unwrap();
    let repo = git_repo();
    let env = [
        ("TRACEVAULT_API_KEY", "tvk_secret"),
        ("TRACEVAULT_SERVER_URL", SERVER),
    ];
    let o = run_in(
        repo.path(),
        home.path(),
        &["init", "--claude-telemetry", "--claude-settings", "local"],
        &env,
    );
    assert!(o.status.success(), "stderr={}", stderr(&o));

    let s = settings(home.path());
    assert_eq!(
        s["otelHeadersHelper"],
        "tracevault otel-headers --server-url https://tv.example.com"
    );
    // The repo's own settings get the hooks, never the telemetry: Claude Code
    // ignores OTel exporter settings there.
    let repo_settings =
        std::fs::read_to_string(repo.path().join(".claude/settings.local.json")).unwrap();
    assert!(
        repo_settings.contains("tracevault stream"),
        "{repo_settings}"
    );
    assert!(
        !repo_settings.contains("otelHeadersHelper"),
        "{repo_settings}"
    );
    assert!(!repo_settings.contains("OTEL_"), "{repo_settings}");
}

#[cfg(target_os = "linux")]
#[test]
fn non_interactive_repo_init_without_the_flag_leaves_telemetry_off() {
    let home = TempDir::new().unwrap();
    let repo = git_repo();
    let o = run_in(
        repo.path(),
        home.path(),
        &["init", "--claude-settings", "local"],
        &[
            ("TRACEVAULT_API_KEY", "tvk_secret"),
            ("TRACEVAULT_SERVER_URL", SERVER),
        ],
    );
    assert!(o.status.success(), "stderr={}", stderr(&o));
    assert!(!home.path().join(".claude/settings.json").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn repo_init_refuses_telemetry_for_a_server_other_than_the_login() {
    let home = TempDir::new().unwrap();
    let repo = git_repo();
    write_credentials(
        home.path(),
        serde_json::json!({"server_url": SERVER, "token": "tvk_file"}),
    );
    let o = run_in(
        repo.path(),
        home.path(),
        &[
            "init",
            "--claude-telemetry",
            "--claude-settings",
            "local",
            "--server-url",
            "https://other.example.com",
        ],
        &[],
    );
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("logged in to https://tv.example.com"),
        "{}",
        stderr(&o)
    );
    assert!(!home.path().join(".claude/settings.json").exists());
}

#[test]
fn claude_telemetry_is_rejected_for_other_agents() {
    let home = TempDir::new().unwrap();
    let o = run(
        home.path(),
        &["init", "--global", "--agent", "codex", "--claude-telemetry"],
        &[],
    );
    assert!(!o.status.success());
    assert!(stderr(&o).contains("--claude-telemetry"), "{}", stderr(&o));
}
