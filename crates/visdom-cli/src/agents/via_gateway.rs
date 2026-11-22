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

/// The value `args` give one of `options` (`--name value` or `--name=value`).
fn option_value<'a>(args: &'a [String], options: &[&str]) -> Option<&'a str> {
    args.iter().enumerate().find_map(|(i, arg)| {
        options.iter().find_map(|o| {
            if arg == o {
                args.get(i + 1).map(String::as_str)
            } else {
                arg.strip_prefix(o).and_then(|rest| rest.strip_prefix('='))
            }
        })
    })
}

/// `value` as one header value: no line breaks, which would start another
/// header.
fn header_value(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}

/// Claude Code's switches to another provider's API. Each takes precedence
/// over `ANTHROPIC_BASE_URL`, so one left set would send the session past the
/// gateway while the hooks record nothing.
const CLAUDE_OTHER_PROVIDERS: [&str; 3] = [
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// The session id the caller's own options give Claude Code, when they name
/// one: `--session-id`, or `--resume <id>`, which carries on under the same
/// id unless `--fork-session` asks for a new one. `None` when Claude Code
/// decides (a picker, `--continue`, a fork).
fn callers_session(args: &[String]) -> Option<&str> {
    let is_id = |value: &&str| uuid::Uuid::parse_str(value).is_ok();
    option_value(args, &["--session-id"])
        .filter(is_id)
        .or_else(|| {
            option_value(args, &["--resume", "-r"])
                .filter(is_id)
                .filter(|_| !has_option(args, &["--fork-session"]))
        })
}

/// The values `args` give `--settings`, each a file or a JSON text, and
/// `args` without them. Options after `--` are the prompt's.
fn split_settings(args: &[String]) -> (Vec<String>, Vec<String>) {
    let (mut given, mut rest) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            rest.extend_from_slice(&args[i..]);
            break;
        }
        if arg == "--settings" {
            given.extend(args.get(i + 1).cloned());
            i += 2;
        } else if let Some(value) = arg.strip_prefix("--settings=") {
            given.push(value.to_string());
            i += 1;
        } else {
            rest.push(arg.clone());
            i += 1;
        }
    }
    (given, rest)
}

/// The settings object a `--settings` value stands for.
fn read_settings(value: &str) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let failed = |why: String| format!("the Claude Code settings given with --settings ({why})");
    let text = if value.trim_start().starts_with('{') {
        value.to_string()
    } else {
        std::fs::read_to_string(value)
            .map_err(|e| failed(format!("`{value}` cannot be read: {e}")))?
    };
    match serde_json::from_str(&text) {
        Ok(serde_json::Value::Object(map)) => Ok(map),
        Ok(_) => Err(failed(format!("`{value}` is not a JSON object"))),
        Err(e) => Err(failed(format!("`{value}` is not JSON: {e}"))),
    }
}

