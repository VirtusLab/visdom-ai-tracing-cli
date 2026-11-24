//! The `visdom` command line.

use clap::{Parser, Subcommand};
use tracevault_cli::agent::Agent;
use tracevault_cli::cli::{Command, InitArgs, LoginArgs};

use crate::mode::TracingMode;

#[derive(Parser)]
#[command(
    name = "visdom",
    version,
    about = "Visdom developer CLI",
    long_about = "Visdom developer CLI.\n\nWithout a subcommand: checks that this project is wired \
                  for the coding agent (tracing, bazaar MCP), offers to fix what is not, and \
                  launches the agent — with hooks tracing, or through the AI Gateway (--mode). \
                  Arguments after `--` go to the agent."
)]
pub struct VisdomCli {
    /// The agent to check and launch: claude-code or codex. Default: the
    /// VISDOM_AGENT variable, then `.visdom/config.toml` in the repository,
    /// then the user config, then claude-code.
    #[arg(long, value_enum)]
    pub agent: Option<Agent>,
    /// How the session is traced: by hooks, or through the AI Gateway.
    /// Default: the VISDOM_TRACING_MODE variable, then `.visdom/config.toml`
    /// in the repository, then the user config (`tracing_mode`), then hooks.
    /// The workspace's AI traffic policy may require one of them.
    #[arg(long, value_enum)]
    pub mode: Option<TracingMode>,
    /// Apply every offered fix without asking.
    #[arg(long)]
    pub yes: bool,
    #[command(subcommand)]
    pub command: Option<VisdomCommand>,
    /// Arguments passed to the agent unchanged.
    #[arg(last = true)]
    pub agent_args: Vec<String>,
}

#[derive(Subcommand)]
pub enum VisdomCommand {
    /// Check this project's Visdom wiring without launching an agent. Exits
    /// with 1 when a check fails.
    Doctor {
        /// Offer the fixes (asking for each; with --yes, apply all).
        #[arg(long)]
        fix: bool,
        /// Print the findings as JSON.
        #[arg(long)]
        json: bool,
        /// With --fix: apply every fix without asking.
        #[arg(long)]
        yes: bool,
        /// The agent to check (see `visdom --help`).
        #[arg(long, value_enum)]
        agent: Option<Agent>,
        /// The tracing mode to check (see `visdom --help`).
        #[arg(long, value_enum)]
        mode: Option<TracingMode>,
    },
    /// Print the developer's AI Gateway key for a project: what a launched
    /// agent runs to get, and renew, its key.
    #[command(hide = true)]
    GatewayKey {
        /// The gateway console's address.
        #[arg(long)]
        console: String,
        /// The gateway project's id.
        #[arg(long)]
        project: String,
    },
    /// Print shell commands that send an agent started without `visdom`
    /// through the AI Gateway: `source <(visdom env)`. The key is not in the
    /// output, only the command that gets it.
    Env {
        /// The agent the commands are for (see `visdom --help`).
        #[arg(long, value_enum)]
        agent: Option<Agent>,
    },
    /// Log in to Visdom
    Login(LoginArgs),
    /// Log out from Visdom
    Logout,
    /// Set up this repository for Visdom: tracing, and
    /// the bazaar MCP server for the agent.
    Init {
        #[command(flatten)]
        args: InitArgs,
    },
    /// Tracing commands: status, check, repo, project, context and the rest.
    Trace {
        #[command(subcommand)]
        command: Command,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> VisdomCli {
        match VisdomCli::try_parse_from(args) {
            Ok(cli) => cli,
            Err(e) => panic!("{args:?} must parse: {e}"),
        }
    }

    #[test]
    fn the_mode_is_an_option_of_the_launch_and_of_doctor() {
        let cli = parse(&["visdom", "--mode", "gateway", "--", "--verbose"]);
        assert_eq!(cli.mode, Some(TracingMode::Gateway));
        assert_eq!(cli.agent_args, ["--verbose"]);
        let cli = parse(&["visdom", "doctor", "--mode", "hooks"]);
        assert!(matches!(
            cli.command,
            Some(VisdomCommand::Doctor {
                mode: Some(TracingMode::Hooks),
                ..
            })
        ));
        assert!(VisdomCli::try_parse_from(["visdom", "--mode", "proxy"]).is_err());
    }

    #[test]
    fn the_key_helper_takes_a_console_and_a_project() {
        let cli = parse(&[
            "visdom",
            "gateway-key",
            "--console",
            "https://gw.example.com",
            "--project",
            "p-1",
        ]);
        match cli.command {
            Some(VisdomCommand::GatewayKey { console, project }) => {
                assert_eq!(
                    (console.as_str(), project.as_str()),
                    ("https://gw.example.com", "p-1")
                );
            }
            _ => panic!("expected gateway-key"),
        }
    }

    #[test]
    fn command_definition_is_consistent() {
        use clap::CommandFactory;
        VisdomCli::command().debug_assert();
    }

    #[test]
    fn parses_trace_status() {
        let cli = parse(&["visdom", "trace", "status"]);
        assert!(matches!(
            cli.command,
            Some(VisdomCommand::Trace {
                command: Command::Status { .. }
            })
        ));
    }

    #[test]
    fn parses_hidden_trace_stream() {
        let cli = parse(&["visdom", "trace", "stream", "--event", "stop"]);
        assert!(matches!(
            cli.command,
            Some(VisdomCommand::Trace {
                command: Command::Stream { .. }
            })
        ));
    }

    #[test]
    fn bare_visdom_with_agent_args() {
        let cli = parse(&["visdom", "--agent", "codex", "--", "--model", "x"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.agent, Some(Agent::Codex));
        assert_eq!(cli.agent_args, ["--model", "x"]);
    }

    #[test]
    fn bare_visdom_without_arguments() {
        let cli = parse(&["visdom"]);
        assert!(cli.command.is_none());
        assert!(cli.agent.is_none() && !cli.yes && cli.agent_args.is_empty());
    }

    #[test]
    fn parses_doctor_json() {
        let cli = parse(&["visdom", "doctor", "--json"]);
        assert!(matches!(
            cli.command,
            Some(VisdomCommand::Doctor {
                json: true,
                fix: false,
                yes: false,
                agent: None,
                mode: None
            })
        ));
    }

    #[test]
    fn init_takes_tracevault_init_flags() {
        let cli = parse(&["visdom", "init", "--agent", "codex", "--no-gitignore"]);
        match cli.command {
            Some(VisdomCommand::Init { args }) => {
                assert_eq!(args.agent, Agent::Codex);
                assert!(args.no_gitignore);
            }
            _ => panic!("expected init"),
        }
    }
}
