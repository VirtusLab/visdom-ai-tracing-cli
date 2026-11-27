//! `visdom` starting an agent through the AI Gateway, against local stand-ins
//! for the tracing server, Control Plane and the gateway (one server plays
//! all three) and a script standing in for the agent.
#![cfg(target_os = "linux")]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use common::http::MockServer;
use common::{stderr, stdout, Sandbox, FAKE_LOGIN_TOKEN};

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
const SUB_ISSUE: (&str, u16, &str) = (
    "POST /api/me/keys",
    201,
    r#"{"secret":"sk-subscription","id":"sub-hash-1","name":"subscription-dev-1","expiresAt":"2099-01-01T00:00:00Z","billing":"subscription"}"#,
);
const MESSAGES: (&str, u16, &str) = (
    "POST /v1/messages",
    200,
    r#"{"type":"message","content":[{"type":"text","text":"hi"}]}"#,
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

/// A workspace that keeps sessions on hooks, so nothing of the gateway's is
/// needed to start an agent.
fn hooks_workspace() -> MockServer {
    let settings = policy("direct");
    MockServer::start(&[SUITE, ("GET /api/v1/workspace/settings", 200, &settings)])
}

/// The agent's hooks, installed for the whole machine: what binds a session
/// to its repository.
fn install_hooks(sb: &Sandbox, file: &str) {
    let path = sb.home.path().join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"visdom trace session-start"}]}]}}"#,
    )
    .unwrap();
}

const CLAUDE_HOOKS: &str = ".claude/settings.json";
const CODEX_HOOKS: &str = ".codex/hooks.json";

