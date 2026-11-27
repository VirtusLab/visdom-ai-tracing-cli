//! The final check of a setup paid by the developer's Claude subscription
//! (`billing = subscription`), in `visdom init`, `visdom doctor` and a plain
//! `visdom`, against a local stand-in for the tracing server, Control Plane
//! and the AI Gateway that answers as the gateway does (VIS-1399, VIS-1400,
//! VIS-1402), and a script standing in for Claude Code.
#![cfg(target_os = "linux")]

mod common;

use std::path::{Path, PathBuf};

use common::http::MockServer;
use common::{stderr, stdout, Sandbox};

const PERSON_TOKEN: &str = "person-access-token";

/// The product owner's message, character for character.
const NOT_SUPPORTED: &str = "Your platform configuration doesn't support subscription traffic through visdom gateway. Contact your platform administrator or use Claude API key or agent hooks";

const SUITE: (&str, u16, &str) = (
    "GET /api/v1/auth/public-config",
    200,
    r#"{"oidc_enabled":true,"control_plane_url":"{URL}","ai_gateway_url":"{URL}"}"#,
);
const ALLOWED: &str = r#"{"llmGatewayUrl":"{URL}","mcpGatewayUrl":"{URL}","suite":[],"controlPlaneUrl":"{URL}","subscriptionTraffic":"allowed"}"#;
const DENIED: &str = r#"{"llmGatewayUrl":"{URL}","mcpGatewayUrl":"{URL}","suite":[],"controlPlaneUrl":"{URL}","subscriptionTraffic":"denied"}"#;
/// A console from before the setting: no `subscriptionTraffic` at all.
const OLD: &str =
    r#"{"llmGatewayUrl":"{URL}","mcpGatewayUrl":"{URL}","suite":[],"controlPlaneUrl":"{URL}"}"#;
/// `claude-haiku` is listed first but is not served by Anthropic directly.
const PROJECT: &str = r#"{"hours":24,"subscriptionHours":720,"projects":[{"id":"p-1","slug":"alpha","name":"Alpha","budget":null,"models":[{"alias":"claude-haiku","subscription":false},{"alias":"gpt-general","subscription":false},{"alias":"claude-sonnet","subscription":true}]}],"keys":[]}"#;
const SUB_ISSUE: (&str, u16, &str) = (
    "POST /api/me/keys",
    201,
    r#"{"secret":"sk-subscription","id":"sub-1","name":"subscription-dev-1","expiresAt":"2099-01-01T00:00:00Z","billing":"subscription"}"#,
);
/// The console's refusal while the deployment denies subscription traffic.
const KEY_DENIED: (&str, u16, &str) = (
    "POST /api/me/keys",
    403,
    r#"{"error":"This gateway does not take subscription traffic (VISDOM_SUBSCRIPTION_TRAFFIC=deny), so it issues no subscription keys; use an ordinary personal key","code":"subscription_traffic_denied"}"#,
);
const MODELS: (&str, u16, &str) = (
    "GET /v1/models",
    200,
    r#"{"object":"list","data":[{"id":"claude-haiku"},{"id":"gpt-general"},{"id":"claude-sonnet"}]}"#,
);
const MESSAGES: (&str, u16, &str) = (
    "POST /v1/messages",
    200,
    r#"{"type":"message","content":[{"type":"text","text":"hi"}]}"#,
);

/// A gateway under the workspace policy `policy`, whose console's settings
/// are `settings` and which answers a key request with `issue`.
fn gateway(policy: &str, settings: &str, issue: (&str, u16, &str)) -> MockServer {
    let workspace = format!(r#"{{"timezone":"UTC","ai_traffic_policy":"{policy}"}}"#);
    MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &workspace),
        ("GET /api/settings", 200, settings),
        ("GET /api/me/keys", 200, PROJECT),
        issue,
        MODELS,
        MESSAGES,
    ])
}

/// A sandbox logged in to `server`, whose Claude Code is a recording script
/// (signed in with a claude.ai subscription unless told otherwise), set to
/// go through the gateway on the subscription.
fn sandbox(server: &MockServer) -> (Sandbox, PathBuf) {
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    let (agent, log) = sb.recording_agent(&["VISDOM_TRACING_MODE", "ANTHROPIC_MODEL"]);
    sb.write_user_config(&format!(
        "tracing_mode = \"gateway\"\n\n[gateway]\nbilling = \"subscription\"\n\n[agents]\nclaude_code = \"{}\"\n",
        agent.display()
    ));
    (sb, log)
}

