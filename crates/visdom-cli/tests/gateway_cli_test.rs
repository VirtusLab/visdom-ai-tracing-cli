//! `visdom` starting an agent through the AI Gateway, against local stand-ins
//! for the tracing server, Control Plane and the gateway (one server plays
//! all three) and a script standing in for the agent.
#![cfg(target_os = "linux")]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use common::http::MockServer;
use common::{stderr, stdout, Sandbox};

const PERSON_TOKEN: &str = "person-access-token";
const VARIABLES: [&str; 8] = [
    "VISDOM_GATEWAY_SESSION",
    "CLAUDE_CODE_USE_BEDROCK",
    "VISDOM_TRACING_MODE",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_CUSTOM_HEADERS",
    "CLAUDE_CODE_API_KEY_HELPER_TTL_MS",
];

const SUITE: (&str, u16, &str) = (
    "GET /api/v1/auth/public-config",
    200,
    r#"{"oidc_enabled":true,"control_plane_url":"{URL}","ai_gateway_url":"{URL}"}"#,
);
const SETTINGS: (&str, u16, &str) = (
    "GET /api/settings",
    200,
    r#"{"llmGatewayUrl":"{URL}","mcpGatewayUrl":"{URL}","suite":[],"controlPlaneUrl":"{URL}"}"#,
);
const ONE_PROJECT: (&str, u16, &str) = (
    "GET /api/me/keys",
    200,
    r#"{"hours":24,"projects":[{"id":"p-1","slug":"alpha","name":"Alpha","budget":null}],"keys":[]}"#,
);
const NO_PROJECT: (&str, u16, &str) = (
    "GET /api/me/keys",
    200,
    r#"{"hours":24,"projects":[],"keys":[]}"#,
);
const ISSUE: (&str, u16, &str) = (
    "POST /api/me/keys",
    201,
    r#"{"secret":"sk-personal","id":"h","name":"personal-dev-1","expiresAt":"2099-01-01T00:00:00Z"}"#,
);
const MODELS: (&str, u16, &str) = (
    "GET /v1/models",
    200,
    r#"{"object":"list","data":[{"id":"gpt-general"},{"id":"claude-sonnet"}]}"#,
);

fn policy(name: &str) -> String {
    format!(r#"{{"timezone":"UTC","ai_traffic_policy":"{name}"}}"#)
}

/// A sandbox logged in to `server`, whose Claude Code is a recording script.
fn sandbox(server: &MockServer) -> (Sandbox, std::path::PathBuf) {
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    let (agent, log) = sb.recording_agent(&VARIABLES);
    sb.write_user_config(&format!(
        "[agents]\nclaude_code = \"{}\"\n",
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

fn args(log: &Path) -> Vec<String> {
    lines(log)
        .iter()
        .filter_map(|l| l.strip_prefix("arg:").map(String::from))
        .collect()
}

#[test]
fn a_chosen_gateway_session_gets_a_key_a_model_and_its_ids() {
    let settings = policy("per_agent");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        MODELS,
    ]);
    let (sb, log) = sandbox(&server);
    let output = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway", "--", "--verbose"])
        // Left over from working with another provider.
        .env("CLAUDE_CODE_USE_BEDROCK", "1")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let recorded = lines(&log);
    let has = |line: &str| recorded.iter().any(|l| l == line);
    assert!(has("VISDOM_TRACING_MODE=gateway"), "{recorded:?}");
    assert!(has("VISDOM_GATEWAY_SESSION=claude-code"), "{recorded:?}");
    assert!(has("CLAUDE_CODE_USE_BEDROCK unset"), "{recorded:?}");
    assert!(
        has(&format!("ANTHROPIC_BASE_URL={}", server.url)),
        "{recorded:?}"
    );
    assert!(has("ANTHROPIC_API_KEY unset"), "{recorded:?}");
    assert!(has("ANTHROPIC_MODEL=claude-sonnet"), "{recorded:?}");
    assert!(
        has("CLAUDE_CODE_API_KEY_HELPER_TTL_MS=300000"),
        "{recorded:?}"
    );
    assert!(
        recorded
            .iter()
            .any(|l| l == "ANTHROPIC_CUSTOM_HEADERS=x-tracevault-agent: claude-code"),
        "{recorded:?}"
    );

    let args = args(&log);
    assert_eq!(args[0], "--settings");
    let helper = serde_json::from_str::<serde_json::Value>(&args[1]).unwrap()["apiKeyHelper"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(helper.contains("gateway-key"), "{helper}");
    assert_eq!(args[2], "--session-id");
    assert_eq!(args[4], "--verbose");
    // The key reaches the agent through the helper only.
    assert!(
        !recorded.iter().any(|l| l.contains("sk-personal")),
        "{recorded:?}"
    );

    // Control Plane and the gateway were asked as the person.
    for asked in server.requests() {
        if asked.route != "GET /api/v1/auth/public-config" && asked.route != "GET /v1/models" {
            assert_eq!(
                asked.authorization.as_deref(),
                Some("Bearer person-access-token"),
                "{}",
                asked.route
            );
        }
    }
    assert_eq!(
        server
            .routes()
            .iter()
            .filter(|r| *r == "POST /api/me/keys")
            .count(),
        1
    );

    // The key is kept, for the user alone, and the helper prints it without
    // asking anybody again.
    let store = sb.home.path().join(".config/visdom/gateway-keys.json");
    let mode = std::fs::metadata(&store).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let before = server.routes().len();
    let helper_run = std::process::Command::new("sh")
        .arg("-c")
        .arg(&helper)
        .current_dir(sb.repo.path())
        .env("HOME", sb.home.path())
        .env("XDG_CONFIG_HOME", sb.home.path().join(".config"))
        .output()
        .unwrap();
    assert_eq!(
        stdout(&helper_run),
        "sk-personal\n",
        "{}",
        stderr(&helper_run)
    );
    assert_eq!(server.routes().len(), before, "answered from the kept key");
}

#[test]
fn a_second_session_shares_the_kept_key() {
    let settings = policy("per_agent");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        MODELS,
    ]);
    let (sb, log) = sandbox(&server);
    for _ in 0..2 {
        let output = sb
            .visdom(sb.repo.path())
            .args(["--mode", "gateway"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    }
    let issued = server
        .routes()
        .iter()
        .filter(|r| *r == "POST /api/me/keys")
        .count();
    assert_eq!(issued, 1, "a new key would revoke the first session's");
    let sessions: Vec<String> = args(&log)
        .windows(2)
        .filter(|w| w[0] == "--session-id")
        .map(|w| w[1].clone())
        .collect();
    assert_eq!(sessions.len(), 2);
    assert_ne!(sessions[0], sessions[1], "each session has its own id");
}

#[test]
fn an_enforcing_workspace_puts_every_session_through_the_gateway() {
    let settings = policy("enforced");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        MODELS,
    ]);
    let (sb, log) = sandbox(&server);
    // Nothing chosen, and then hooks chosen: the gateway either way.
    for chosen in [vec![], vec!["--mode", "hooks"]] {
        let output = sb.visdom(sb.repo.path()).args(&chosen).output().unwrap();
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        if !chosen.is_empty() {
            assert!(
                stderr(&output).contains("`enforced`"),
                "{}",
                stderr(&output)
            );
        }
    }
    let modes: Vec<String> = lines(&log)
        .into_iter()
        .filter(|l| l.starts_with("VISDOM_TRACING_MODE"))
        .collect();
    assert_eq!(
        modes,
        ["VISDOM_TRACING_MODE=gateway", "VISDOM_TRACING_MODE=gateway"]
    );
}

#[test]
fn an_enforcing_workspace_does_not_start_an_agent_it_cannot_route() {
    let settings = policy("enforced");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        NO_PROJECT,
    ]);
    let (sb, log) = sandbox(&server);
    let output = sb.visdom(sb.repo.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let said = stderr(&output);
    assert!(said.contains("personal keys on"), "{said}");
    assert!(said.contains("the agent is not started"), "{said}");
    assert!(lines(&log).is_empty(), "the agent must not have run");
}