#[test]
fn a_session_with_hooks_is_told_how_to_ask_for_its_policies() {
    let server = hooks_workspace();
    let (sb, log) = sandbox(&server);
    install_hooks(&sb, CLAUDE_HOOKS);
    let output = sb
        .visdom(sb.repo.path())
        .args(["--", "--verbose"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        args(&log),
        ["--append-system-prompt", "# Visdom", "--verbose"]
    );
    let told = std::fs::read_to_string(&log).unwrap();
    assert!(told.contains("visdom trace agent-policies"), "{told}");
    assert!(told.contains("visdom trace repo switch <path>"), "{told}");
    assert!(told.contains("visdom trace check"), "{told}");
    assert!(!told.contains("## AI Gateway"), "{told}");
    // Nothing is registered for this agent.
    assert!(!told.contains("Bazaar"), "{told}");
}

#[test]
fn a_session_through_the_gateway_is_told_to_leave_the_provider_alone() {
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
    install_hooks(&sb, CLAUDE_HOOKS);
    let output = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway", "--", "--verbose"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let args = args(&log);
    // After what routes the session, ahead of the caller's options.
    assert_eq!(args[0], "--settings");
    assert_eq!(args[2], "--session-id");
    assert_eq!(
        args[4..],
        ["--append-system-prompt", "# Visdom", "--verbose"]
    );
    let told = std::fs::read_to_string(&log).unwrap();
    assert!(told.contains("## AI Gateway"), "{told}");
    assert!(told.contains("visdom trace repo switch <path>"), "{told}");
    // A check made by the hooks, which record nothing of this session.
    assert!(!told.contains("visdom trace check"), "{told}");
}

#[test]
fn an_agent_is_told_about_the_bazaar_it_has_under_the_name_it_has_it() {
    const BAZAAR: &str = "https://ctx.example.com/mcp/remote";
    let server = hooks_workspace();
    let (sb, log) = sandbox(&server);
    std::fs::write(
        sb.home.path().join(".claude.json"),
        format!(r#"{{"mcpServers":{{"org-context":{{"type":"http","url":"{BAZAAR}"}}}}}}"#),
    )
    .unwrap();
    let run = |agent_args: &[&str]| {
        let _ = std::fs::remove_file(&log);
        let output = sb
            .visdom(sb.repo.path())
            .env("VISDOM_BAZAAR_URL", BAZAAR)
            .arg("--")
            .args(agent_args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        (args(&log), std::fs::read_to_string(&log).unwrap())
    };

    // The caller's own addition stays, after visdom's.
    let (args, told) = run(&["--append-system-prompt", "Answer in Polish.", "-p", "hi"]);
    assert_eq!(args[0], "--append-system-prompt");
    assert_eq!(args[args.len() - 2..], ["-p", "hi"], "{args:?}");
    assert_eq!(
        args.iter()
            .filter(|a| *a == "--append-system-prompt")
            .count(),
        1,
        "{args:?}"
    );
    assert!(
        told.contains("## Bazaar: the `org-context` MCP server"),
        "{told}"
    );
    // Without the hooks no session is bound: nothing is said of policies.
    assert!(!told.contains("visdom trace"), "{told}");
    let (ours, theirs) = (
        told.find("visdom doctor").unwrap(),
        told.find("Answer in Polish.").unwrap(),
    );
    assert!(ours < theirs, "{told}");

    // Started without its registered servers, it has no bazaar to be told
    // of, and then nothing at all.
    let (args, _) = run(&["--strict-mcp-config", "-p", "hi"]);
    assert_eq!(args, ["--strict-mcp-config", "-p", "hi"]);
}

#[test]
fn an_agent_is_told_nothing_when_that_is_asked_for() {
    let server = hooks_workspace();
    let (sb, log) = sandbox(&server);
    install_hooks(&sb, CLAUDE_HOOKS);
    let output = sb
        .visdom(sb.repo.path())
        .args(["--no-instructions", "--", "--verbose"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(args(&log), ["--verbose"]);

    // A Claude Code that runs no hooks has no bound session to be told of.
    std::fs::remove_file(&log).unwrap();
    let output = sb
        .visdom(sb.repo.path())
        .args(["--", "--bare", "-p", "hi"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(args(&log), ["--bare", "-p", "hi"]);

    std::fs::remove_file(&log).unwrap();
    let config = std::fs::read_to_string(sb.user_config()).unwrap();
    sb.write_user_config(&format!("agent_instructions = false\n{config}"));
    let output = sb
        .visdom(sb.repo.path())
        .args(["--", "--verbose"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(args(&log), ["--verbose"]);
}

#[test]
fn codex_is_told_in_its_developer_instructions_ahead_of_the_developers_own() {
    let server = hooks_workspace();
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    let (agent, log) = sb.recording_agent(&VARIABLES);
    sb.write_user_config(&format!("[agents]\ncodex = \"{}\"\n", agent.display()));
    install_hooks(&sb, CODEX_HOOKS);
    std::fs::write(
        sb.home.path().join(".codex/config.toml"),
        "developer_instructions = \"Keep answers short.\"\n",
    )
    .unwrap();
    let run = || {
        let _ = std::fs::remove_file(&log);
        let output = sb
            .visdom(sb.repo.path())
            .args(["--agent", "codex", "--", "exec", "say hi"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        (args(&log), stderr(&output))
    };

    let (args, said) = run();
    assert_eq!(args[0], "-c");
    assert_eq!(args[2..], ["exec", "say hi"], "{args:?}");
    // One line, a TOML string: read back as Codex reads it.
    let setting: toml::Table = toml::from_str(&args[1]).unwrap();
    let told = setting["developer_instructions"].as_str().unwrap();
    assert!(told.starts_with("# Visdom\n"), "{told}");
    assert!(told.contains("visdom trace agent-policies"), "{told}");
    assert!(told.ends_with("\n\nKeep answers short."), "{told}");
    assert!(!said.contains("developer instructions"), "{said}");

    // A project's own are Codex's to read or not (its trust): they are
    // neither replaced nor made into an option, and the developer is told.
    let project = sb.repo.path().join(".codex/config.toml");
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(&project, "developer_instructions = \"Project rules.\"\n").unwrap();
    let (args, said) = run();
    assert_eq!(args, ["exec", "say hi"]);
    assert!(
        said.contains(".codex/config.toml sets Codex's developer instructions"),
        "{said}"
    );
    assert!(said.contains("visdom instructions"), "{said}");
    assert_eq!(said.matches("developer instructions").count(), 1, "{said}");
}

/// What is added for the agent takes nothing from the routing: Codex goes
/// through the gateway with the instructions, the caller's own kept, and an
/// option that would take it elsewhere still stops the launch.
#[test]
fn codex_through_the_gateway_is_told_and_still_cannot_be_sent_elsewhere() {
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
    install_hooks(&sb, CODEX_HOOKS);

    let output = sb
        .visdom(sb.repo.path())
        .args(["--agent", "codex", "--"])
        .args([
            "-c",
            "developer_instructions=Mine",
            "-c",
            "model_provider=openai",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("`-c model_provider=openai`"),
        "{}",
        stderr(&output)
    );
    assert!(lines(&log).is_empty(), "the agent must not have run");

    let output = sb
        .visdom(sb.repo.path())
        .args(["--agent", "codex", "--"])
        .args(["-c", "developer_instructions=Mine", "exec", "hi"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(modes(&log), ["VISDOM_TRACING_MODE=gateway"]);
    let args = args(&log);
    assert!(args.contains(&r#"model_provider="visdom-gateway""#.to_string()));
    assert_eq!(args[args.len() - 2..], ["exec", "hi"], "{args:?}");
    let told: Vec<&String> = args
        .iter()
        .filter(|a| a.starts_with("developer_instructions="))
        .collect();
    assert_eq!(told.len(), 1, "{args:?}");
    let setting: toml::Table = toml::from_str(told[0]).unwrap();
    let told = setting["developer_instructions"].as_str().unwrap();
    assert!(told.contains("## AI Gateway"), "{told}");
    assert!(told.ends_with("\n\nMine"), "{told}");
}

#[test]
fn instructions_prints_what_an_agent_would_be_told() {
    let server = hooks_workspace();
    let (sb, log) = sandbox(&server);
    let print = || {
        let output = sb
            .visdom(sb.repo.path())
            .arg("instructions")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        stdout(&output)
    };
    // Logged in, but no hook binds a session here: nothing to tell.
    assert_eq!(print(), "");

    install_hooks(&sb, CLAUDE_HOOKS);
    let told = print();
    assert!(told.starts_with("# Visdom\n"), "{told}");
    assert!(told.contains("visdom trace repo switch <path>"), "{told}");
    assert!(lines(&log).is_empty(), "no agent is started");
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

/// Who pays, named further up, is kept in a workspace folder's new config
/// too: otherwise that file would hide it and the project would pay.
#[test]
fn a_workspaces_new_config_keeps_the_billing_chosen_further_up() {
    let settings = policy("per_agent");
    let server = MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        PROJECTS,
        SETTINGS,
        ONE_PROJECT,
        SUB_ISSUE,
        MODELS,
    ]);
    let (sb, log) = sandbox(&server);
    let above = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(above.path().join(".visdom")).unwrap();
    std::fs::write(
        above.path().join(".visdom/config.toml"),
        "tracing_mode = \"gateway\"\n\n[gateway]\nbilling = \"subscription\"\n",
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
    let own = std::fs::read_to_string(folder.join(".visdom/config.toml")).unwrap();
    assert!(own.contains("billing = \"subscription\""), "{own}");

    // Still the subscription: Claude Code is started on it, and only a
    // subscription key was ever asked for.
    let output = sb.visdom(&folder).output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(lines(&log).contains(&"VISDOM_GATEWAY_SESSION=claude-code".to_string()));
    for asked in server.requests() {
        if asked.route == "POST /api/me/keys" {
            assert!(
                asked.body.contains(r#""billing":"subscription""#),
                "{}",
                asked.body
            );
        }
    }
    let args = args(&log);
    assert!(!args[1].starts_with('{'), "settings in a file: {args:?}");
}

fn gateway_server(policy_name: &str) -> MockServer {
    let settings = policy(policy_name);
    MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        ISSUE,
        MODELS,
    ])
}

/// A gateway whose console issues a subscription key first, then an
/// ordinary one, and whose LLM endpoint answers messages.
fn subscription_server(policy_name: &str) -> MockServer {
    let settings = policy(policy_name);
    MockServer::start(&[
        SUITE,
        ("GET /api/v1/workspace/settings", 200, &settings),
        SETTINGS,
        ONE_PROJECT,
        SUB_ISSUE,
        ISSUE,
        MODELS,
        MESSAGES,
    ])
}

fn finding(output: &std::process::Output, id: &str) -> serde_json::Value {
    let json: serde_json::Value =
        serde_json::from_str(&stdout(output)).unwrap_or_else(|e| panic!("{e}: {}", stdout(output)));
    json["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("no {id} in {json}"))
}

/// `visdom --mode gateway --billing subscription init` saves who pays without
/// asking, gets a subscription key and shows it with its source; the next
/// `visdom` starts Claude Code on it, `--billing project` on the project.
#[test]
fn init_saves_the_billing_given_on_the_command_line() {
    let server = subscription_server("per_agent");
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    let output = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway", "--billing", "subscription", "init"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let said = stdout(&output);
    assert!(said.contains("AI Gateway ready for Claude Code"), "{said}");
    assert!(said.contains("my Claude subscription"), "{said}");
    assert!(said.contains("(--billing)"), "{said}");
    assert!(said.contains("a subscription key"), "{said}");
    assert!(!said.to_lowercase().contains("terms"), "{said}");
    let config = std::fs::read_to_string(sb.user_config()).unwrap();
    assert!(config.contains("billing = \"subscription\""), "{config}");
    assert!(config.contains("tracing_mode = \"gateway\""), "{config}");

    let (agent, log) = sb.recording_agent(&VARIABLES);
    let mut config = config;
    config.push_str(&format!(
        "\n[agents]\nclaude_code = \"{}\"\n",
        agent.display()
    ));
    sb.write_user_config(&config);
    let output = sb.visdom(sb.repo.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let recorded = lines(&log);
    assert!(recorded.contains(&"VISDOM_GATEWAY_SESSION=claude-code".to_string()));
    assert!(
        recorded.contains(&"CLAUDE_CODE_API_KEY_HELPER_TTL_MS unset".to_string()),
        "no key helper: {recorded:?}"
    );
    assert_eq!(count(&server, "POST /api/me/keys"), 1, "init's key is kept");
    // `--billing project` for one run starts it, paid by the project.
    let output = sb
        .visdom(sb.repo.path())
        .args(["--billing", "project"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(lines(&log).contains(&"VISDOM_GATEWAY_SESSION=claude-code".to_string()));
}

/// A billing saved earlier is used by `init` without asking, and shown with
/// the file it comes from; `doctor` reports it too.
#[test]
fn init_and_doctor_show_a_saved_billing_and_where_it_is() {
    let server = gateway_server("enforced");
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    sb.write_user_config("[gateway]\nbilling = \"project\"\n");
    let output = sb.visdom(sb.repo.path()).arg("init").output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let said = stdout(&output);
    assert!(said.contains("project (gateway key)"), "{said}");
    assert!(said.contains("billing under [gateway] in"), "{said}");
    assert!(!said.contains("not available"), "{said}");

    // `enforced` is satisfied by a subscription too: reported, not refused.
    sb.write_user_config("[gateway]\nbilling = \"subscription\"\n");
    let output = sb
        .visdom(sb.repo.path())
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    let billing = finding(&output, "gateway.billing");
    assert_eq!(billing["severity"], "ok", "{billing}");
    let message = billing["message"].as_str().unwrap();
    assert!(message.contains("billing under [gateway] in"), "{message}");
    assert!(
        message.contains("your Claude subscription pays"),
        "{message}"
    );
    // The environment comes before the file.
    let output = sb
        .visdom(sb.repo.path())
        .args(["doctor", "--json"])
        .env("VISDOM_GATEWAY_BILLING", "project")
        .output()
        .unwrap();
    let billing = finding(&output, "gateway.billing");
    assert_eq!(billing["severity"], "ok", "{billing}");
    assert!(billing["message"]
        .as_str()
        .unwrap()
        .contains("VISDOM_GATEWAY_BILLING in the environment"));
    // A blank variable is not set; one that names nothing is an error.
    let output = sb
        .visdom(sb.repo.path())
        .args(["doctor", "--json"])
        .env("VISDOM_GATEWAY_BILLING", " ")
        .output()
        .unwrap();
    let billing = finding(&output, "gateway.billing");
    let message = billing["message"].as_str().unwrap();
    assert!(
        message.starts_with("billing = subscription (billing under [gateway] in"),
        "the file's, not the blank variable's: {message}"
    );
    let output = sb
        .visdom(sb.repo.path())
        .args(["doctor", "--json"])
        .env("VISDOM_GATEWAY_BILLING", "free")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("VISDOM_GATEWAY_BILLING"),
        "{}",
        stderr(&output)
    );
}

/// A subscription outside the gateway, or for Codex, is an error that says
/// why, and nothing is started or saved.
#[test]
fn a_subscription_where_it_cannot_be_used_is_refused() {
    let server = gateway_server("per_agent");
    let (sb, log) = sandbox(&server);
    let output = sb
        .visdom(sb.repo.path())
        .args(["--mode", "hooks", "--billing", "subscription"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let said = stderr(&output);
    assert!(said.contains("hooks tracing has no gateway key"), "{said}");
    assert!(lines(&log).is_empty(), "the agent must not have run");

    let output = sb
        .visdom(sb.repo.path())
        .args([
            "--agent",
            "codex",
            "--mode",
            "gateway",
            "--billing",
            "subscription",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("not supported yet for Codex"),
        "{}",
        stderr(&output)
    );

    let output = sb
        .visdom(sb.repo.path())
        .args(["--mode", "hooks", "--billing", "subscription", "init"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let config = std::fs::read_to_string(sb.user_config()).unwrap();
    assert!(!config.contains("billing"), "{config}");

    let output = sb
        .visdom(sb.repo.path())
        .args([
            "doctor",
            "--json",
            "--mode",
            "hooks",
            "--billing",
            "subscription",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(finding(&output, "gateway.billing")["severity"], "error");
}

/// Where the repository's config names who pays, a different `--billing`
/// saved by init would not be used by a plain `visdom`: init says so.
#[test]
fn init_fails_when_the_billing_it_saves_would_be_overruled() {
    let server = gateway_server("per_agent");
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    std::fs::create_dir_all(sb.repo.path().join(".visdom")).unwrap();
    std::fs::write(
        sb.repo.path().join(".visdom/config.toml"),
        "[gateway]\nbilling = \"project\"\n",
    )
    .unwrap();
    let output = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway", "--billing", "subscription", "init"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let said = stderr(&output);
    assert!(said.contains(".visdom/config.toml"), "{said}");
    assert!(!stdout(&output).contains("AI Gateway ready"));
}

/// `visdom env` with the subscription: no bearer token; the subscription key
/// goes into the custom headers through `visdom gateway-key --billing
/// subscription`, so the script holds no secret. A Claude Code started from
/// that shell keeps its login. Codex is refused outright.
#[test]
fn env_sends_a_claude_on_its_subscription_through_the_gateway() {
    let server = subscription_server("per_agent");
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    let output = sb
        .visdom(sb.repo.path())
        .args(["--billing", "subscription", "env"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let script = stdout(&output);
    assert!(!script.contains("sk-subscription"), "{script}");
    assert!(!script.contains("ANTHROPIC_AUTH_TOKEN="), "{script}");
    assert!(script.contains("--billing subscription"), "{script}");
    let (agent, log) = sb.fake_claude_code();
    let run = sourced(
        &sb,
        &format!("export ANTHROPIC_API_KEY=sk-mine ANTHROPIC_AUTH_TOKEN=tok-mine\n{script}"),
        &format!("'{}' -p hi", agent.display()),
    );
    assert!(run.status.success(), "{}", stderr(&run));
    assert_eq!(fake_runs(&log)[0]["status"], 200);
    let sent = last_message(&server);
    assert_eq!(
        sent.authorization.as_deref(),
        Some(format!("Bearer {FAKE_LOGIN_TOKEN}").as_str())
    );
    assert_eq!(sent.header("x-litellm-api-key"), ["Bearer sk-subscription"]);
    assert_eq!(count(&server, "POST /api/me/keys"), 1, "the kept key");
    let output = sb
        .visdom(sb.repo.path())
        .args(["env", "--agent", "codex"])
        .env("VISDOM_GATEWAY_BILLING", "subscription")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("not supported yet for Codex"),
        "{}",
        stderr(&output)
    );
}

/// Codex is not asked who pays, but `--billing project` given to its init is
/// kept: a subscription saved earlier (for Claude Code) would otherwise stop
/// every Codex launch.
#[test]
fn codex_init_keeps_an_explicit_project_billing() {
    let server = gateway_server("per_agent");
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    sb.write_user_config("[gateway]\nbilling = \"subscription\"\n");
    let output = sb
        .visdom(sb.repo.path())
        .args([
            "--mode",
            "gateway",
            "--billing",
            "project",
            "init",
            "--agent",
            "codex",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let config = std::fs::read_to_string(sb.user_config()).unwrap();
    assert!(config.contains("billing = \"project\""), "{config}");
    assert!(stdout(&output).contains("AI Gateway ready for Codex"));
}

/// The runs [`Sandbox::fake_claude_code`] logged, one JSON object each.
fn fake_runs(log: &Path) -> Vec<serde_json::Value> {
    lines(log)
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// The last `POST /v1/messages` the gateway got.
fn last_message(server: &MockServer) -> common::http::Recorded {
    server
        .requests()
        .into_iter()
        .rfind(|r| r.route == "POST /v1/messages")
        .expect("a message was sent")
}

/// A sandbox logged in to `server` whose Claude Code is the stand-in that
/// sends one request, and whose developer has a key helper and a key of
/// their own in Claude Code's user settings.
fn subscription_sandbox(server: &MockServer) -> (Sandbox, std::path::PathBuf) {
    let sb = Sandbox::new();
    sb.log_in(&server.url, PERSON_TOKEN);
    let (agent, log) = sb.fake_claude_code();
    sb.write_user_config(&format!(
        "[agents]\nclaude_code = \"{}\"\n",
        agent.display()
    ));
    let claude = sb.home.path().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(
        claude.join("settings.json"),
        r#"{"apiKeyHelper":"echo sk-dev-helper","env":{"ANTHROPIC_API_KEY":"sk-dev-settings"}}"#,
    )
    .unwrap();
    (sb, log)
}

/// Where a launch writes its private settings in the sandbox.
fn private_dir(sb: &Sandbox) -> std::path::PathBuf {
    sb.home.path().join(".local/state/visdom")
}

fn settings_files(sb: &Sandbox) -> Vec<String> {
    std::fs::read_dir(private_dir(sb))
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("claude-settings-"))
                .collect()
        })
        .unwrap_or_default()
}

/// `billing = subscription`: Claude Code keeps its claude.ai login, so the
/// request carries the subscription token as it is, and the subscription key
/// in `x-litellm-api-key`. Nothing of the developer's own — a key helper or a
/// key in their Claude settings, keys in the environment, a stale gateway
/// header — takes over. The key is on no command line: the settings are in a
/// file only the user can read, removed when the agent exits.
#[test]
fn a_subscription_session_keeps_the_login_and_sends_the_key_in_a_header() {
    let server = subscription_server("per_agent");
    let (sb, log) = subscription_sandbox(&server);
    let output = sb
        .visdom(sb.repo.path())
        .args([
            "--mode",
            "gateway",
            "--billing",
            "subscription",
            "--",
            "-p",
            "hi",
        ])
        .env("ANTHROPIC_API_KEY", "sk-dev-env")
        .env("ANTHROPIC_AUTH_TOKEN", "tok-dev-env")
        .env(
            "ANTHROPIC_CUSTOM_HEADERS",
            "x-litellm-api-key: Bearer sk-stale\nx-team: blue",
        )
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let sent = last_message(&server);
    assert_eq!(
        sent.authorization.as_deref(),
        Some(format!("Bearer {FAKE_LOGIN_TOKEN}").as_str()),
        "the login's token, untouched"
    );
    assert_eq!(sent.header("x-litellm-api-key"), ["Bearer sk-subscription"]);
    assert!(sent.header("x-api-key").is_empty(), "{:?}", sent.headers);
    assert_eq!(sent.header("x-tracevault-agent"), ["claude-code"]);
    assert_eq!(sent.header("x-team"), ["blue"]);
    assert_eq!(sent.header("x-tracevault-session").len(), 1);

    let run = &fake_runs(&log)[0];
    assert_eq!(run["status"], 200);
    assert_eq!(run["settings"]["apiKeyHelper"], "", "no key helper");
    assert_eq!(run["settings_mode"], "0o600");
    assert_eq!(run["settings_left"], true, "there while the agent ran");
    let path = run["settings_path"].as_str().unwrap();
    assert!(
        path.starts_with(&private_dir(&sb).to_string_lossy().into_owned()),
        "{path}"
    );
    let args: Vec<String> = serde_json::from_value(run["args"].clone()).unwrap();
    assert!(
        !args.iter().any(|a| a.contains("sk-subscription")),
        "{args:?}"
    );
    assert_eq!(args[args.len() - 2..], ["-p", "hi"]);
    assert!(settings_files(&sb).is_empty(), "removed when it exited");

    // Asked for as a subscription key, and checked on the LLM gateway with
    // it as the bearer token.
    let issued: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.route == "POST /api/me/keys")
        .collect();
    assert_eq!(issued.len(), 1);
    assert_eq!(
        issued[0].body,
        r#"{"projectId":"p-1","billing":"subscription"}"#
    );
    let models = server
        .requests()
        .into_iter()
        .find(|r| r.route == "GET /v1/models")
        .unwrap();
    assert_eq!(
        models.authorization.as_deref(),
        Some("Bearer sk-subscription")
    );

    // The same stand-in paid by the project: the key helper's key is sent
    // instead of the login — so the one above is the subscription's doing.
    let output = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway", "--billing", "project"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let sent = last_message(&server);
    assert_eq!(sent.header("x-api-key"), ["sk-personal"]);
    assert!(sent.header("x-litellm-api-key").is_empty());
    assert_eq!(sent.authorization, None);
}

/// A second `visdom` started while the first runs gets the kept subscription
/// key and its own settings file; the first one's key is not revoked and its
/// file is not touched, so it carries on.
#[test]
fn a_second_subscription_session_does_not_break_the_first() {
    let server = subscription_server("per_agent");
    let (sb, log) = subscription_sandbox(&server);
    let go = sb.home.path().join("go");
    let first = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway", "--billing", "subscription"])
        .env("FAKE_CLAUDE_WAIT_FOR", &go)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    while settings_files(&sb).is_empty() {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "the first session did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let second = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway", "--billing", "subscription"])
        .output()
        .unwrap();
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    assert_eq!(
        settings_files(&sb).len(),
        1,
        "the first one's is still there"
    );
    std::fs::write(&go, "").unwrap();
    let first = first.wait_with_output().unwrap();
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));

    let runs = fake_runs(&log);
    assert_eq!(runs.len(), 2);
    for run in &runs {
        assert_eq!(run["status"], 200, "{run}");
        assert_eq!(run["settings_left"], true, "{run}");
    }
    assert_ne!(runs[0]["settings_path"], runs[1]["settings_path"]);
    let messages: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.route == "POST /v1/messages")
        .collect();
    assert_eq!(messages.len(), 2);
    for sent in &messages {
        assert_eq!(sent.header("x-litellm-api-key"), ["Bearer sk-subscription"]);
    }
    assert_eq!(count(&server, "POST /api/me/keys"), 1, "one key, shared");
    assert!(
        !server.routes().iter().any(|r| r.starts_with("DELETE")),
        "nothing revoked"
    );
    assert!(settings_files(&sb).is_empty());
}

/// `visdom gateway-key --billing subscription` hands out the kept
/// subscription key until a week before it expires, then a new one — and
/// the old one is not revoked, and stays kept for logout to revoke.
#[test]
fn the_subscription_key_is_renewed_a_week_before_it_expires() {
    let server = MockServer::start(&[SUB_ISSUE]);
    let (sb, _log) = sandbox(&server);
    let store = sb.home.path().join(".config/visdom/gateway-keys.json");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let kept = |days: u64| {
        serde_json::json!([{
            "console_url": server.url, "project_id": "p-1", "account": "",
            "secret": "sk-sub-old", "expires_at": now + days * 86_400,
            "billing": "subscription", "id": "old-id",
        }])
        .to_string()
    };
    let helper = || {
        sb.visdom(sb.repo.path())
            .args([
                "gateway-key",
                "--console",
                &server.url,
                "--project",
                "p-1",
                "--billing",
                "subscription",
            ])
            .output()
            .unwrap()
    };
    std::fs::write(&store, kept(8)).unwrap();
    assert_eq!(stdout(&helper()), "sk-sub-old\n");
    assert!(server.requests().is_empty());

    std::fs::write(&store, kept(6)).unwrap();
    let output = helper();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "sk-subscription\n");
    let asked = server.requests();
    assert_eq!(asked.len(), 1);
    assert_eq!(
        asked[0].body,
        r#"{"projectId":"p-1","billing":"subscription"}"#
    );
    let kept = std::fs::read_to_string(&store).unwrap();
    assert!(
        kept.contains("sk-sub-old") && kept.contains("sk-subscription"),
        "{kept}"
    );
    // An ordinary key is another kind: the subscription key is not it.
    let plain = MockServer::start(&[ISSUE]);
    let output = sb
        .visdom(sb.repo.path())
        .args(["gateway-key", "--console", &plain.url, "--project", "p-1"])
        .output()
        .unwrap();
    assert_eq!(stdout(&output), "sk-personal\n");
}

/// `visdom logout` revokes the kept subscription key at the console (as the
/// person) before it forgets it.
#[test]
fn logging_out_revokes_the_kept_subscription_key() {
    let server = MockServer::start(&[SUB_ISSUE, ("DELETE /api/me/keys/sub-hash-1", 204, "")]);
    let (sb, _log) = sandbox(&server);
    let output = sb
        .visdom(sb.repo.path())
        .args([
            "gateway-key",
            "--console",
            &server.url,
            "--project",
            "p-1",
            "--billing",
            "subscription",
        ])
        .output()
        .unwrap();
    assert_eq!(stdout(&output), "sk-subscription\n", "{}", stderr(&output));
    let store = sb.home.path().join(".config/visdom/gateway-keys.json");
    assert!(store.exists());
    let output = sb.visdom(sb.repo.path()).arg("logout").output().unwrap();
    let revoked: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.route.starts_with("DELETE"))
        .collect();
    assert_eq!(revoked.len(), 1, "{}", stderr(&output));
    assert_eq!(revoked[0].route, "DELETE /api/me/keys/sub-hash-1");
    assert_eq!(
        revoked[0].authorization.as_deref(),
        Some("Bearer person-access-token")
    );
    assert!(!store.exists(), "the keys go with the login");
    assert!(
        !stderr(&output).contains("could not be revoked"),
        "{}",
        stderr(&output)
    );
}

/// Send `signal` (`-INT`, `-TERM`) to process `pid`.
fn signal(pid: u32, signal: &str) {
    let status = std::process::Command::new("kill")
        .args([signal, &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Start a subscription session whose agent waits for `go`, and return once
/// the agent runs (it says so in `<go>.started`).
fn waiting_session(sb: &Sandbox, go: &Path) -> std::process::Child {
    let mut marker = go.as_os_str().to_owned();
    marker.push(".started");
    let marker = std::path::PathBuf::from(marker);
    let _ = std::fs::remove_file(&marker);
    let child = sb
        .visdom(sb.repo.path())
        .args(["--mode", "gateway", "--billing", "subscription"])
        .env("FAKE_CLAUDE_WAIT_FOR", go)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    while !marker.exists() {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "the session did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    child
}

/// While the agent runs as its child, `visdom` waits out an interrupt (the
/// terminal sends it to the agent too, which decides), passes a termination
/// on to the agent, and removes the settings file however the agent ends.
#[test]
fn a_subscription_session_waits_out_an_interrupt_and_passes_a_termination_on() {
    let server = subscription_server("per_agent");
    let (sb, log) = subscription_sandbox(&server);
    let go = sb.home.path().join("go");

    // An interrupt to visdom alone: it keeps waiting, the agent finishes.
    let mut session = waiting_session(&sb, &go);
    signal(session.id(), "-INT");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(session.try_wait().unwrap().is_none(), "visdom still waits");
    std::fs::write(&go, "").unwrap();
    assert_eq!(exited(&mut session).code(), Some(0));
    assert_eq!(fake_runs(&log).len(), 1);
    assert!(settings_files(&sb).is_empty());

    // A termination: the agent gets it and ends by it, and visdom reports it
    // as a shell would.
    std::fs::remove_file(&go).unwrap();
    let mut session = waiting_session(&sb, &go);
    signal(session.id(), "-TERM");
    let status = exited(&mut session);
    assert_eq!(status.code(), Some(128 + 15), "{status:?}");
    assert_eq!(fake_runs(&log).len(), 1, "the agent did not get to send");
    assert!(
        settings_files(&sb).is_empty(),
        "removed after the agent ended"
    );
}

/// `child`'s exit status, within 20 seconds: a session that does not end is
/// killed and fails the test instead of hanging it.
fn exited(child: &mut std::process::Child) -> std::process::ExitStatus {
    let started = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if started.elapsed() > std::time::Duration::from_secs(20) {
            let _ = child.kill();
            panic!("the session did not end");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
