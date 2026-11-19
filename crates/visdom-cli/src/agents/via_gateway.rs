//! Starting an agent so that its LLM traffic goes through the AI Gateway.
//!
//! Everything is given to the launched process alone — environment variables
//! and launch options — never written into the agent's configuration files:
//! an agent started without `visdom` is exactly as it was.

use std::process::Command;

use tracevault_cli::agent::Agent;

use crate::mode::{TracingMode, MODE_ENV};

/// The environment variable Codex reads the gateway key from.
pub const CODEX_KEY_ENV: &str = "VISDOM_GATEWAY_API_KEY";

/// The name of the model provider `visdom` defines for Codex.
const CODEX_PROVIDER: &str = "visdom-gateway";

/// How long Claude Code keeps a key before asking the helper again. The
/// helper answers from visdom's key store, so asking often is cheap, and a
/// key replaced by another session is picked up within this time.
const CLAUDE_KEY_TTL_MS: &str = "300000";

/// What an agent needs to run through the gateway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatewayLaunch {
    /// The LLM gateway's address, without `/v1`.
    pub llm_url: String,
    /// The developer's personal key.
    pub key: String,
    /// A shell command that prints the current key (`visdom gateway-key …`).
    pub key_helper: String,
    /// The model alias to ask for; `None` leaves the agent's own choice.
    pub model: Option<String>,
    /// The repository's id in AI Tracing, for attribution.
    pub repo_id: Option<String>,
    /// The id the session gets, so the gateway's records join one session.
    pub session_id: String,
}

/// Whether `args` already carry one of `options` (as `--name value` or
/// `--name=value`).
fn has_option(args: &[String], options: &[&str]) -> bool {
    args.iter().any(|arg| {
        options.iter().any(|o| {
            arg == o
                || arg
                    .strip_prefix(o)
                    .is_some_and(|rest| rest.starts_with('='))
        })
    })
}

/// `value` as one header value: no line breaks, which would start another
/// header.
fn header_value(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}

/// Claude Code through the gateway. `env` reads the current environment.
///
/// - The key comes from `apiKeyHelper` (given with `--settings`), so a key
///   renewed during a long session is picked up; when the caller passes their
///   own `--settings`, the key is given once, as `ANTHROPIC_AUTH_TOKEN`.
/// - The session id is fixed up front and sent with every request, unless the
///   caller resumes or names a session themselves.
pub fn claude_command(
    program: &str,
    args: &[String],
    gateway: &GatewayLaunch,
    env: &dyn Fn(&str) -> Option<String>,
) -> Command {
    let mut cmd = Command::new(program);
    cmd.env(MODE_ENV, TracingMode::Gateway.name());
    cmd.env("ANTHROPIC_BASE_URL", &gateway.llm_url);
    // A key of the developer's own would bypass the gateway's.
    cmd.env_remove("ANTHROPIC_API_KEY");
    if has_option(args, &["--settings"]) {
        cmd.env("ANTHROPIC_AUTH_TOKEN", &gateway.key);
    } else {
        cmd.env_remove("ANTHROPIC_AUTH_TOKEN");
        cmd.env("CLAUDE_CODE_API_KEY_HELPER_TTL_MS", CLAUDE_KEY_TTL_MS);
        let settings = serde_json::json!({ "apiKeyHelper": gateway.key_helper });
        cmd.arg("--settings").arg(settings.to_string());
    }
    if let Some(model) = &gateway.model {
        // The project's allow-list has aliases, not Claude Code's defaults:
        // both the main and the small model must name one. A model the
        // developer already chose stays.
        for name in ["ANTHROPIC_MODEL", "ANTHROPIC_DEFAULT_HAIKU_MODEL"] {
            if env(name).is_none() {
                cmd.env(name, model);
            }
        }
    }

    let own_session = has_option(
        args,
        &[
            "--session-id",
            "--resume",
            "-r",
            "--continue",
            "-c",
            "--from-pr",
            "--teleport",
        ],
    );
    let mut headers: Vec<String> = env("ANTHROPIC_CUSTOM_HEADERS")
        .filter(|h| !h.trim().is_empty())
        .into_iter()
        .collect();
    headers.push(format!(
        "x-tracevault-agent: {}",
        Agent::ClaudeCode.tool_name()
    ));
    if let Some(repo) = &gateway.repo_id {
        headers.push(format!("x-tracevault-repo: {}", header_value(repo)));
    }
    if !own_session {
        headers.push(format!(
            "x-tracevault-session: {}",
            header_value(&gateway.session_id)
        ));
        cmd.arg("--session-id").arg(&gateway.session_id);
    }
    cmd.env("ANTHROPIC_CUSTOM_HEADERS", headers.join("\n"));
    cmd.args(args);
    cmd
}

/// A TOML basic string.
fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_string()).to_string()
}