#[test]
fn a_chosen_gateway_that_is_not_ready_falls_back_to_hooks() {
    let settings = policy("per_agent");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        NO_PROJECT,
    ]);
    let (sb, log) = sandbox(&server);
    let output = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway", "--", "--verbose"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("with hooks tracing instead"),
        "{}",
        stderr(&output)
    );
    let recorded = lines(&log);
    assert!(
        recorded.contains(&"VISDOM_TRACING_MODE=hooks".to_string()),
        "{recorded:?}"
    );
    assert!(
        recorded.contains(&"ANTHROPIC_BASE_URL unset".to_string()),
        "{recorded:?}"
    );
    assert_eq!(args(&log), ["--verbose"]);
}

#[test]
fn a_direct_workspace_keeps_sessions_on_hooks() {
    let settings = policy("direct");
    let server = MockServer::start(&[SUITE, ("GET /api/v1/workspace/settings", 200, &settings)]);
    let (sb, log) = sandbox(&server);
    let output = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway"])
        // What is left in the environment (of a gateway session's shell,
        // say) must not switch the hooks' capture off either.
        .env("VISDOM_TRACING_MODE", "gateway")
        .env("VISDOM_GATEWAY_SESSION", "claude-code")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).contains("`direct`"), "{}", stderr(&output));
    let recorded = lines(&log);
    assert!(
        recorded.contains(&"VISDOM_TRACING_MODE=hooks".to_string()),
        "{recorded:?}"
    );
    assert!(
        recorded.contains(&"VISDOM_GATEWAY_SESSION unset".to_string()),
        "{recorded:?}"
    );
    assert!(
        !server.routes().iter().any(|r| r.contains("/api/me/keys")),
        "the gateway is not asked for anything"
    );
}

#[test]
fn an_api_key_is_never_sent_beyond_the_tracing_server() {
    let settings = policy("per_agent");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
    ]);
    let sb = Sandbox::new();
    let (agent, log) = sb.recording_agent(&VARIABLES);
    sb.write_user_config(&format!(
        "[agents]\nclaude_code = \"{}\"\n",
        agent.display()
    ));
    let output = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway"])
        .env("VISDOM_API_KEY", "tvk_secret")
        .env("VISDOM_SERVER_URL", &server.url)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("Visdom account login"),
        "{}",
        stderr(&output)
    );
    for asked in server.requests() {
        assert_eq!(asked.authorization, None, "{}", asked.route);
    }
    assert!(lines(&log).contains(&"VISDOM_TRACING_MODE=hooks".to_string()));
}

