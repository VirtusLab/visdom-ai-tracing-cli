//! Handing the terminal over to the agent.

use std::path::Path;
use std::process::Command;

use tracevault_cli::agent::Agent;
use tracevault_cli::config::TracevaultConfig;

use crate::agents::via_gateway::GatewayLaunch;
use crate::config::agent_config_key;
use crate::doctor::Session;
use crate::gateway::{self, KeyStore};

pub use crate::agents::via_gateway::shell_quote;

/// The command a launched agent runs to get its gateway key: this `visdom`,
/// asked for the key of one console and project.
pub fn key_helper_command(visdom: &Path, console_url: &str, project_id: &str) -> String {
    format!(
        "{} gateway-key --console {} --project {}",
        shell_quote(&visdom.to_string_lossy()),
        shell_quote(console_url),
        shell_quote(project_id)
    )
}

/// The HTTP client for getting a key: the agent does not start without it,
/// so it may take longer than a check.
pub fn key_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_default()
}

/// A gateway session ready to start.
pub struct Prepared {
    pub launch: GatewayLaunch,
    /// What the developer should know (no model alias to ask for).
    pub notes: Vec<String>,
    /// The model aliases the key may call.
    pub available: Vec<String>,
}

/// Everything the agent needs to run through the gateway: the developer's
/// key (the kept one, or a new one), a model alias and the ids that attribute
/// the session. The second value holds what the developer should know (no
/// model alias to ask for).
///
/// The LLM gateway is asked for its models with the key before anything is
/// started: an agent pointed at a gateway that does not answer, or does not
/// take the key, would run with nothing recording it. A kept key the gateway
/// refuses (a newer one was issued elsewhere, which revoked it) is replaced
/// once.
pub fn prepare_gateway(
    session: &Session,
    runtime: &tokio::runtime::Runtime,
    visdom: &Path,
    now: i64,
) -> Result<Prepared, String> {
    let plan = match &session.gateway {
        Some(Ok(plan)) => plan,
        Some(Err(why)) => return Err(why.clone()),
        None => return Err("this session is not in gateway mode".to_string()),
    };
    let token = session
        .token
        .as_deref()
        .ok_or("the AI Gateway needs a Visdom account login: run `visdom login`")?;
    let http = key_http_client();
    let store = KeyStore::at(gateway::key_store_path_in(&session.config_root));
    let key_unless = |rejected: Option<&str>| {
        runtime.block_on(gateway::ensure_key(
            &http,
            &plan.console_url,
            token,
            &plan.project.id,
            &store,
            now,
            rejected,
        ))
    };
    let models = |key: &str| runtime.block_on(gateway::models(&http, &plan.llm_url, key));
    let mut ensured = key_unless(None)?;
    let available = match models(&ensured.key.secret) {
        Ok(available) => available,
        Err(refusal) if refusal.rejected && !ensured.issued => {
            ensured = key_unless(Some(&ensured.key.secret))?;
            models(&ensured.key.secret).map_err(|e| e.why)?
        }
        Err(e) => return Err(e.why),
    };
    let key = ensured.key;
    let mut notes: Vec<String> = Vec::new();
    let model = gateway::pick_model(
        session.ctx.agent,
        session.gateway_model.as_deref(),
        &available,
    );
    if model.is_none() {
        notes.push(format!(
            "no AI Gateway model alias was chosen for {}: it asks for its own default, which \
             the project `{}` may not allow (set {}_model under [gateway] in your visdom config, \
             or VISDOM_GATEWAY_MODEL)",
            session.ctx.agent.label(),
            plan.project.slug,
            agent_config_key(session.ctx.agent)
        ));
    }
    // The repository a new session's events go under, as far as it is known
    // before the session exists: the checkout's own, else the user's default
    // (`tracevault repo switch --user`), as the capture hook has it.
    let repo_id = session
        .ctx
        .workdir
        .primary
        .as_deref()
        .and_then(|root| TracevaultConfig::try_load(root).ok().flatten())
        .and_then(|config| config.repo_id)
        .filter(|id| !id.trim().is_empty())
        .or_else(|| tracevault_cli::user_default::load().map(|binding| binding.repo_id))
        .filter(|id| !id.trim().is_empty());
    Ok(Prepared {
        launch: GatewayLaunch {
            llm_url: plan.llm_url.clone(),
            key: key.secret,
            key_helper: key_helper_command(visdom, &plan.console_url, &plan.project.id),
            model,
            repo_id,
            session_id: uuid::Uuid::new_v4().to_string(),
        },
        notes,
        available,
    })
}

/// The environment variable that gives the gateway setting `key` of the user
/// config, for when the file cannot be written.
pub fn gateway_setting_env(key: &str) -> Option<&'static str> {
    match key {
        "project" => Some("export VISDOM_GATEWAY_PROJECT=<project>"),
        "url" => Some("export VISDOM_GATEWAY_URL=<console address>"),
        key if key.ends_with("_model") => Some("export VISDOM_GATEWAY_MODEL=<model alias>"),
        _ => None,
    }
}

/// Why there is no model alias for `agent` to ask the gateway for, and what
/// to set: without one the agent asks for its own default, which a project's
/// allow-list of aliases does not have.
pub fn no_model(agent: Agent, project: &str, available: &[String]) -> String {
    let key = format!("{}_model", agent_config_key(agent));
    let choice = match available.is_empty() {
        true => format!("the project `{project}` lets your key call no model alias at all"),
        false => format!(
            "none of the model aliases your key may call in the project `{project}` is named \
             for {} ({})",
            agent.label(),
            available.join(", ")
        ),
    };
    format!(
        "{choice}: set {key} under [gateway] in your visdom config, or VISDOM_GATEWAY_MODEL, to \
         the alias to use"
    )
}