/// Codex through the gateway: a model provider defined for this run with
/// `-c`, its key in the environment. Codex names the session itself
/// (`thread-id`), which the gateway takes as the session. The key is fixed
/// for the life of the process.
pub fn codex_command(program: &str, args: &[String], gateway: &GatewayLaunch) -> Command {
    let mut cmd = Command::new(program);
    cmd.env(MODE_ENV, TracingMode::Gateway.name());
    cmd.env(CODEX_KEY_ENV, &gateway.key);
    let provider = format!("model_providers.{CODEX_PROVIDER}");
    let mut headers = vec![format!(
        "\"x-tracevault-agent\" = {}",
        toml_string(Agent::Codex.tool_name())
    )];
    if let Some(repo) = &gateway.repo_id {
        headers.push(format!(
            "\"x-tracevault-repo\" = {}",
            toml_string(&header_value(repo))
        ));
    }
    let settings = [
        format!("model_provider={}", toml_string(CODEX_PROVIDER)),
        format!("{provider}.name={}", toml_string("Visdom AI Gateway")),
        format!(
            "{provider}.base_url={}",
            toml_string(&format!("{}/v1", gateway.llm_url.trim_end_matches('/')))
        ),
        format!("{provider}.env_key={}", toml_string(CODEX_KEY_ENV)),
        format!("{provider}.wire_api={}", toml_string("responses")),
        format!("{provider}.http_headers={{ {} }}", headers.join(", ")),
    ];
    for setting in settings {
        cmd.arg("-c").arg(setting);
    }
    if let Some(model) = &gateway.model {
        if !has_option(args, &["-m", "--model"]) {
            cmd.arg("--model").arg(model);
        }
    }
    cmd.args(args);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsStr;

    fn gateway() -> GatewayLaunch {
        GatewayLaunch {
            llm_url: "https://llm.example.com".to_string(),
            key: "sk-abc".to_string(),
            key_helper: "'/usr/bin/visdom' gateway-key --console 'https://gw' --project 'p-1'"
                .to_string(),
            model: Some("claude-sonnet".to_string()),
            repo_id: Some("11111111-2222-3333-4444-555555555555".to_string()),
            session_id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string(),
        }
    }

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// The variables a command sets (`Some`) or removes (`None`).
    fn envs_of(cmd: &Command) -> HashMap<String, Option<String>> {
        cmd.get_envs()
            .map(|(k, v): (&OsStr, Option<&OsStr>)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn claude_gets_the_gateway_the_key_helper_and_a_session() {
        let cmd = claude_command("claude", &strings(&["--verbose"]), &gateway(), &no_env);
        let env = envs_of(&cmd);
        assert_eq!(env["VISDOM_TRACING_MODE"].as_deref(), Some("gateway"));
        assert_eq!(
            env["ANTHROPIC_BASE_URL"].as_deref(),
            Some("https://llm.example.com")
        );
        assert_eq!(env["ANTHROPIC_API_KEY"], None, "removed");
        assert_eq!(
            env["ANTHROPIC_AUTH_TOKEN"], None,
            "removed: the helper gives the key"
        );
        assert_eq!(env["ANTHROPIC_MODEL"].as_deref(), Some("claude-sonnet"));
        assert_eq!(
            env["ANTHROPIC_DEFAULT_HAIKU_MODEL"].as_deref(),
            Some("claude-sonnet")
        );
        assert_eq!(
            env["ANTHROPIC_CUSTOM_HEADERS"].as_deref(),
            Some(
                "x-tracevault-agent: claude-code\n\
                 x-tracevault-repo: 11111111-2222-3333-4444-555555555555\n\
                 x-tracevault-session: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
            )
        );
        let args = args_of(&cmd);
        assert_eq!(args[0], "--settings");
        let settings: serde_json::Value = serde_json::from_str(&args[1]).unwrap();
        assert_eq!(settings["apiKeyHelper"], gateway().key_helper);
        assert_eq!(
            args[2..],
            [
                "--session-id",
                "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                "--verbose"
            ]
        );
        // The key itself is in no argument.
        assert!(!args.iter().any(|a| a.contains("sk-abc")));
    }

    #[test]
    fn a_resumed_claude_session_keeps_its_own_id() {
        for resume in [
            strings(&["--resume", "abc"]),
            strings(&["-c"]),
            strings(&["--session-id=xyz"]),
        ] {
            let cmd = claude_command("claude", &resume, &gateway(), &no_env);
            let args = args_of(&cmd);
            assert_eq!(args[2..], resume[..], "{args:?}");
            let headers = envs_of(&cmd)["ANTHROPIC_CUSTOM_HEADERS"].clone().unwrap();
            assert!(!headers.contains("x-tracevault-session"), "{headers}");
            assert!(headers.contains("x-tracevault-repo"), "{headers}");
        }
    }

    #[test]
    fn claude_with_the_callers_own_settings_gets_the_key_directly() {
        let own = strings(&["--settings", "/tmp/mine.json"]);
        let cmd = claude_command("claude", &own, &gateway(), &no_env);
        assert_eq!(
            envs_of(&cmd)["ANTHROPIC_AUTH_TOKEN"].as_deref(),
            Some("sk-abc")
        );
        let args = args_of(&cmd);
        assert_eq!(args.iter().filter(|a| *a == "--settings").count(), 1);
        assert_eq!(args[args.len() - 2..], own[..]);
    }

    #[test]
    fn claude_keeps_what_the_developer_already_set() {
        let env = |name: &str| match name {
            "ANTHROPIC_MODEL" => Some("my-alias".to_string()),
            "ANTHROPIC_CUSTOM_HEADERS" => Some("x-team: blue".to_string()),
            _ => None,
        };
        let mut launch = gateway();
        launch.repo_id = None;
        let cmd = claude_command("claude", &[], &launch, &env);
        let set = envs_of(&cmd);
        assert!(!set.contains_key("ANTHROPIC_MODEL"), "left as it is");
        assert_eq!(
            set["ANTHROPIC_DEFAULT_HAIKU_MODEL"].as_deref(),
            Some("claude-sonnet")
        );
        let headers = set["ANTHROPIC_CUSTOM_HEADERS"].clone().unwrap();
        assert!(
            headers.starts_with("x-team: blue\nx-tracevault-agent"),
            "{headers}"
        );
        assert!(!headers.contains("x-tracevault-repo"), "{headers}");
    }

    #[test]
    fn without_a_model_claude_is_given_none() {
        let mut launch = gateway();
        launch.model = None;
        let set = envs_of(&claude_command("claude", &[], &launch, &no_env));
        assert!(!set.contains_key("ANTHROPIC_MODEL"));
        assert!(!set.contains_key("ANTHROPIC_DEFAULT_HAIKU_MODEL"));
    }

    #[test]
    fn a_header_value_cannot_start_another_header() {
        let mut launch = gateway();
        launch.repo_id = Some("abc\nx-evil: 1".to_string());
        let set = envs_of(&claude_command("claude", &[], &launch, &no_env));
        let headers = set["ANTHROPIC_CUSTOM_HEADERS"].clone().unwrap();
        assert_eq!(headers.lines().count(), 3, "{headers}");
        assert!(
            headers.contains("x-tracevault-repo: abcx-evil: 1"),
            "{headers}"
        );
    }

    #[test]
    fn codex_gets_a_provider_for_this_run_and_the_key_in_its_environment() {
        let mut launch = gateway();
        launch.model = Some("gpt-general".to_string());
        let cmd = codex_command("codex", &strings(&["exec", "hi"]), &launch);
        let env = envs_of(&cmd);
        assert_eq!(env["VISDOM_TRACING_MODE"].as_deref(), Some("gateway"));
        assert_eq!(env["VISDOM_GATEWAY_API_KEY"].as_deref(), Some("sk-abc"));
        let args = args_of(&cmd);
        assert!(!args.iter().any(|a| a.contains("sk-abc")), "{args:?}");
        let settings: Vec<&String> = args
            .iter()
            .zip(args.iter().skip(1))
            .filter(|(flag, _)| *flag == "-c")
            .map(|(_, value)| value)
            .collect();
        assert_eq!(
            settings,
            [
                r#"model_provider="visdom-gateway""#,
                r#"model_providers.visdom-gateway.name="Visdom AI Gateway""#,
                r#"model_providers.visdom-gateway.base_url="https://llm.example.com/v1""#,
                r#"model_providers.visdom-gateway.env_key="VISDOM_GATEWAY_API_KEY""#,
                r#"model_providers.visdom-gateway.wire_api="responses""#,
                r#"model_providers.visdom-gateway.http_headers={ "x-tracevault-agent" = "codex", "x-tracevault-repo" = "11111111-2222-3333-4444-555555555555" }"#,
            ]
        );
        // Every value is valid TOML for the key it sets.
        for setting in settings {
            let (key, value) = setting.split_once('=').unwrap();
            let parsed: Result<toml::Table, _> = toml::from_str(&format!("x = {value}"));
            assert!(parsed.is_ok(), "{key}: {value}");
        }
        assert_eq!(
            args[args.len() - 4..],
            ["--model", "gpt-general", "exec", "hi"]
        );
    }

    #[test]
    fn codex_keeps_the_callers_model() {
        let cmd = codex_command("codex", &strings(&["-m", "o-mine"]), &gateway());
        let args = args_of(&cmd);
        assert!(!args.contains(&"--model".to_string()), "{args:?}");
        assert_eq!(args[args.len() - 2..], ["-m", "o-mine"]);
    }

    #[test]
    fn options_are_recognised_with_and_without_an_equals_sign() {
        let args = strings(&["--model=x", "--resume"]);
        assert!(has_option(&args, &["--model"]));
        assert!(has_option(&args, &["-r", "--resume"]));
        assert!(!has_option(&args, &["--mod", "--settings"]));
    }
}