#[test]
fn doctor_reports_the_mode_without_issuing_a_key() {
    let settings = policy("per_agent");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
    ]);
    let (sb, _log) = sandbox(&server);
    let output = sb
        .visdom(sb.repo.path())
        .args(["doctor", "--json", "--mode", "gateway"])
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    let find = |id: &str| {
        json["findings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no {id} in {json}"))
    };
    assert_eq!(find("gateway.mode")["severity"], "ok");
    assert!(find("gateway.project")["message"]
        .as_str()
        .unwrap()
        .contains("`alpha`"));
    assert!(
        !server.routes().iter().any(|r| r == "POST /api/me/keys"),
        "a check issues nothing: a new key would revoke the one in use"
    );
}

fn count(server: &MockServer, route: &str) -> usize {
    server.routes().iter().filter(|r| *r == route).count()
}

fn modes(log: &Path) -> Vec<String> {
    lines(log)
        .into_iter()
        .filter(|l| l.starts_with("VISDOM_TRACING_MODE"))
        .collect()
}

/// The tracing server names Control Plane. While it is away, a workspace
/// that requires the gateway still does: what the server said last holds.
#[test]
fn an_enforcing_workspace_still_enforces_while_the_tracing_server_is_away() {
    let settings = policy("enforced");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/auth/public-config", 503, "{}"),
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        MODELS,
    ]);
    let (sb, log) = sandbox(&server);
    for _ in 0..2 {
        let output = sb.visdom(sb.repo.path()).output().unwrap();
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    }
    assert_eq!(count(&server, "GET /api/v1/auth/public-config"), 2);
    assert_eq!(
        modes(&log),
        ["VISDOM_TRACING_MODE=gateway", "VISDOM_TRACING_MODE=gateway"],
        "the second session must not fall to hooks"
    );
}

#[test]
fn a_chosen_gateway_whose_llm_endpoint_fails_falls_back_to_hooks() {
    let settings = policy("per_agent");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        ("GET /v1/models", 502, "{}"),
    ]);
    let (sb, log) = sandbox(&server);
    let output = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let said = stderr(&output);
    assert!(
        said.contains("cannot list the AI Gateway's models"),
        "{said}"
    );
    assert!(said.contains("with hooks tracing instead"), "{said}");
    assert_eq!(modes(&log), ["VISDOM_TRACING_MODE=hooks"]);
    assert!(lines(&log).contains(&"ANTHROPIC_BASE_URL unset".to_string()));
}

#[test]
fn an_enforcing_workspace_does_not_start_an_agent_whose_llm_endpoint_fails() {
    let settings = policy("enforced");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        ("GET /v1/models", 502, "{}"),
    ]);
    let (sb, log) = sandbox(&server);
    let output = sb.visdom(sb.repo.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let said = stderr(&output);
    assert!(
        said.contains("cannot list the AI Gateway's models"),
        "{said}"
    );
    assert!(said.contains("the agent is not started"), "{said}");
    assert!(lines(&log).is_empty(), "the agent must not have run");
}

/// A key issued elsewhere (the console) revokes the kept one: the gateway
/// refuses it, and the session gets a new key instead of a dead one.
#[test]
fn a_kept_key_the_gateway_refuses_is_replaced() {
    let settings = policy("per_agent");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        (
            "POST /api/me/keys",
            201,
            r#"{"secret":"sk-second","id":"h2","name":"personal-dev-2","expiresAt":"2099-01-01T00:00:00Z"}"#,
        ),
        MODELS,
        ("GET /v1/models", 401, r#"{"error":"revoked"}"#),
        MODELS,
    ]);
    let (sb, log) = sandbox(&server);
    for _ in 0..2 {
        let output = sb
            .visdom(sb.repo.path())
            .args(["--mode", "gateway"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    }
    assert_eq!(count(&server, "POST /api/me/keys"), 2);
    assert_eq!(count(&server, "GET /v1/models"), 3);
    let asked: Vec<Option<String>> = server
        .requests()
        .into_iter()
        .filter(|r| r.route == "GET /v1/models")
        .map(|r| r.authorization)
        .collect();
    assert_eq!(
        asked,
        ["sk-personal", "sk-personal", "sk-second"].map(|k| Some(format!("Bearer {k}")))
    );
    assert_eq!(modes(&log), ["VISDOM_TRACING_MODE=gateway"; 2]);
    let kept =
        std::fs::read_to_string(sb.home.path().join(".config/visdom/gateway-keys.json")).unwrap();
    assert!(
        kept.contains("sk-second") && !kept.contains("sk-personal"),
        "{kept}"
    );
}

fn key_helper(sb: &Sandbox, server: &MockServer) -> std::process::Output {
    sb.visdom(sb.repo.path())
        .args(["gateway-key", "--console", &server.url, "--project", "p-1"])
        .output()
        .unwrap()
}

/// What Claude Code runs during a long session: a key about to expire is
/// renewed with the stored login, kept, and nothing but the key is printed.
#[test]
fn the_key_helper_renews_an_expiring_key() {
    let server = MockServer::start(&[ISSUE]);
    let (sb, _log) = sandbox(&server);
    let store = sb.home.path().join(".config/visdom/gateway-keys.json");
    let soon = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 60;
    std::fs::write(
        &store,
        serde_json::json!([{
            "console_url": server.url, "project_id": "p-1", "account": "",
            "secret": "sk-expiring", "expires_at": soon,
        }])
        .to_string(),
    )
    .unwrap();

    let output = key_helper(&sb, &server);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "sk-personal\n");
    let asked = server.requests();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].route, "POST /api/me/keys");
    assert_eq!(
        asked[0].authorization.as_deref(),
        Some("Bearer person-access-token")
    );
    let kept = std::fs::read_to_string(&store).unwrap();
    assert!(
        kept.contains("sk-personal") && !kept.contains("sk-expiring"),
        "{kept}"
    );
    assert_eq!(
        std::fs::metadata(&store).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // And from then on it is answered from the store.
    assert_eq!(stdout(&key_helper(&sb, &server)), "sk-personal\n");
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn a_key_helper_that_cannot_renew_fails_without_printing_anything() {
    let server = MockServer::start(&[(
        "POST /api/me/keys",
        403,
        r#"{"error":"Alpha does not allow personal keys"}"#,
    )]);
    let (sb, _log) = sandbox(&server);
    let output = key_helper(&sb, &server);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "", "stdout is the key, or nothing");
    let said = stderr(&output);
    assert!(said.contains("does not allow personal keys"), "{said}");
    assert!(!said.contains(PERSON_TOKEN), "{said}");

    // Without a login there is nobody to renew it for.
    let anonymous = Sandbox::new();
    let output = key_helper(&anonymous, &server);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("visdom login"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        server.requests().len(),
        1,
        "nothing was asked without a login"
    );
}

