//! The `visdom` command line.

use clap::{Parser, Subcommand};
use tracevault_cli::agent::Agent;
use tracevault_cli::cli::{Command, InitArgs, LoginArgs};

#[derive(Parser)]
#[command(
    name = "visdom",
    version,
    about = "Visdom developer CLI",
    long_about = "Visdom developer CLI.\n\nWithout a subcommand: checks that this project is wired \
                  for the coding agent (tracing, bazaar MCP), offers to fix what is not, and \
                  launches the agent. Arguments after `--` go to the agent."
)]
pub struct VisdomCli {
    /// The agent to check and launch: claude-code or codex. Default: the
    /// VISDOM_AGENT variable, then `.visdom/config.toml` in the repository,
    /// then the user config, then claude-code.
    #[arg(long, value_enum)]
    pub agent: Option<Agent>,
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
    },
    /// Log in to Visdom
    Login(LoginArgs),
    /// Log out from Visdom
    Logout,
    /// Set up tracing in this repository, then check the rest of the wiring
    /// (bazaar MCP) and offer to fix it.
    Init {
        #[command(flatten)]
        args: InitArgs,
        /// Apply every offered fix without asking.
        #[arg(long)]
        yes: bool,
    },
    /// Tracing commands (everything `tracevault` does).
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
                agent: None
            })
        ));
    }

    #[test]
    fn init_takes_tracevault_init_flags_and_yes() {
        let cli = parse(&["visdom", "init", "--agent", "codex", "--yes"]);
        match cli.command {
            Some(VisdomCommand::Init { args, yes }) => {
                assert!(yes);
                assert_eq!(args.agent, Agent::Codex);
            }
            _ => panic!("expected init"),
        }
    }
}
