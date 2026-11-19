//! Handing the terminal over to the agent.

use std::path::Path;
use std::process::Command;

use tracevault_cli::agent::Agent;

use crate::config::agent_config_key;

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
}