#[test]
fn logging_out_forgets_the_kept_keys() {
    let server = MockServer::start(&[ISSUE]);
    let (sb, _log) = sandbox(&server);
    assert_eq!(stdout(&key_helper(&sb, &server)), "sk-personal\n");
    let store = sb.home.path().join(".config/visdom/gateway-keys.json");
    assert!(store.exists());
    // Whether or not the identity provider can be told.
    let _ = sb.visdom(sb.repo.path()).arg("logout").output().unwrap();
    assert!(!store.exists(), "the keys go with the login");
}

/// Codex takes the caller's `-c` overrides after visdom's: one that names
/// another provider would take the session past a gateway the workspace
/// requires, so nothing is started.
#[test]
fn an_enforcing_workspace_does_not_start_codex_told_to_use_another_provider() {
    let settings = policy("enforced");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        MODELS,
    ]);
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    let (agent, log) = sb.recording_agent(&VARIABLES);
    sb.write_user_config(&format!("[agents]\ncodex = \"{}\"\n", agent.display()));
    let output = sb
        .visdom(sb.repo.path())
        .args(["--agent", "codex", "--", "-c", "model_provider=openai"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let said = stderr(&output);
    assert!(said.contains("`-c model_provider=openai`"), "{said}");
    assert!(said.contains("the agent is not started"), "{said}");
    assert!(lines(&log).is_empty(), "the agent must not have run");

    // Without it, Codex starts on the gateway's provider.
    let output = sb
        .visdom(sb.repo.path())
        .args(["--agent", "codex"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(modes(&log), ["VISDOM_TRACING_MODE=gateway"]);
    assert!(args(&log).contains(&r#"model_provider="visdom-gateway""#.to_string()));
}

const TWO_PROJECTS: (&str, u16, &str) = (
    "GET /api/me/keys",
    200,
    r#"{"hours":24,"projects":[{"id":"p-1","slug":"alpha","name":"Alpha","budget":null},{"id":"p-2","slug":"beta","name":"Beta","budget":null}],"keys":[]}"#,
);

/// Run `script` (the output of `visdom env`) in a shell of the sandbox, then
/// `command` in the same shell.
fn sourced(sb: &Sandbox, script: &str, command: &str) -> std::process::Output {
    // The sandbox's own environment, as `visdom` is given it.
    let shell = sb.visdom(sb.repo.path());
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg(format!("{script}\n{command}"))
        .current_dir(sb.repo.path());
    for (name, value) in shell.get_envs() {
        match value {
            Some(value) => cmd.env(name, value),
            None => cmd.env_remove(name),
        };
    }
    // `visdom` in the script is named by its full path.
    cmd.output().unwrap()
}

/// `visdom env`: what makes a Claude Code started by hand go through the
/// gateway. The key is not in it — only the command that gets it.
#[test]
fn env_sends_a_claude_started_without_visdom_through_the_gateway() {
    let settings = policy("per_agent");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        MODELS,
    ]);
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    let (agent, log) = sb.recording_agent(&[
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_MODEL",
        "ANTHROPIC_CUSTOM_HEADERS",
        "VISDOM_GATEWAY_SESSION",
        "CLAUDE_CODE_USE_BEDROCK",
    ]);
    // Hooks at rest: asking for the variables is asking for the gateway.
    let output = sb.visdom(sb.repo.path()).arg("env").output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let script = stdout(&output);
    assert!(!script.contains("sk-personal"), "{script}");
    assert!(script.contains("gateway-key --console"), "{script}");
    assert!(script.contains("project `alpha`"), "{script}");

    let run = sourced(
        &sb,
        &format!("export ANTHROPIC_API_KEY=sk-mine CLAUDE_CODE_USE_BEDROCK=1\n{script}"),
        &format!("'{}' --verbose", agent.display()),
    );
    assert!(run.status.success(), "{}", stderr(&run));
    let recorded = lines(&log);
    let has = |line: &str| recorded.iter().any(|l| l == line);
    assert!(
        has(&format!("ANTHROPIC_BASE_URL={}", server.url)),
        "{recorded:?}"
    );
    assert!(has("ANTHROPIC_AUTH_TOKEN=sk-personal"), "{recorded:?}");
    assert!(has("ANTHROPIC_API_KEY unset"), "{recorded:?}");
    assert!(has("ANTHROPIC_MODEL=claude-sonnet"), "{recorded:?}");
    assert!(has("VISDOM_GATEWAY_SESSION=claude-code"), "{recorded:?}");
    assert!(has("CLAUDE_CODE_USE_BEDROCK unset"), "{recorded:?}");
    assert!(
        has("ANTHROPIC_CUSTOM_HEADERS=x-tracevault-agent: claude-code"),
        "{recorded:?}"
    );
    assert_eq!(args(&log), ["--verbose"]);
}

/// Codex takes its provider as options, so the script defines a `codex`
/// function that adds them to whatever it is called with.
#[test]
fn env_gives_codex_a_function_that_adds_the_gateway() {
    let settings = policy("enforced");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        MODELS,
    ]);
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    let (agent, log) = sb.recording_agent(&["VISDOM_GATEWAY_API_KEY", "VISDOM_GATEWAY_SESSION"]);
    sb.write_user_config(&format!("[agents]\ncodex = \"{}\"\n", agent.display()));
    let output = sb
        .visdom(sb.repo.path())
        .args(["env", "--agent", "codex"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let script = stdout(&output);
    assert!(!script.contains("sk-personal"), "{script}");

    let run = sourced(&sb, &script, "codex exec 'say hi'");
    assert!(run.status.success(), "{}", stderr(&run));
    let recorded = lines(&log);
    assert!(
        recorded.contains(&"VISDOM_GATEWAY_API_KEY=sk-personal".to_string()),
        "{recorded:?}"
    );
    assert!(
        recorded.contains(&"VISDOM_GATEWAY_SESSION=codex".to_string()),
        "{recorded:?}"
    );
    let args = args(&log);
    assert!(
        args.contains(&"model_provider=\"visdom-gateway\"".to_string()),
        "{args:?}"
    );
    assert!(
        args.iter().any(|a| a
            == &format!(
                "model_providers.visdom-gateway.base_url=\"{}/v1\"",
                server.url
            )),
        "{args:?}"
    );
    assert_eq!(&args[args.len() - 2..], ["exec", "say hi"], "{args:?}");
}

/// Where the workspace keeps sessions off the gateway there is nothing to
/// set, and nothing is printed for a shell to run.
#[test]
fn env_prints_nothing_where_the_workspace_keeps_sessions_on_hooks() {
    let settings = policy("direct");
    let server = MockServer::start(&[SUITE, ("GET /api/v1/workspace/settings", 200, &settings)]);
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    let output = sb.visdom(sb.repo.path()).arg("env").output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("AI traffic policy"),
        "{}",
        stderr(&output)
    );
}

/// A setting left out of the config is taken from the environment: the
/// project among several, and the model alias.
#[test]
fn the_project_and_the_model_can_come_from_the_environment() {
    let settings = policy("per_agent");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        TWO_PROJECTS,
        ISSUE,
        MODELS,
    ]);
    let (sb, log) = sandbox(&server);
    // Several projects and nobody said which: no gateway session.
    let output = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway"])
        .output()
        .unwrap();
    assert!(
        stderr(&output).contains("VISDOM_GATEWAY_PROJECT"),
        "{}",
        stderr(&output)
    );
    assert!(lines(&log).contains(&"VISDOM_GATEWAY_SESSION unset".to_string()));
    std::fs::remove_file(&log).unwrap();

    let output = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway"])
        .env("VISDOM_GATEWAY_PROJECT", "beta")
        .env("VISDOM_GATEWAY_MODEL", "gpt-general")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let recorded = lines(&log);
    assert!(
        recorded.contains(&"ANTHROPIC_MODEL=gpt-general".to_string()),
        "{recorded:?}"
    );
    assert!(
        recorded.contains(&"VISDOM_GATEWAY_SESSION=claude-code".to_string()),
        "{recorded:?}"
    );
}