fn lines(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

fn count(server: &MockServer, route: &str) -> usize {
    server
        .requests()
        .iter()
        .filter(|r| r.route == route)
        .count()
}

fn init(sb: &Sandbox) -> std::process::Output {
    sb.visdom(sb.repo.path()).arg("init").output().unwrap()
}

fn doctor(sb: &Sandbox) -> serde_json::Value {
    let output = sb
        .visdom(sb.repo.path())
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    serde_json::from_str(&stdout(&output)).unwrap_or_else(|e| panic!("{e}: {}", stdout(&output)))
}

fn finding(report: &serde_json::Value, id: &str) -> (String, String) {
    let found = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == id)
        .unwrap_or_else(|| panic!("no {id} in {report}"));
    (
        found["severity"].as_str().unwrap().to_string(),
        found["message"].as_str().unwrap().to_string(),
    )
}

fn has_line(text: &str, line: &str) -> bool {
    text.lines().any(|l| l == line)
}

/// Everything in order: init passes and says so, doctor reports every check
/// as fine (the key with its expiry date), and the agent starts on the
/// alias that takes subscription calls, not the first one listed.
#[test]
fn a_consistent_subscription_setup_passes_every_check() {
    let server = gateway("per_agent", ALLOWED, SUB_ISSUE);
    let (sb, log) = sandbox(&server);
    let report = doctor(&sb);
    assert_eq!(
        finding(&report, "gateway.subscription_key").0,
        "info",
        "no key kept yet, and doctor issues none"
    );
    assert_eq!(count(&server, "POST /api/me/keys"), 0);

    let output = init(&sb);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("AI Gateway ready for Claude Code"));
    let config = std::fs::read_to_string(sb.user_config()).unwrap();
    assert!(
        config.contains("claude_code_model = \"claude-sonnet\""),
        "the alias that takes a subscription call: {config}"
    );

    let report = doctor(&sb);
    for id in [
        "gateway.subscription",
        "gateway.claude_login",
        "gateway.subscription_model",
        "gateway.subscription_key",
        "gateway.claude_settings",
    ] {
        assert_eq!(finding(&report, id).0, "ok", "{id}: {report}");
    }
    assert!(finding(&report, "gateway.subscription_key")
        .1
        .contains("2099-01-01"));
    assert_eq!(count(&server, "POST /api/me/keys"), 1, "doctor issued none");

    let output = sb.visdom(sb.repo.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let recorded = lines(&log);
    assert!(recorded.contains(&"VISDOM_TRACING_MODE=gateway".to_string()));
    assert!(recorded.contains(&"ANTHROPIC_MODEL=claude-sonnet".to_string()));
    // No terms of a provider are mentioned anywhere.
    for text in [stdout(&output), stderr(&output), report.to_string()] {
        assert!(!text.to_lowercase().contains("terms"), "{text}");
    }
}

/// A deployment that denies subscription traffic: init prints the product
/// owner's message exactly, on its own line, and exits 1 without asking for
/// a key; doctor reports it as `gateway.subscription`.
#[test]
fn a_gateway_that_denies_subscription_traffic_stops_init_with_the_message() {
    let server = gateway("per_agent", DENIED, SUB_ISSUE);
    let (sb, _log) = sandbox(&server);
    let output = init(&sb);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let said = stderr(&output);
    assert!(has_line(&said, NOT_SUPPORTED), "{said}");
    assert!(said.contains("visdom --billing project init"), "{said}");
    assert!(said.contains("visdom --mode hooks init"), "{said}");
    assert!(!stdout(&output).contains("AI Gateway ready"));
    assert_eq!(count(&server, "POST /api/me/keys"), 0);

    let (severity, message) = finding(&doctor(&sb), "gateway.subscription");
    assert_eq!(severity, "error");
    assert_eq!(message.lines().next(), Some(NOT_SUPPORTED));
}

/// An older gateway, whose settings do not have the field, counts as denied.
#[test]
fn an_older_gateway_without_the_setting_counts_as_denied() {
    let server = gateway("per_agent", OLD, SUB_ISSUE);
    let (sb, _log) = sandbox(&server);
    let output = init(&sb);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        has_line(&stderr(&output), NOT_SUPPORTED),
        "{}",
        stderr(&output)
    );
    assert_eq!(count(&server, "POST /api/me/keys"), 0);
    let (severity, message) = finding(&doctor(&sb), "gateway.subscription");
    assert_eq!(severity, "error");
    assert!(message.starts_with(NOT_SUPPORTED), "{message}");
}

