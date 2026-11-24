//! What `visdom` knows about each agent it can launch: where the agent keeps
//! its MCP servers, how to register one through the agent's own CLI, and how
//! to start it. Components never read an agent's files themselves.

use std::path::{Path, PathBuf};

use tracevault_cli::agent::Agent;

use crate::finding::FixAction;

pub mod claude;
pub mod codex;
pub mod via_gateway;

/// Where an MCP server entry is configured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpScope {
    /// For one project directory, in the user's private agent state.
    Local,
    /// In a file committed to the repository.
    Project,
    /// For every project of this user.
    User,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpEntry {
    pub name: String,
    pub url: Option<String>,
    pub oauth_client_id: Option<String>,
    pub scope: McpScope,
    /// The entry carries settings beyond its URL and OAuth client (headers,
    /// timeouts, …) that removing and re-adding it would lose.
    pub extra_settings: bool,
    /// Configured but switched off: the agent does not load it.
    pub disabled: bool,
}

/// Where the agent is about to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workdir {
    /// The directory `visdom` was run in; the agent starts there.
    pub start_dir: PathBuf,
    /// Top level of the git checkout around it. `None` outside a repository.
    pub checkout: Option<PathBuf>,
    /// The repository's primary checkout: the same as `checkout`, except in a
    /// linked worktree.
    pub primary: Option<PathBuf>,
}

impl Workdir {
    /// A directory outside any repository.
    pub fn plain(start_dir: impl Into<PathBuf>) -> Self {
        Workdir {
            start_dir: start_dir.into(),
            checkout: None,
            primary: None,
        }
    }

    /// A primary checkout, entered at its top level.
    pub fn checkout(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Workdir {
            start_dir: dir.clone(),
            checkout: Some(dir.clone()),
            primary: Some(dir),
        }
    }
}

/// The directories the agents keep their state under.
#[derive(Clone, Debug)]
pub struct AgentPaths {
    pub home: PathBuf,
    /// `$CODEX_HOME`, or `~/.codex`.
    pub codex_home: PathBuf,
}

pub trait AgentAdapter {
    fn agent(&self) -> Agent;

    /// The executable `visdom` runs for this agent.
    fn program(&self) -> &str;

    /// Every MCP server the agent would load when started in `workdir`. An
    /// unreadable or malformed file is an error that names it.
    fn mcp_entries(&self, workdir: &Workdir) -> Result<Vec<McpEntry>, String>;

    /// The command that registers an HTTP MCP server with a fixed OAuth
    /// client in `scope`, for an agent started in `workdir`. An agent with a
    /// single configuration ignores the scope.
    fn register_mcp(
        &self,
        name: &str,
        url: &str,
        client_id: &str,
        scope: McpScope,
        workdir: &Workdir,
    ) -> FixAction;

    /// The command that removes `entry`.
    fn remove_mcp(&self, entry: &McpEntry, workdir: &Workdir) -> FixAction;

    /// The command that starts the agent with `args`.
    fn launch(&self, args: &[String]) -> std::process::Command {
        let mut cmd = std::process::Command::new(self.program());
        cmd.args(args);
        cmd
    }