/// `visdom init` leaves the gateway ready: the mode asked for is kept, the
/// model alias is written down, a key is issued and tried, and what was set
/// up is shown with where each setting comes from.
#[test]
fn init_leaves_the_gateway_ready_and_says_where_its_settings_are() {
    let settings = policy("per_agent");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        MODELS,
    ]);
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    let output = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway", "init"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let said = stdout(&output);
    assert!(said.contains("AI Gateway ready for Claude Code"), "{said}");
    for line in [
        "published by the tracing server",
        "project under [gateway]",
        "claude_code_model under [gateway]",
        "VISDOM_GATEWAY_PROJECT",
        "source <(visdom env)",
    ] {
        assert!(said.contains(line), "{line}: {said}");
    }
    assert!(!said.contains("sk-personal"), "{said}");
    // What was settled is in the config: the next `visdom` needs no option.
    let config = std::fs::read_to_string(sb.user_config()).unwrap();
    assert!(config.contains("tracing_mode = \"gateway\""), "{config}");
    // The only project open today too: one more opened later changes nothing.
    assert!(config.contains("project = \"alpha\""), "{config}");
    assert!(
        config.contains("claude_code_model = \"claude-sonnet\""),
        "{config}"
    );
    assert_eq!(count(&server, "POST /api/me/keys"), 1);
    assert_eq!(count(&server, "GET /v1/models"), 1);

    let (agent, log) = sb.recording_agent(&VARIABLES);
    let mut config = std::fs::read_to_string(sb.user_config()).unwrap();
    config.push_str(&format!(
        "\n[agents]\nclaude_code = \"{}\"\n",
        agent.display()
    ));
    sb.write_user_config(&config);
    let output = sb.visdom(sb.repo.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let recorded = lines(&log);
    assert!(
        recorded.contains(&"VISDOM_GATEWAY_SESSION=claude-code".to_string()),
        "{recorded:?}"
    );
    // The key issued by init is the one used.
    assert_eq!(count(&server, "POST /api/me/keys"), 1);
}