/// Where the gateway's settings are made, for somebody whose gateway is not
/// ready or who wants another one.
pub fn gateway_settings_hint(user_config: &Path) -> String {
    format!(
        "  Settings: `url`, `project`, `claude_code_model`, `codex_model` under [gateway] and \
         `tracing_mode` in {}.\n  \
         Left out there, they are taken from the environment — VISDOM_GATEWAY_URL, \
         VISDOM_GATEWAY_PROJECT, VISDOM_GATEWAY_MODEL, VISDOM_TRACING_MODE (which win over the \
         file when set) — and the gateway's address from the tracing server.",
        user_config.display()
    )
}

/// What `visdom init` set up for the gateway, and where each setting comes
/// from.
pub fn gateway_summary(
    agent: Agent,
    mode: &crate::mode::ResolvedMode,
    plan: &crate::components::gateway::GatewayPlan,
    sources: &crate::doctor::GatewaySources,
    model: Option<&str>,
    user_config: &Path,
) -> String {
    use crate::mode::ModeReason;
    let mode_source = match mode.reason {
        ModeReason::Policy(policy) => format!(
            "required by the workspace's AI traffic policy, `{}`",
            policy.name()
        ),
        _ => "your choice".to_string(),
    };
    let or = |source: &Option<String>, otherwise: &str| {
        source.clone().unwrap_or_else(|| otherwise.to_string())
    };
    let rows = [
        ("mode", "gateway".to_string(), mode_source),
        (
            "console",
            plan.console_url.clone(),
            or(&sources.console_url, "not set"),
        ),
        (
            "LLM gateway",
            plan.llm_url.clone(),
            "named by the console".to_string(),
        ),
        (
            "project",
            plan.project.slug.clone(),
            or(&sources.project, "the only project open to you"),
        ),
        (
            "model",
            model.unwrap_or("the agent's own default").to_string(),
            match model {
                Some(_) => or(
                    &sources.model,
                    "the first alias the key may call that is named for the agent",
                ),
                None => "no alias is set, and the project may refuse the default".to_string(),
            },
        ),
        (
            "personal key",
            "issued, and accepted by the LLM gateway".to_string(),
            "kept for you only, renewed by `visdom`".to_string(),
        ),
    ];
    let width = rows
        .iter()
        .map(|(_, value, _)| value.len())
        .max()
        .unwrap_or(0);
    let mut out = format!(
        "AI Gateway ready for {}: `visdom` starts it through the gateway.\n",
        agent.label()
    );
    for (name, value, source) in rows {
        out.push_str(&format!("  {name:<13}{value:<width$}  ({source})\n"));
    }
    out.push_str(&gateway_settings_hint(user_config));
    out.push_str(
        "\n  To send an agent you start without `visdom` through the gateway as well:  \
         source <(visdom env)\n",
    );
    out
}

/// What to tell the user when the agent's executable cannot be started.
pub fn launch_error_message(
    program: &str,
    error: &std::io::Error,
    agent: Agent,
    user_config: &Path,
) -> String {
    format!(
        "visdom: cannot start {program}: {error} (set agents.{} in {} to use another executable)",
        agent_config_key(agent),
        user_config.display()
    )
}

/// Replace this process with the agent, so the terminal, signals and exit
/// code are the agent's. Returns only by exiting: 127 when the executable
/// cannot be started.
pub fn exec_agent(mut cmd: Command, agent: Agent, user_config: &Path) -> ! {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let error = replace_process(&mut cmd);
    eprintln!(
        "{}",
        launch_error_message(&program, &error, agent, user_config)
    );
    std::process::exit(127)
}

#[cfg(unix)]
fn replace_process(cmd: &mut Command) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    cmd.exec()
}

/// Without `exec`: run the agent as a child and exit with its status.
#[cfg(not(unix))]
fn replace_process(cmd: &mut Command) -> std::io::Error {
    match cmd.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_error_names_the_program_and_the_config_key() {
        let error = std::io::Error::new(std::io::ErrorKind::NotFound, "No such file");
        let message = launch_error_message(
            "claude",
            &error,
            Agent::ClaudeCode,
            Path::new("/home/u/.config/visdom/config.toml"),
        );
        assert!(
            message.contains("cannot start claude: No such file"),
            "{message}"
        );
        assert!(message.contains("agents.claude_code"), "{message}");
        assert!(
            message.contains("/home/u/.config/visdom/config.toml"),
            "{message}"
        );
        let message = launch_error_message("codex", &error, Agent::Codex, Path::new("/c"));
        assert!(message.contains("agents.codex"), "{message}");
    }

    #[test]
    fn the_key_helper_is_one_safe_shell_command() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(
            key_helper_command(
                Path::new("/opt/my tools/visdom"),
                "https://gw.example.com",
                "p-1; rm -rf ~"
            ),
            "'/opt/my tools/visdom' gateway-key --console 'https://gw.example.com' --project \
             'p-1; rm -rf ~'"
        );
    }

    /// The helper command survives a real shell as the three values it holds.
    #[cfg(unix)]
    #[test]
    fn a_shell_reads_the_quoted_values_back() {
        let tricky = ["it's", "a b", "$HOME `id` \"x\"", "p-1; echo no"];
        for value in tricky {
            let output = Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {}", shell_quote(value)))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&output.stdout), value);
        }
    }
}
