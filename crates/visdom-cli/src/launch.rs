//! Handing the terminal over to the agent.

use std::path::Path;
use std::process::Command;

use tracevault_cli::agent::Agent;
use tracevault_cli::config::TracevaultConfig;

use crate::agents::via_gateway::GatewayLaunch;
use crate::config::agent_config_key;
use crate::doctor::Session;
use crate::gateway::{self, KeyStore};

/// `text` as one word of a POSIX shell command.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

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

/// Everything the agent needs to run through the gateway: the developer's
/// key (the kept one, or a new one), a model alias and the ids that attribute
/// the session. The second value holds what the developer should know (a key
/// that could not be kept, models that could not be listed).
pub fn prepare_gateway(
    session: &Session,
    runtime: &tokio::runtime::Runtime,
    visdom: &Path,
    now: i64,
) -> Result<(GatewayLaunch, Vec<String>), String> {
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
    let (key, kept) = runtime.block_on(gateway::ensure_key(
        &http,
        &plan.console_url,
        token,
        &plan.project.id,
        &store,
        now,
    ))?;
    let mut notes: Vec<String> = kept.into_iter().collect();
    // The aliases only matter when no model is configured.
    let available = match &session.gateway_model {
        Some(_) => Vec::new(),
        None => runtime
            .block_on(gateway::models(&http, &plan.llm_url, &key.secret))
            .unwrap_or_else(|why| {
                notes.push(why);
                Vec::new()
            }),
    };
    let model = gateway::pick_model(
        session.ctx.agent,
        session.gateway_model.as_deref(),
        &available,
    );
    if model.is_none() {
        notes.push(format!(
            "no AI Gateway model alias was chosen for {}: it asks for its own default, which \
             the project `{}` may not allow (set {}_model under [gateway] in your visdom config)",
            session.ctx.agent.label(),
            plan.project.slug,
            agent_config_key(session.ctx.agent)
        ));
    }
    let repo_id = session
        .ctx
        .workdir
        .primary
        .as_deref()
        .and_then(|root| TracevaultConfig::try_load(root).ok().flatten())
        .and_then(|config| config.repo_id)
        .filter(|id| !id.trim().is_empty());
    Ok((
        GatewayLaunch {
            llm_url: plan.llm_url.clone(),
            key: key.secret,
            key_helper: key_helper_command(visdom, &plan.console_url, &plan.project.id),
            model,
            repo_id,
            session_id: uuid::Uuid::new_v4().to_string(),
        },
        notes,
    ))
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