/// A gateway that cannot be made ready fails init, with what to set and
/// where: init does not end "done" on a setup whose first start would fail.
#[test]
fn init_fails_when_the_gateway_cannot_be_made_ready() {
    let settings = policy("enforced");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        TWO_PROJECTS,
    ]);
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    // Nobody to ask which of the two projects.
    let output = sb.visdom(sb.repo.path()).arg("init").output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    let said = stderr(&output);
    assert!(said.contains("the AI Gateway is not ready"), "{said}");
    assert!(said.contains("alpha, beta"), "{said}");
    assert!(said.contains("`project`"), "{said}");
    assert!(said.contains("VISDOM_GATEWAY_PROJECT"), "{said}");
    assert!(!stdout(&output).contains("AI Gateway ready"));

    // Said in the environment, it is set up.
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        TWO_PROJECTS,
        ISSUE,
        MODELS,
    ]);
    sb.log_in(&server.url, PERSON_TOKEN);
    let output = sb
        .visdom(sb.repo.path())
        .arg("init")
        .env("VISDOM_GATEWAY_PROJECT", "beta")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let said = stdout(&output);
    assert!(
        said.contains("VISDOM_GATEWAY_PROJECT in the environment"),
        "{said}"
    );
    assert!(said.contains("required by the workspace"), "{said}");
}

const CUSTOM_MODELS: (&str, u16, &str) = (
    "GET /v1/models",
    200,
    r#"{"object":"list","data":[{"id":"team-assistant"},{"id":"team-fast"}]}"#,
);

/// A gateway whose aliases say nothing about the agent, and nobody to ask
/// which to use: the agent would ask for its own default, which the project
/// does not have. That is not a gateway left ready, nor one to hand out
/// variables for.
#[test]
fn init_and_env_fail_without_a_model_alias_to_ask_for() {
    let settings = policy("per_agent");
    let routes = [
        SUITE,
        ("GET /api/v1/workspace/settings", 200, settings.as_str()),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        CUSTOM_MODELS,
    ];
    let server = MockServer::start(&routes);
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    let output = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway", "init"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    let said = stderr(&output);
    assert!(said.contains("the AI Gateway is not ready"), "{said}");
    assert!(said.contains("team-assistant, team-fast"), "{said}");
    assert!(said.contains("claude_code_model"), "{said}");
    assert!(said.contains("VISDOM_GATEWAY_MODEL"), "{said}");
    assert!(!stdout(&output).contains("AI Gateway ready"));

    let output = sb.visdom(sb.repo.path()).arg("env").output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("team-assistant"),
        "{}",
        stderr(&output)
    );

    // An alias the project does not have is no better than none.
    for command in ["init", "env"] {
        let output = sb
            .visdom(sb.repo.path())
            .args(["--mode", "gateway", command])
            .env("VISDOM_GATEWAY_MODEL", "claude-sonnet")
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "{command}: {}",
            stdout(&output)
        );
        let said = stderr(&output);
        assert!(said.contains("`claude-sonnet`"), "{command}: {said}");
        assert!(
            said.contains("VISDOM_GATEWAY_MODEL in the environment"),
            "{said}"
        );
        assert!(said.contains("team-assistant, team-fast"), "{said}");
        assert!(!stdout(&output).contains("ready"), "{command}");
    }

    // Named, it is ready.
    let output = sb
        .visdom(sb.repo.path())
        .arg("init")
        .env("VISDOM_GATEWAY_MODEL", "team-assistant")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("VISDOM_GATEWAY_MODEL in the environment"),
        "{}",
        stdout(&output)
    );
}

/// What init settles has to be found by the next `visdom`: a config that
/// cannot be written is a failed init, not a summary of a setup that is gone
/// when the command ends.
#[cfg(unix)]
#[test]
fn init_fails_when_what_it_settled_cannot_be_saved() {
    use std::os::unix::fs::PermissionsExt;
    let settings = policy("enforced");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        MODELS,
    ]);
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    sb.write_user_config("agent = \"claude-code\"\n");
    let config = sb.user_config();
    let read_only = std::fs::Permissions::from_mode(0o444);
    std::fs::set_permissions(&config, read_only).unwrap();
    let output = sb.visdom(sb.repo.path()).arg("init").output().unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
    if std::fs::read_to_string(&config)
        .unwrap()
        .contains("[gateway]")
    {
        // Running as root: nothing is read-only.
        return;
    }
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    let said = stderr(&output);
    assert!(said.contains("could not be saved"), "{said}");
    assert!(said.contains("VISDOM_GATEWAY_PROJECT"), "{said}");
    assert!(!stdout(&output).contains("AI Gateway ready"));

    // The mode asked for is such a setting too.
    sb.write_user_config("[gateway]\nproject = \"alpha\"\nclaude_code_model = \"claude-sonnet\"\n");
    let settings = policy("per_agent");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        MODELS,
    ]);
    sb.log_in(&server.url, PERSON_TOKEN);
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o444)).unwrap();
    let output = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway", "init"])
        .output()
        .unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    let said = stderr(&output);
    assert!(said.contains("tracing mode could not be saved"), "{said}");
    assert!(said.contains("VISDOM_TRACING_MODE=gateway"), "{said}");
    assert!(!stdout(&output).contains("AI Gateway ready"));
}

