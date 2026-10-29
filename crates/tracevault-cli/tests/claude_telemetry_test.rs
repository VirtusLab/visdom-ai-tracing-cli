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

/// A fake IdP: answers OpenID discovery (naming itself as issuer), then one
/// token request with `token_status`/`token_body`. Returns its URL.
fn spawn_idp(token_status: &'static str, token_body: &'static str) -> String {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let discovery = format!(
        r#"{{"issuer":"{base}","token_endpoint":"{base}/token","device_authorization_endpoint":"{base}/device"}}"#
    );
    std::thread::spawn(move || {
        for (status, body) in [("200 OK", discovery.as_str()), (token_status, token_body)] {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            // Consume the whole request (headers, then any form body).
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut buf = vec![0; length];
            let _ = reader.read_exact(&mut buf);
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    base
}

fn write_expiring_session(home: &Path, issuer: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    write_credentials(
        home,
        serde_json::json!({
            "server_url": SERVER,
            "email": "dev@example.com",
            "auth": {
                "issuer": issuer,
                "client_id": "tracevault-cli",
                "refresh_token": "rt_old",
                "access_token": "at_stale",
                // Valid, but not for the next helper interval.
                "access_expires_at": now + 30
            }
        }),
    );
}

#[test]
fn otel_headers_refreshes_an_expiring_session_and_persists_it() {
    let home = TempDir::new().unwrap();
    let issuer = spawn_idp(
        "200 OK",
        r#"{"access_token":"at_new","refresh_token":"rt_new","expires_in":300}"#,
    );
    write_expiring_session(home.path(), &issuer);

    let o = run(home.path(), &["otel-headers", "--server-url", SERVER], &[]);
    assert_eq!(
        headers(&o),
        serde_json::json!({"Authorization": "Bearer at_new"})
    );
    let saved =
        std::fs::read_to_string(home.path().join(".config/tracevault/credentials.json")).unwrap();
    assert!(
        saved.contains("at_new") && saved.contains("rt_new"),
        "{saved}"
    );
}

#[test]
fn otel_headers_fails_without_output_when_the_refresh_is_rejected() {
    let home = TempDir::new().unwrap();
    let issuer = spawn_idp("400 Bad Request", r#"{"error":"invalid_grant"}"#);
    write_expiring_session(home.path(), &issuer);

    let o = run(home.path(), &["otel-headers", "--server-url", SERVER], &[]);
    assert!(!o.status.success());
    assert!(
        stdout(&o).is_empty(),
        "no header on failure: {}",
        stdout(&o)
    );
    assert!(!stderr(&o).contains("at_stale"), "{}", stderr(&o));
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

const LOGIN: [(&str, &str); 2] = [
    ("TRACEVAULT_API_KEY", "tvk_secret"),
    ("TRACEVAULT_SERVER_URL", SERVER),
];

fn state(home: &Path) -> serde_json::Value {
    let path = home.join(".config/tracevault/install-state.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap_or_else(|_| "{}".into())).unwrap()
}

fn telemetry_on(home: &Path) -> bool {
    home.join(".claude/settings.json").exists()
        && settings(home)
            .get("otelHeadersHelper")
            .is_some_and(|h| h.as_str().unwrap().starts_with("tracevault otel-headers"))
}

#[cfg(target_os = "linux")]
#[test]
fn fresh_repo_init_turns_telemetry_on_by_default() {
    let home = TempDir::new().unwrap();
    let repo = git_repo();
    let o = run_in(
        repo.path(),
        home.path(),
        &["init", "--claude-settings", "local"],
        &LOGIN,
    );
    assert!(o.status.success(), "stderr={}", stderr(&o));
    assert!(telemetry_on(home.path()));
    assert!(
        stdout(&o).contains("tracevault telemetry off"),
        "{}",
        stdout(&o)
    );
    // Settled at this version: the upgrade step will not redo it.
    assert_eq!(
        state(home.path())["configured_version"],
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn fresh_global_init_turns_telemetry_on_and_not_logged_in_is_only_a_warning() {
    let home = TempDir::new().unwrap();
    let o = run(home.path(), &["init", "--global"], &[]);
    assert!(o.status.success(), "stderr={}", stderr(&o));
    assert!(stderr(&o).contains("Warning"), "{}", stderr(&o));
    assert!(!telemetry_on(home.path()));
    assert!(state(home.path()).get("configured_version").is_none());

    let o = run(home.path(), &["init", "--global"], &LOGIN);
    assert!(o.status.success(), "stderr={}", stderr(&o));
    assert!(telemetry_on(home.path()));
}

#[cfg(target_os = "linux")]
#[test]
fn no_claude_telemetry_removes_ours_and_survives_init_and_upgrade() {
    let home = TempDir::new().unwrap();
    let repo = git_repo();
    let init = |args: &[&str]| {
        let o = run_in(repo.path(), home.path(), args, &LOGIN);
        assert!(o.status.success(), "stderr={}", stderr(&o));
        o
    };
    init(&["init", "--claude-settings", "local"]);
    assert!(telemetry_on(home.path()));

    init(&[
        "init",
        "--claude-settings",
        "local",
        "--no-claude-telemetry",
    ]);
    assert!(!telemetry_on(home.path()));
    assert_eq!(settings(home.path()), serde_json::json!({}));
    assert_eq!(state(home.path())["claude_telemetry_opt_out"], true);

    // A later plain init keeps it off.
    let o = init(&["init", "--claude-settings", "local"]);
    assert!(stdout(&o).contains("stays off"), "{}", stdout(&o));
    assert!(!telemetry_on(home.path()));

    // So does the first session after an upgrade.
    pretend_older_install(home.path());
    let o = session_start(repo.path(), home.path());
    assert!(
        !stdout(&o).contains("telemetry is now on"),
        "{}",
        stdout(&o)
    );
    assert!(!telemetry_on(home.path()));

    // Until the user turns it on explicitly.
    let o = run_in(repo.path(), home.path(), &["telemetry", "on"], &LOGIN);
    assert!(o.status.success(), "stderr={}", stderr(&o));
    assert!(telemetry_on(home.path()));
    assert!(state(home.path()).get("claude_telemetry_opt_out").is_none());
    let o = run_in(repo.path(), home.path(), &["telemetry", "status"], &LOGIN);
    assert!(stdout(&o).contains("exports cost to"), "{}", stdout(&o));
    let o = run_in(repo.path(), home.path(), &["telemetry", "off"], &LOGIN);
    assert!(o.status.success(), "stderr={}", stderr(&o));
    assert!(!telemetry_on(home.path()));
    let o = run_in(repo.path(), home.path(), &["telemetry", "status"], &LOGIN);
    assert!(stdout(&o).contains("you opted out"), "{}", stdout(&o));
}

/// Make the install state look as if an older CLI had configured this
/// machine (keeping an opt-out, if any).
fn pretend_older_install(home: &Path) {
    let mut s = state(home);
    s["configured_version"] = serde_json::json!("0.0.1");
    s.as_object_mut().unwrap().remove("repos");
    std::fs::write(
        home.join(".config/tracevault/install-state.json"),
        s.to_string(),
    )
    .unwrap();
}

/// Run the `SessionStart` hook as Claude Code would, in `cwd`. The server is
/// unreachable on purpose: nothing here may need it.
fn session_start(cwd: &Path, home: &Path) -> Output {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_tracevault"))
        .arg("session-start")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".state"))
        .env("TRACEVAULT_API_KEY", "tvk_secret")
        .env("TRACEVAULT_SERVER_URL", SERVER)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let event = serde_json::json!({
        "session_id": "upgrade-test",
        "transcript_path": "t",
        "cwd": cwd,
        "hook_event_name": "SessionStart"
    });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(event.to_string().as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success(), "stderr={}", stderr(&o));
    serde_json::from_str::<serde_json::Value>(stdout(&o).trim())
        .expect("session-start must print hook JSON");
    o
}

fn system_message(o: &Output) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(stdout(o).trim()).unwrap();
    v.get("systemMessage")
        .and_then(|m| m.as_str())
        .map(String::from)
}

/// A repo set up by an older CLI: its hooks lack events `init` installs now,
/// telemetry was never turned on, and there is no install state.
fn old_install(home: &Path) -> TempDir {
    let repo = git_repo();
    std::fs::create_dir_all(repo.path().join(".claude")).unwrap();
    std::fs::write(
        repo.path().join(".claude/settings.local.json"),
        serde_json::json!({"hooks": {
            "Stop": [{"matcher": "", "hooks": [
                {"type": "command", "command": "tracevault stream --event stop"}]}],
            "SessionStart": [{"matcher": "", "hooks": [
                {"type": "command", "command": "tracevault session-start"}]}]
        }})
        .to_string(),
    )
    .unwrap();
    assert!(!home.join(".config/tracevault/install-state.json").exists());
    repo
}

#[cfg(target_os = "linux")]
#[test]
fn existing_install_is_upgraded_on_its_first_session_with_the_notice_once() {
    let home = TempDir::new().unwrap();
    let repo = old_install(home.path());

    let o = session_start(repo.path(), home.path());
    let notice = system_message(&o).expect("notice on the first session");
    assert!(notice.contains("telemetry is now on"), "{notice}");
    assert!(notice.contains("tracevault telemetry off"), "{notice}");
    assert!(telemetry_on(home.path()));
    let hooks = std::fs::read_to_string(repo.path().join(".claude/settings.local.json")).unwrap();
    assert!(
        hooks.contains("tracevault stream --event subagent-stop"),
        "{hooks}"
    );
    assert!(!repo.path().join(".claude/settings.json").exists());

    let o = session_start(repo.path(), home.path());
    assert_eq!(system_message(&o), None, "the notice is shown once");

    let o = run_in(repo.path(), home.path(), &["telemetry", "status"], &LOGIN);
    assert!(
        stdout(&o).contains("turned on automatically"),
        "{}",
        stdout(&o)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn upgrade_leaves_a_foreign_exporter_untouched() {
    let home = TempDir::new().unwrap();
    let repo = old_install(home.path());
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    let foreign = serde_json::json!({"env": {"OTEL_METRICS_EXPORTER": "otlp",
        "OTEL_EXPORTER_OTLP_ENDPOINT": "http://collector:4318"}});
    std::fs::write(
        home.path().join(".claude/settings.json"),
        foreign.to_string(),
    )
    .unwrap();

    let o = session_start(repo.path(), home.path());
    assert_eq!(system_message(&o), None);
    assert_eq!(settings(home.path()), foreign);
    // Hooks are still brought up to date.
    let hooks = std::fs::read_to_string(repo.path().join(".claude/settings.local.json")).unwrap();
    assert!(hooks.contains("subagent-stop"), "{hooks}");
}

#[cfg(target_os = "linux")]
#[test]
fn default_init_does_not_turn_telemetry_on_while_content_logging_is_set() {
    let home = TempDir::new().unwrap();
    let env = [LOGIN[0], LOGIN[1], ("OTEL_LOG_USER_PROMPTS", "1")];
    let o = run(home.path(), &["init", "--global"], &env);
    assert!(o.status.success(), "stderr={}", stderr(&o));
    assert!(
        stderr(&o).contains("OTEL_LOG_USER_PROMPTS=1"),
        "{}",
        stderr(&o)
    );
    assert!(!telemetry_on(home.path()));

    // Asking for it explicitly is consent.
    let o = run(home.path(), &["telemetry", "on"], &env);
    assert!(o.status.success(), "stderr={}", stderr(&o));
    assert!(telemetry_on(home.path()));
}

#[test]
fn both_telemetry_flags_conflict() {
    let home = TempDir::new().unwrap();
    let o = run(
        home.path(),
        &[
            "init",
            "--global",
            "--claude-telemetry",
            "--no-claude-telemetry",
        ],
        &[],
    );
    assert!(!o.status.success());
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