/// Claude Code through the gateway. `env` reads the current environment.
///
/// Where the session goes is given as settings on the command line
/// (`--settings`), not only as environment: Claude Code applies the `env` of
/// its settings files over the environment it was started in, so a base URL
/// or a provider switch left in the developer's `settings.json` would
/// otherwise send the session past the gateway — or the gateway key to
/// another address — while the hooks record nothing. Settings given on the
/// command line come after the user's and the project's. (Settings an
/// organisation manages come after everything and are not `visdom`'s to
/// overrule.)
///
/// - The key comes from `apiKeyHelper`, so a key renewed during a long
///   session is picked up. Settings the caller passes with `--settings` are
///   kept, merged under these.
/// - The session id is fixed up front and sent with every request. A caller
///   who names the session themselves gets theirs sent; one who leaves it to
///   Claude Code (`--continue`, a picker) gets none, and the gateway's
///   records of that session carry no session id.
///
/// `Err` when the caller's own `--settings` cannot be read: they could not be
/// merged, and passing them on unread could undo the routing.
pub fn claude_command(
    program: &str,
    args: &[String],
    gateway: &GatewayLaunch,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Command, String> {
    let (given, args) = split_settings(args);
    let mut settings = serde_json::Map::new();
    for value in &given {
        // A later one wins key by key, as far as this can tell.
        settings.extend(read_settings(value)?);
    }
    let mut settings_env = match settings.remove("env") {
        Some(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };

    let own_session = has_option(
        &args,
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
    let session = match own_session {
        true => callers_session(&args),
        false => Some(gateway.session_id.as_str()),
    };
    // The developer's own headers stay, from the environment and from the
    // settings they passed.
    let callers_headers = settings_env
        .get("ANTHROPIC_CUSTOM_HEADERS")
        .and_then(|v| v.as_str())
        .map(String::from);
    let mut headers: Vec<String> = [env("ANTHROPIC_CUSTOM_HEADERS"), callers_headers]
        .into_iter()
        .flatten()
        .filter(|h| !h.trim().is_empty())
        .collect();
    headers.push(format!(
        "x-tracevault-agent: {}",
        Agent::ClaudeCode.tool_name()
    ));
    if let Some(repo) = &gateway.repo_id {
        headers.push(format!("x-tracevault-repo: {}", header_value(repo)));
    }
    if let Some(session) = session {
        headers.push(format!("x-tracevault-session: {}", header_value(session)));
    }
    let headers = headers.join("\n");

    // What decides where the session goes: in the environment and, over
    // whatever a settings file says, in the settings.
    let mut routing: Vec<(&str, String)> = vec![
        ("ANTHROPIC_BASE_URL", gateway.llm_url.clone()),
        ("ANTHROPIC_CUSTOM_HEADERS", headers),
        (
            "CLAUDE_CODE_API_KEY_HELPER_TTL_MS",
            CLAUDE_KEY_TTL_MS.into(),
        ),
    ];
    // Claude Code reads "0" as off; a variable cannot be unset from settings.
    routing.extend(CLAUDE_OTHER_PROVIDERS.map(|switch| (switch, "0".to_string())));

    let mut cmd = Command::new(program);
    cmd.env(MODE_ENV, TracingMode::Gateway.name());
    for (name, value) in &routing {
        cmd.env(name, value);
        settings_env.insert(name.to_string(), value.clone().into());
    }
    for switch in CLAUDE_OTHER_PROVIDERS {
        cmd.env_remove(switch);
    }
    // A key of the developer's own would be sent in place of the gateway's.
    for key in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"] {
        cmd.env_remove(key);
        settings_env.remove(key);
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
    settings.insert("env".to_string(), settings_env.into());
    settings.insert(
        "apiKeyHelper".to_string(),
        gateway.key_helper.clone().into(),
    );
    cmd.arg("--settings")
        .arg(serde_json::Value::Object(settings).to_string());
    if !own_session {
        cmd.arg("--session-id").arg(&gateway.session_id);
    }
    cmd.args(args);
    Ok(cmd)
}

/// A TOML basic string.
fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_string()).to_string()
}

/// What the caller's own options would take away from the gateway, if
/// anything. Codex applies configuration overrides in the order given, and
/// the caller's come last: one that names another model provider, or redefines
/// `visdom`'s, would send the session past the gateway (or the gateway key
/// elsewhere) while the hooks record nothing. Options after `--` are the
/// prompt's.
fn codex_conflict(args: &[String]) -> Option<String> {
    let options = args.iter().take_while(|arg| *arg != "--");
    let mut overrides: Vec<&str> = Vec::new();
    let mut awaiting = false;
    for arg in options {
        if std::mem::take(&mut awaiting) {
            overrides.push(arg);
        } else if arg == "-c" || arg == "--config" {
            awaiting = true;
        } else if let Some(value) = arg.strip_prefix("--config=") {
            overrides.push(value);
        } else if let Some(value) = arg.strip_prefix("-c").filter(|_| !arg.starts_with("--")) {
            overrides.push(value.strip_prefix('=').unwrap_or(value));
        } else if arg == "--oss"
            || arg == "--local-provider"
            || arg.starts_with("--local-provider=")
        {
            return Some(arg.clone());
        }
    }
    overrides
        .into_iter()
        .find(|setting| {
            let key: String = setting
                .split('=')
                .next()
                .unwrap_or_default()
                .chars()
                .filter(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
                .collect();
            key == "model_provider"
                || key == "model_providers"
                || key == format!("model_providers.{CODEX_PROVIDER}")
                || key.starts_with(&format!("model_providers.{CODEX_PROVIDER}."))
        })
        .map(|setting| format!("-c {setting}"))
}

/// Codex through the gateway: a model provider defined for this run with
/// `-c`, its key in the environment. Codex names the session itself
/// (`thread-id`), which the gateway takes as the session. The key is fixed
/// for the life of the process.
///
/// `Err` when the caller's own options choose or redefine the model provider:
/// they come after these and would win.
pub fn codex_command(
    program: &str,
    args: &[String],
    gateway: &GatewayLaunch,
) -> Result<Command, String> {
    if let Some(option) = codex_conflict(args) {
        return Err(format!(
            "`{option}` chooses Codex's model provider, which through the AI Gateway is the \
             gateway: leave it out, or use `--mode hooks`"
        ));
    }
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
    Ok(cmd)
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

    fn claude(
        args: &[String],
        gateway: &GatewayLaunch,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Command {
        claude_command("claude", args, gateway, env).unwrap()
    }

    fn codex(args: &[String], gateway: &GatewayLaunch) -> Command {
        codex_command("codex", args, gateway).unwrap()
    }

    /// The settings a Claude command carries: always the first option.
    fn settings_of(cmd: &Command) -> serde_json::Value {
        let args = args_of(cmd);
        assert_eq!(args[0], "--settings");
        assert_eq!(args.iter().filter(|a| *a == "--settings").count(), 1);
        serde_json::from_str(&args[1]).unwrap()
    }

    #[test]
    fn claude_gets_the_gateway_the_key_helper_and_a_session() {
        let cmd = claude(&strings(&["--verbose"]), &gateway(), &no_env);
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
        let settings = settings_of(&cmd);
        assert_eq!(settings["apiKeyHelper"], gateway().key_helper);
        // Where the session goes is in the settings too, over whatever a
        // settings file of the developer's says.
        assert_eq!(
            settings["env"],
            serde_json::json!({
                "ANTHROPIC_BASE_URL": "https://llm.example.com",
                "ANTHROPIC_CUSTOM_HEADERS": env["ANTHROPIC_CUSTOM_HEADERS"],
                "CLAUDE_CODE_API_KEY_HELPER_TTL_MS": "300000",
                "CLAUDE_CODE_USE_BEDROCK": "0",
                "CLAUDE_CODE_USE_VERTEX": "0",
                "CLAUDE_CODE_USE_FOUNDRY": "0",
            })
        );
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
    fn a_session_claude_code_picks_itself_gets_no_session_id() {
        for resume in [
            strings(&["--resume"]),
            strings(&["--resume", "a title, not an id"]),
            strings(&["-c"]),
            strings(&["--session-id=xyz"]),
            // A fork gets a new id, which only Claude Code knows.
            strings(&["--resume", OTHER_SESSION, "--fork-session"]),
        ] {
            let cmd = claude(&resume, &gateway(), &no_env);
            let args = args_of(&cmd);
            assert_eq!(args[2..], resume[..], "{args:?}");
            let headers = envs_of(&cmd)["ANTHROPIC_CUSTOM_HEADERS"].clone().unwrap();
            assert!(!headers.contains("x-tracevault-session"), "{headers}");
            assert!(headers.contains("x-tracevault-repo"), "{headers}");
        }
    }

    const OTHER_SESSION: &str = "99999999-8888-7777-6666-555555555555";

    #[test]
    fn a_session_the_caller_names_is_the_one_sent() {
        for named in [
            strings(&["--session-id", OTHER_SESSION]),
            strings(&[&format!("--session-id={OTHER_SESSION}")]),
            strings(&["--resume", OTHER_SESSION]),
            strings(&["-r", OTHER_SESSION, "--verbose"]),
        ] {
            let cmd = claude(&named, &gateway(), &no_env);
            // Nothing of visdom's own is added to what the caller asked for.
            assert_eq!(args_of(&cmd)[2..], named[..]);
            let headers = envs_of(&cmd)["ANTHROPIC_CUSTOM_HEADERS"].clone().unwrap();
            assert!(
                headers.ends_with(&format!("x-tracevault-session: {OTHER_SESSION}")),
                "{headers}"
            );
            assert!(!headers.contains(&gateway().session_id), "{headers}");
        }
    }

    #[test]
    fn claude_is_not_left_pointed_at_another_provider() {
        let cmd = claude(&[], &gateway(), &no_env);
        let env = envs_of(&cmd);
        let settings = settings_of(&cmd);
        for switch in [
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ] {
            assert_eq!(env[switch], None, "{switch} must be removed");
            assert_eq!(settings["env"][switch], "0", "{switch} must be off");
        }
    }

    #[test]
    fn the_callers_own_settings_are_merged_under_the_gateways() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("mine.json");
        std::fs::write(
            &file,
            r#"{"permissions":{"allow":["Bash(ls)"]},"apiKeyHelper":"echo mine",
                "env":{"ANTHROPIC_BASE_URL":"https://elsewhere.example.com",
                       "CLAUDE_CODE_USE_BEDROCK":"1","ANTHROPIC_API_KEY":"sk-mine",
                       "ANTHROPIC_CUSTOM_HEADERS":"x-team: blue","MY_VAR":"kept"}}"#,
        )
        .unwrap();
        let inline = r#"{"model":"opus","env":{"OTHER":"1"}}"#;
        for own in [
            strings(&["--verbose", "--settings", file.to_str().unwrap()]),
            strings(&[&format!("--settings={}", file.display()), "--verbose"]),
        ] {
            let cmd = claude(&own, &gateway(), &no_env);
            let settings = settings_of(&cmd);
            assert_eq!(settings["permissions"]["allow"][0], "Bash(ls)");
            assert_eq!(settings["apiKeyHelper"], gateway().key_helper);
            let env = &settings["env"];
            assert_eq!(env["MY_VAR"], "kept");
            assert_eq!(env["ANTHROPIC_BASE_URL"], "https://llm.example.com");
            assert_eq!(env["CLAUDE_CODE_USE_BEDROCK"], "0");
            assert_eq!(env.get("ANTHROPIC_API_KEY"), None);
            let headers = env["ANTHROPIC_CUSTOM_HEADERS"].as_str().unwrap();
            assert!(
                headers.starts_with("x-team: blue\nx-tracevault-agent"),
                "{headers}"
            );
            // Theirs is gone from the options; the rest stays.
            let args = args_of(&cmd);
            assert_eq!(args[2..4], ["--session-id", gateway().session_id.as_str()]);
            assert_eq!(args[4..], ["--verbose"]);
            // The key reaches Claude Code through the helper alone.
            assert_eq!(envs_of(&cmd)["ANTHROPIC_AUTH_TOKEN"], None);
        }
        // JSON given in place, twice: both are kept.
        let both = strings(&["--settings", inline, "--settings", file.to_str().unwrap()]);
        let settings = settings_of(&claude(&both, &gateway(), &no_env));
        assert_eq!(settings["model"], "opus");
        assert_eq!(settings["permissions"]["allow"][0], "Bash(ls)");
    }

    #[test]
    fn settings_that_cannot_be_read_stop_the_gateway_launch() {
        for (own, said) in [
            (
                strings(&["--settings", "/nonexistent/mine.json"]),
                "cannot be read",
            ),
            (strings(&["--settings", "{not json"]), "is not JSON"),
            (strings(&["--settings=[1]"]), "cannot be read"),
        ] {
            let err = claude_command("claude", &own, &gateway(), &no_env).unwrap_err();
            assert!(err.contains(said), "{err}");
        }
        // After `--` it is the prompt, not an option.
        let prompt = strings(&["--", "--settings", "what does this flag do?"]);
        let cmd = claude(&prompt, &gateway(), &no_env);
        assert_eq!(args_of(&cmd)[4..], prompt[..]);
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
        let cmd = claude(&[], &launch, &env);
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
        let set = envs_of(&claude(&[], &launch, &no_env));
        assert!(!set.contains_key("ANTHROPIC_MODEL"));
        assert!(!set.contains_key("ANTHROPIC_DEFAULT_HAIKU_MODEL"));
    }

    #[test]
    fn a_header_value_cannot_start_another_header() {
        let mut launch = gateway();
        launch.repo_id = Some("abc\nx-evil: 1".to_string());
        let set = envs_of(&claude(&[], &launch, &no_env));
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
        let cmd = codex(&strings(&["exec", "hi"]), &launch);
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
        let cmd = codex(&strings(&["-m", "o-mine"]), &gateway());
        let args = args_of(&cmd);
        assert!(!args.contains(&"--model".to_string()), "{args:?}");
        assert_eq!(args[args.len() - 2..], ["-m", "o-mine"]);
    }

    #[test]
    fn codex_options_that_choose_the_provider_stop_the_gateway_launch() {
        for conflicting in [
            strings(&["-c", "model_provider=openai"]),
            strings(&["exec", "--config", r#"model_provider = "openai""#, "hi"]),
            strings(&["--config=model_provider=openai"]),
            strings(&["-cmodel_provider=openai"]),
            strings(&[
                "-c",
                r#"model_providers.visdom-gateway.base_url="https://evil""#,
            ]),
            strings(&["-c", r#"model_providers."visdom-gateway".env_key="X""#]),
            strings(&["-c", "model_providers={}"]),
            strings(&["--oss"]),
            strings(&["--local-provider", "ollama"]),
        ] {
            let err = codex_command("codex", &conflicting, &gateway()).unwrap_err();
            assert!(err.contains("model provider"), "{conflicting:?}: {err}");
        }
        for fine in [
            strings(&["-c", "model_reasoning_effort=high", "exec", "hi"]),
            strings(&["-c", r#"model_providers.mine.base_url="https://x""#]),
            strings(&["--config", "model=o-mine"]),
            // After `--` it is the prompt.
            strings(&["exec", "--", "-c", "model_provider=openai"]),
        ] {
            assert!(
                codex_command("codex", &fine, &gateway()).is_ok(),
                "{fine:?}"
            );
        }
    }

    #[test]
    fn options_are_recognised_with_and_without_an_equals_sign() {
        let args = strings(&["--model=x", "--resume"]);
        assert!(has_option(&args, &["--model"]));
        assert!(has_option(&args, &["-r", "--resume"]));
        assert!(!has_option(&args, &["--mod", "--settings"]));
    }
}