/// A variable that is set wins over the files: the project over the
/// repository's and the user's, the model over the user's.
#[test]
fn the_environment_wins_over_the_config_files() {
    let settings = policy("per_agent");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        TWO_PROJECTS,
        ISSUE,
        MODELS,
    ]);
    let (sb, log) = sandbox(&server);
    let mut config = std::fs::read_to_string(sb.user_config()).unwrap();
    config.push_str("\n[gateway]\nproject = \"alpha\"\nclaude_code_model = \"claude-sonnet\"\n");
    sb.write_user_config(&config);
    std::fs::create_dir_all(sb.repo.path().join(".visdom")).unwrap();
    std::fs::write(
        sb.repo.path().join(".visdom/config.toml"),
        "[gateway]\nproject = \"alpha\"\n",
    )
    .unwrap();
    let output = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway"])
        .env("VISDOM_GATEWAY_PROJECT", "beta")
        .env("VISDOM_GATEWAY_MODEL", "gpt-general")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let recorded = lines(&log);
    assert!(
        recorded.contains(&"ANTHROPIC_MODEL=gpt-general".to_string()),
        "{recorded:?}"
    );
    assert!(
        server
            .requests()
            .iter()
            .any(|r| r.route == "POST /api/me/keys" && r.body.contains("p-2")),
        "{:?}",
        server.routes()
    );
}

/// A model alias the project no longer has does not stop a launch — the
/// gateway refuses the request, and the developer was told why.
#[test]
fn a_launch_warns_about_a_model_alias_the_project_does_not_have() {
    let settings = policy("per_agent");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        MODELS,
    ]);
    let (sb, log) = sandbox(&server);
    let output = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway"])
        .env("VISDOM_GATEWAY_MODEL", "removed-alias")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let said = stderr(&output);
    assert!(said.contains("`removed-alias` is not among"), "{said}");
    assert!(said.contains("gpt-general, claude-sonnet"), "{said}");
    let recorded = lines(&log);
    assert!(
        recorded.contains(&"ANTHROPIC_MODEL=removed-alias".to_string()),
        "{recorded:?}"
    );
}

/// `visdom --mode gateway init` keeps the mode in the user config. Where the
/// repository's config (or the environment) says otherwise, that wins at the
/// next `visdom`: init says so instead of calling the setup done.
#[test]
fn init_fails_when_the_mode_it_saves_would_be_overruled() {
    let settings = policy("per_agent");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        MODELS,
    ]);
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    std::fs::create_dir_all(sb.repo.path().join(".visdom")).unwrap();
    std::fs::write(
        sb.repo.path().join(".visdom/config.toml"),
        "tracing_mode = \"hooks\"\n",
    )
    .unwrap();
    let output = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway", "init"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    let said = stderr(&output);
    assert!(said.contains(".visdom/config.toml"), "{said}");
    assert!(said.contains("hooks"), "{said}");
    assert!(!stdout(&output).contains("AI Gateway ready"));
}

/// `visdom init --global` is the whole setup too, for the machine: bazaar is
/// registered for all of the user's projects, and the gateway is left ready.
#[test]
fn a_machine_wide_init_sets_up_bazaar_and_the_gateway_too() {
    let settings = policy("enforced");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        MODELS,
    ]);
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    let (agent, log) = sb.recording_agent(&[]);
    sb.write_user_config(&format!(
        "bazaar_url = \"https://context.example.com/mcp/remote\"\n\n[agents]\nclaude_code = \"{}\"\n",
        agent.display()
    ));
    // Not in a repository: a machine-wide setup needs none.
    let elsewhere = tempfile::tempdir().unwrap();
    let output = sb
        .visdom(elsewhere.path())
        .args(["init", "--global"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let said = stdout(&output);
    assert!(said.contains("AI Gateway ready for Claude Code"), "{said}");
    let args = args(&log);
    let scope = args.iter().position(|a| a == "--scope").expect("mcp add");
    assert_eq!(args[scope + 1], "user", "{args:?}");
}

/// A folder of checkouts set up as a workspace keeps its gateway project in
/// its own `.visdom/config.toml`, which applies in it and below it although
/// it is no git checkout.
#[test]
fn a_workspace_folder_names_its_own_gateway_project() {
    let settings = policy("per_agent");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        TWO_PROJECTS,
        ISSUE,
        MODELS,
    ]);
    let (sb, log) = sandbox(&server);
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(workspace.path().join(".visdom")).unwrap();
    std::fs::write(
        workspace.path().join(".visdom/config.toml"),
        "tracing_mode = \"gateway\"\n\n[gateway]\nproject = \"beta\"\n",
    )
    .unwrap();
    let below = workspace.path().join("notes");
    std::fs::create_dir_all(&below).unwrap();
    let output = sb.visdom(&below).output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let recorded = lines(&log);
    assert!(
        recorded.contains(&"VISDOM_GATEWAY_SESSION=claude-code".to_string()),
        "{recorded:?} {}",
        stderr(&output)
    );
    // The key is the workspace's project's.
    assert!(
        server
            .requests()
            .iter()
            .any(|r| r.route == "POST /api/me/keys" && r.body.contains("p-2")),
        "{:?}",
        server.routes()
    );
}