    /// The command that starts the agent with `args` and its LLM traffic
    /// going through the AI Gateway. `env` reads the current environment.
    /// `Err` when the caller's own `args` would take the session elsewhere.
    fn launch_via_gateway(
        &self,
        args: &[String],
        gateway: &via_gateway::GatewayLaunch,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<std::process::Command, String> {
        match self.agent() {
            Agent::Codex => via_gateway::codex_command(self.program(), args, gateway),
            _ => via_gateway::claude_command(self.program(), args, gateway, env),
        }
    }

    /// Shell commands that send this agent, started from that shell without
    /// `visdom`, through the AI Gateway (`visdom env`).
    fn env_script(
        &self,
        gateway: &via_gateway::GatewayLaunch,
        project: &str,
    ) -> Result<String, String> {
        match self.agent() {
            Agent::Codex => via_gateway::codex_env_script(self.program(), gateway, project),
            _ => Ok(via_gateway::claude_env_script(gateway, project)),
        }
    }
}

/// The adapter for `agent`. `program_override` replaces the executable name.
/// Agents `visdom` cannot launch have no adapter.
pub fn adapter_for(
    agent: Agent,
    paths: AgentPaths,
    program_override: Option<String>,
) -> Option<Box<dyn AgentAdapter>> {
    match agent {
        Agent::ClaudeCode => Some(Box::new(claude::Claude::new(paths, program_override))),
        Agent::Codex => Some(Box::new(codex::Codex::new(paths, program_override))),
        Agent::Gsd | Agent::OpenCode | Agent::Amp => None,
    }
}

/// `text` of the file at `path`, `None` when it does not exist.
pub(crate) fn read_optional(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

fn run_command(program: &str, args: &[&str], cwd: Option<&Path>) -> FixAction {
    FixAction::RunCommand {
        program: program.to_string(),
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: cwd.map(Path::to_path_buf),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(root: &Path) -> AgentPaths {
        AgentPaths {
            home: root.to_path_buf(),
            codex_home: root.join(".codex"),
        }
    }

    fn args_of(action: &FixAction) -> (String, Vec<String>, Option<PathBuf>) {
        match action {
            FixAction::RunCommand { program, args, cwd } => {
                (program.clone(), args.clone(), cwd.clone())
            }
            other => panic!("expected a command, got {other:?}"),
        }
    }

    #[test]
    fn register_commands_exact_args() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Path::new("/work/repo");
        let workdir = Workdir {
            start_dir: repo.join("src"),
            checkout: Some(repo.to_path_buf()),
            primary: Some(repo.to_path_buf()),
        };
        let url = "https://ctx.example.com/mcp/remote";

        let claude = adapter_for(Agent::ClaudeCode, paths(tmp.path()), None).unwrap();
        let (program, args, cwd) = args_of(&claude.register_mcp(
            "bazaar",
            url,
            "visdom-context-mcp",
            McpScope::Local,
            &workdir,
        ));
        assert_eq!(program, "claude");
        assert_eq!(
            args,
            [
                "mcp",
                "add",
                "--transport",
                "http",
                "--scope",
                "local",
                "--client-id",
                "visdom-context-mcp",
                "bazaar",
                url
            ]
        );
        assert_eq!(
            cwd.as_deref(),
            Some(repo.join("src").as_path()),
            "local scope belongs to the project the command runs in"
        );

        let codex = adapter_for(Agent::Codex, paths(tmp.path()), None).unwrap();
        let (program, args, cwd) = args_of(&codex.register_mcp(
            "bazaar",
            url,
            "visdom-context-mcp",
            McpScope::Local,
            &workdir,
        ));
        assert_eq!(program, "codex");
        assert_eq!(
            args,
            [
                "mcp",
                "add",
                "bazaar",
                "--url",
                url,
                "--oauth-client-id",
                "visdom-context-mcp"
            ]
        );
        assert_eq!(cwd, None, "Codex has one global configuration");

        // A user-scope registration stays a user-scope one.
        let (_, args, _) =
            args_of(&claude.register_mcp("visdom", url, "c", McpScope::User, &workdir));
        assert_eq!(args[4..6], ["--scope", "user"]);
    }

    #[test]
    fn remove_commands_exact_args() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Path::new("/work/repo");
        let workdir = Workdir::checkout(repo);
        let entry = |scope| McpEntry {
            name: "visdom".to_string(),
            url: None,
            oauth_client_id: None,
            scope,
            extra_settings: false,
            disabled: false,
        };
        let claude = adapter_for(Agent::ClaudeCode, paths(tmp.path()), None).unwrap();
        let (_, args, cwd) = args_of(&claude.remove_mcp(&entry(McpScope::User), &workdir));
        assert_eq!(args, ["mcp", "remove", "--scope", "user", "visdom"]);
        assert_eq!(cwd.as_deref(), Some(repo));
        let (_, args, _) = args_of(&claude.remove_mcp(&entry(McpScope::Local), &workdir));
        assert_eq!(args, ["mcp", "remove", "--scope", "local", "visdom"]);

        let codex = adapter_for(Agent::Codex, paths(tmp.path()), None).unwrap();
        let (_, args, _) = args_of(&codex.remove_mcp(&entry(McpScope::User), &workdir));
        assert_eq!(args, ["mcp", "remove", "visdom"]);
    }

    #[test]
    fn launch_passes_args_through() {
        let tmp = tempfile::tempdir().unwrap();
        let args = ["--model".to_string(), "x".to_string()];

        let claude = adapter_for(Agent::ClaudeCode, paths(tmp.path()), None).unwrap();
        let cmd = claude.launch(&args);
        assert_eq!(cmd.get_program(), "claude");
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["--model", "x"]);

        let codex = adapter_for(
            Agent::Codex,
            paths(tmp.path()),
            Some("/opt/codex".to_string()),
        )
        .unwrap();
        let cmd = codex.launch(&args);
        assert_eq!(cmd.get_program(), "/opt/codex");
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["--model", "x"]);
        // The override also names the program of the registration command.
        let (program, ..) = args_of(&codex.register_mcp(
            "bazaar",
            "https://x/mcp",
            "c",
            McpScope::User,
            &Workdir::plain("/tmp"),
        ));
        assert_eq!(program, "/opt/codex");
    }

    #[test]
    fn agents_visdom_cannot_launch_have_no_adapter() {
        let tmp = tempfile::tempdir().unwrap();
        for agent in [Agent::Gsd, Agent::OpenCode, Agent::Amp] {
            assert!(adapter_for(agent, paths(tmp.path()), None).is_none());
        }
    }
}