/// The console refuses the subscription key with the "denied" code: the
/// same message.
#[test]
fn a_subscription_key_the_console_denies_is_the_same_message() {
    let server = gateway("per_agent", ALLOWED, KEY_DENIED);
    let (sb, _log) = sandbox(&server);
    let output = init(&sb);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        has_line(&stderr(&output), NOT_SUPPORTED),
        "{}",
        stderr(&output)
    );
}

/// A plain `visdom` on a gateway that denies it: like any gateway that
/// cannot be used. The developer's own choice starts the agent with hooks
/// and says why; a workspace that requires the gateway does not start it.
#[test]
fn a_plain_visdom_falls_back_to_hooks_unless_the_gateway_is_required() {
    let server = gateway("per_agent", DENIED, SUB_ISSUE);
    let (sb, log) = sandbox(&server);
    let output = sb.visdom(sb.repo.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let said = stderr(&output);
    assert!(said.contains(NOT_SUPPORTED), "{said}");
    assert!(said.contains("hooks tracing instead"), "{said}");
    assert!(lines(&log).contains(&"VISDOM_TRACING_MODE=hooks".to_string()));
    assert_eq!(count(&server, "POST /api/me/keys"), 0);

    let server = gateway("enforced", DENIED, SUB_ISSUE);
    let (sb, log) = sandbox(&server);
    let output = sb.visdom(sb.repo.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let said = stderr(&output);
    assert!(said.contains(NOT_SUPPORTED), "{said}");
    assert!(said.contains("not started"), "{said}");
    assert!(lines(&log).is_empty(), "the agent must not have run");
}

/// Claude Code not signed in, or signed in with a Console key: said, with
/// the fix, in init, doctor and a launch.
#[test]
fn claude_code_must_be_signed_in_with_a_subscription() {
    let server = gateway("per_agent", ALLOWED, SUB_ISSUE);
    let (sb, log) = sandbox(&server);
    for (status, expected) in [
        (r#"{"loggedIn":false,"authMethod":"none"}"#, "not signed in"),
        (
            r#"{"loggedIn":true,"authMethod":"api_key","apiKeySource":"/login"}"#,
            "Anthropic Console",
        ),
    ] {
        let output = sb
            .visdom(sb.repo.path())
            .arg("init")
            .env("FAKE_CLAUDE_AUTH_STATUS", status)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        let said = stderr(&output);
        assert!(said.contains(expected), "{said}");
        assert!(said.contains("/login"), "{said}");
        assert!(!stdout(&output).contains("AI Gateway ready"));

        let output = sb
            .visdom(sb.repo.path())
            .args(["doctor", "--json"])
            .env("FAKE_CLAUDE_AUTH_STATUS", status)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let report: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
        let (severity, message) = finding(&report, "gateway.claude_login");
        assert_eq!(severity, "error");
        assert!(message.contains("/login"), "{message}");

        let output = sb
            .visdom(sb.repo.path())
            .env("FAKE_CLAUDE_AUTH_STATUS", status)
            .output()
            .unwrap();
        assert!(stderr(&output).contains("hooks tracing instead"));
    }
    let recorded = lines(&log);
    assert!(!recorded.contains(&"VISDOM_TRACING_MODE=gateway".to_string()));
}

/// A configured alias the gateway does not take a subscription call on:
/// init fails naming the ones that qualify; doctor says the same.
#[test]
fn the_model_alias_must_take_subscription_calls() {
    let server = gateway("per_agent", ALLOWED, SUB_ISSUE);
    let (sb, _log) = sandbox(&server);
    let mut config = std::fs::read_to_string(sb.user_config()).unwrap();
    config = config.replace(
        "billing = \"subscription\"\n",
        "billing = \"subscription\"\nclaude_code_model = \"gpt-general\"\n",
    );
    sb.write_user_config(&config);
    let output = init(&sb);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let said = stderr(&output);
    assert!(said.contains("`gpt-general`"), "{said}");
    assert!(said.contains("claude-sonnet"), "{said}");
    let (severity, message) = finding(&doctor(&sb), "gateway.subscription_model");
    assert_eq!(severity, "error");
    assert!(message.contains("claude-sonnet"), "{message}");
    assert!(!message.contains("claude-haiku"), "{message}");
}

/// The model Claude Code would really ask for is checked: `ANTHROPIC_MODEL`
/// or `ANTHROPIC_DEFAULT_HAIKU_MODEL` from the environment (which the launch
/// keeps) and a `--model` given to it win over visdom's alias.
#[test]
fn a_model_set_outside_visdom_must_take_subscription_calls_too() {
    let server = gateway("per_agent", ALLOWED, SUB_ISSUE);
    let (sb, log) = sandbox(&server);
    for (name, value) in [
        ("ANTHROPIC_MODEL", "claude-haiku"),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", "gpt-general"),
    ] {
        let output = sb
            .visdom(sb.repo.path())
            .arg("init")
            .env(name, value)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        let said = stderr(&output);
        assert!(said.contains(&format!("`{value}`")), "{said}");
        assert!(
            said.contains(&format!("change {name} in the environment")),
            "{said}"
        );
    }
    let output = init(&sb);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let output = sb
        .visdom(sb.repo.path())
        .args(["--", "--model", "gpt-general"])
        .output()
        .unwrap();
    assert!(
        stderr(&output).contains("--model given to Claude Code"),
        "{}",
        stderr(&output)
    );
    assert!(stderr(&output).contains("hooks tracing instead"));
    assert!(lines(&log).contains(&"VISDOM_TRACING_MODE=hooks".to_string()));
}

/// A model named in Claude Code's own settings (here the caller's
/// `--settings`) comes after the environment the launch sets: checked too.
#[test]
fn a_model_in_claude_codes_settings_must_take_subscription_calls_too() {
    let server = gateway("per_agent", ALLOWED, SUB_ISSUE);
    let (sb, log) = sandbox(&server);
    let output = init(&sb);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let output = sb
        .visdom(sb.repo.path())
        .args([
            "--",
            "--settings",
            r#"{"env":{"ANTHROPIC_MODEL":"gpt-general"}}"#,
        ])
        .output()
        .unwrap();
    let said = stderr(&output);
    assert!(said.contains("`gpt-general`"), "{said}");
    assert!(said.contains("--settings given to Claude Code"), "{said}");
    assert!(said.contains("hooks tracing instead"), "{said}");
    assert!(lines(&log).contains(&"VISDOM_TRACING_MODE=hooks".to_string()));
}

/// Managed settings that take the session over stop a plain `visdom` from
/// going through the gateway: hooks under the developer's own choice, not
/// started where the workspace requires the gateway.
#[test]
fn managed_settings_that_take_the_session_over_stop_a_launch() {
    for (policy, managed) in [
        ("per_agent", r#"{"env":{"CLAUDE_CODE_USE_BEDROCK":"1"}}"#),
        ("enforced", r#"{"apiKeyHelper":"get-org-key"}"#),
    ] {
        let server = gateway(policy, ALLOWED, SUB_ISSUE);
        let (sb, log) = sandbox(&server);
        std::fs::write(sb.managed_settings(), managed).unwrap();
        let output = sb.visdom(sb.repo.path()).output().unwrap();
        let said = stderr(&output);
        assert!(said.contains("managed-settings.json"), "{said}");
        if policy == "enforced" {
            assert_eq!(output.status.code(), Some(1), "{said}");
            assert!(said.contains("not started"), "{said}");
            assert!(lines(&log).is_empty(), "the agent must not have run");
        } else {
            assert_eq!(output.status.code(), Some(0), "{said}");
            assert!(said.contains("CLAUDE_CODE_USE_BEDROCK"), "{said}");
            assert!(lines(&log).contains(&"VISDOM_TRACING_MODE=hooks".to_string()));
        }
    }
}

/// A gateway that publishes the setting but not which aliases qualify
/// cannot be relied on to keep the call on Anthropic: not accepted, and no
/// alias is picked for it.
#[test]
fn a_gateway_without_the_eligibility_list_is_not_accepted() {
    let workspace = r#"{"timezone":"UTC","ai_traffic_policy":"per_agent"}"#;
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, workspace),
        ("GET /api/settings", 200, ALLOWED),
        (
            "GET /api/me/keys",
            200,
            r#"{"projects":[{"id":"p-1","slug":"alpha","name":"Alpha"}],"keys":[]}"#,
        ),
        SUB_ISSUE,
        MODELS,
    ]);
    let (sb, _log) = sandbox(&server);
    let output = init(&sb);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("does not say which model aliases"),
        "{}",
        stderr(&output)
    );
    let config = std::fs::read_to_string(sb.user_config()).unwrap();
    assert!(!config.contains("claude_code_model"), "{config}");
    let (severity, _) = finding(&doctor(&sb), "gateway.subscription_model");
    assert_eq!(severity, "error");
}

/// A subscription key the console issued but the LLM gateway refuses with
/// the "denied" code: the same message in init and doctor; a plain `visdom`
/// falls back to hooks, or under `enforced` does not start the agent.
#[test]
fn a_subscription_key_the_gateway_denies_is_the_same_message() {
    const MODELS_DENIED: (&str, u16, &str) = (
        "GET /v1/models",
        403,
        r#"{"error":{"message":"Subscription traffic is not allowed on this gateway","type":"permission_error","code":"subscription_traffic_denied"}}"#,
    );
    for policy in ["per_agent", "enforced"] {
        let workspace = format!(r#"{{"timezone":"UTC","ai_traffic_policy":"{policy}"}}"#);
        let server = MockServer::start(&[
            SUITE,
            ("GET /api/v1/workspace/settings", 200, &workspace),
            ("GET /api/settings", 200, ALLOWED),
            ("GET /api/me/keys", 200, PROJECT),
            SUB_ISSUE,
            MODELS_DENIED,
        ]);
        let (sb, log) = sandbox(&server);
        let output = init(&sb);
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert!(
            has_line(&stderr(&output), NOT_SUPPORTED),
            "{}",
            stderr(&output)
        );

        // The key init got is kept; doctor tries it.
        let (severity, message) = finding(&doctor(&sb), "gateway.subscription_key");
        assert_eq!(severity, "error");
        assert_eq!(message, NOT_SUPPORTED);

        let output = sb.visdom(sb.repo.path()).output().unwrap();
        assert!(
            stderr(&output).contains(NOT_SUPPORTED),
            "{}",
            stderr(&output)
        );
        if policy == "enforced" {
            assert_eq!(output.status.code(), Some(1));
            assert!(lines(&log).is_empty(), "the agent must not have run");
        } else {
            assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
            assert!(lines(&log).contains(&"VISDOM_TRACING_MODE=hooks".to_string()));
        }
    }
}

/// Settings that take the session over: managed ones are an error; a key
/// helper of the developer's own is a warning (set aside by `visdom`).
#[test]
fn settings_that_would_take_the_session_over_are_reported() {
    let server = gateway("per_agent", ALLOWED, SUB_ISSUE);
    let (sb, _log) = sandbox(&server);
    let claude = sb.home.path().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(
        claude.join("settings.json"),
        r#"{"apiKeyHelper":"echo sk-mine"}"#,
    )
    .unwrap();
    let output = init(&sb);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("apiKeyHelper"),
        "{}",
        stderr(&output)
    );
    let (severity, _) = finding(&doctor(&sb), "gateway.claude_settings");
    assert_eq!(severity, "warn");

    std::fs::write(sb.managed_settings(), r#"{"forceLoginMethod":"console"}"#).unwrap();
    let output = init(&sb);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("forceLoginMethod = console"),
        "{}",
        stderr(&output)
    );
    let (severity, message) = finding(&doctor(&sb), "gateway.claude_settings");
    assert_eq!(severity, "error");
    assert!(message.contains("managed-settings.json"), "{message}");
}

/// Paid by the project, none of this is checked: an older gateway is fine.
#[test]
fn a_project_paid_setup_is_not_checked_for_a_subscription() {
    let server = gateway(
        "per_agent",
        OLD,
        (
            "POST /api/me/keys",
            201,
            r#"{"secret":"sk-personal","id":"h","expiresAt":"2099-01-01T00:00:00Z"}"#,
        ),
    );
    let (sb, _log) = sandbox(&server);
    let output = sb
        .visdom(sb.repo.path())
        .args(["--billing", "project", "init"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let report = doctor(&sb);
    let ids: Vec<&str> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["id"].as_str())
        .collect();
    assert!(!ids.contains(&"gateway.subscription"), "{ids:?}");
    assert!(!ids.contains(&"gateway.claude_login"), "{ids:?}");
}