/// `visdom init --workspace` binds the folder to its project and then goes
/// on with the rest of the setup, like any other init.
#[test]
fn a_workspace_init_binds_the_folder_and_sets_up_the_gateway() {
    let settings = policy("enforced");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        (
            "GET /api/v1/projects",
            200,
            r#"[{"id":"11111111-2222-3333-4444-555555555555","name":"my-platform"}]"#,
        ),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        MODELS,
    ]);
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    // The developer's own default, from an earlier setup.
    sb.write_user_config("[gateway]\nproject = \"alpha\"\n");
    let folder = tempfile::tempdir().unwrap();
    let output = sb
        .visdom(folder.path())
        .args(["init", "--workspace", "--project", "my-platform"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // The folder keeps its gateway project itself: a later change of the
    // developer's default does not move it.
    let own = std::fs::read_to_string(folder.path().join(".visdom/config.toml")).unwrap();
    assert!(own.contains("project = \"alpha\""), "{own}");
    assert!(
        stdout(&output).contains(".visdom/config.toml"),
        "{}",
        stdout(&output)
    );
    let bound = std::fs::read_to_string(folder.path().join(".visdom/tracing.toml")).unwrap();
    assert!(
        bound.contains("11111111-2222-3333-4444-555555555555"),
        "{bound}"
    );
    let said = stdout(&output);
    assert!(said.contains("AI Gateway ready for Claude Code"), "{said}");
    // Said with the flag, it is `tracevault init --workspace` as it is: the
    // machine-wide hooks are not installed behind the developer's back.
    let hooks =
        std::fs::read_to_string(sb.home.path().join(".claude/settings.json")).unwrap_or_default();
    assert!(!hooks.contains(" stream --event"), "{hooks}");
}

const PROJECTS: (&str, u16, &str) = (
    "GET /api/v1/projects",
    200,
    r#"[{"id":"11111111-2222-3333-4444-555555555555","name":"my-platform"}]"#,
);

/// A workspace may be a checkout's top level: its gateway project is kept in
/// that checkout's `.visdom/config.toml` — the inherited one written down,
/// one already named there (an empty value is none) left as it is.
#[test]
fn a_workspace_at_a_checkouts_top_level_keeps_its_gateway_project_there() {
    let settings = policy("enforced");
    let routes = [
        SUITE,
        ("GET /api/v1/workspace/settings", 200, settings.as_str()),
        PROJECTS,
        SETTINGS,
        TWO_PROJECTS,
        ISSUE,
        MODELS,
    ];
    let init = |sb: &Sandbox| {
        sb.visdom(sb.repo.path())
            .args(["init", "--workspace", "--project", "my-platform"])
            .output()
            .unwrap()
    };
    let own =
        |sb: &Sandbox| std::fs::read_to_string(sb.repo.path().join(".visdom/config.toml")).unwrap();

    for before in [None, Some("[gateway]\nproject = \"  \"\n")] {
        let server = MockServer::start(&routes);
        let sb = Sandbox::new();
        sb.log_in(&server.url, PERSON_TOKEN);
        sb.write_user_config("[gateway]\nproject = \"alpha\"\n");
        if let Some(before) = before {
            std::fs::create_dir_all(sb.repo.path().join(".visdom")).unwrap();
            std::fs::write(sb.repo.path().join(".visdom/config.toml"), before).unwrap();
        }
        let output = init(&sb);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert!(
            own(&sb).contains("project = \"alpha\""),
            "{before:?}: {}",
            own(&sb)
        );
    }

    let server = MockServer::start(&routes);
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    sb.write_user_config("[gateway]\nproject = \"alpha\"\n");
    std::fs::create_dir_all(sb.repo.path().join(".visdom")).unwrap();
    std::fs::write(
        sb.repo.path().join(".visdom/config.toml"),
        "[gateway]\nproject = \"beta\"\n",
    )
    .unwrap();
    let output = init(&sb);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(own(&sb).contains("project = \"beta\""), "{}", own(&sb));
    assert!(!own(&sb).contains("alpha"), "{}", own(&sb));
}

/// A folder's new config comes before one further up. Where that one chose
/// the gateway, the choice is carried into the new file, so that the folder
/// does not fall back to hooks after an init that called the gateway ready.
#[test]
fn a_workspaces_new_config_keeps_the_mode_chosen_further_up() {
    let settings = policy("per_agent");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        PROJECTS,
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        MODELS,
    ]);
    let (sb, log) = sandbox(&server);
    let above = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(above.path().join(".visdom")).unwrap();
    std::fs::write(
        above.path().join(".visdom/config.toml"),
        "tracing_mode = \"gateway\"\n",
    )
    .unwrap();
    let folder = above.path().join("platform");
    std::fs::create_dir_all(&folder).unwrap();
    let output = sb
        .visdom(&folder)
        .args(["init", "--workspace", "--project", "my-platform"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("AI Gateway ready"),
        "{}",
        stdout(&output)
    );
    let own = std::fs::read_to_string(folder.join(".visdom/config.toml")).unwrap();
    assert!(own.contains("tracing_mode = \"gateway\""), "{own}");
    assert!(own.contains("project = \"alpha\""), "{own}");

    let output = sb.visdom(&folder).output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        lines(&log).contains(&"VISDOM_GATEWAY_SESSION=claude-code".to_string()),
        "{:?}",
        lines(&log)
    );
}
