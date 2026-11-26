//! The `visdom` command line.

use clap::{Parser, Subcommand};
use tracevault_cli::agent::Agent;
use tracevault_cli::cli::{Command, InitArgs, LoginArgs};

use crate::billing::Billing;
use crate::mode::TracingMode;
use crate::surface::Surface;

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
    /// Who pays for the agent's calls through the AI Gateway: the project
    /// (its gateway key) or your Claude subscription (Claude Code only).
    /// Default: the VISDOM_GATEWAY_BILLING variable, then `billing` under
    /// [gateway] in `.visdom/config.toml`, then in the user config, then
    /// project. `visdom --billing … init` saves it.
    #[arg(long, value_enum)]
    pub billing: Option<Billing>,
    /// Where the agent opens: terminal or desktop (the Claude Desktop app).
    /// Default: the VISDOM_SURFACE variable, then `.visdom/config.toml` in the
    /// repository, then the user config (`surface`), then terminal.
    #[arg(long, value_enum)]
    pub surface: Option<Surface>,
    /// Apply every offered fix without asking.
    #[arg(long)]
    pub yes: bool,
    /// Start the agent without telling it about Visdom's tools (see `visdom
    /// instructions`). To leave them out always: `agent_instructions = false`
    /// in the user config.
    #[arg(long)]
    pub no_instructions: bool,
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
        /// Who pays through the gateway, to check (see `visdom --help`).
        #[arg(long, value_enum)]
        billing: Option<Billing>,
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
        /// The kind of key: an ordinary personal key (the project pays) or a
        /// subscription key (Claude Code on your claude.ai login).
        #[arg(long, value_enum, default_value_t = Billing::Project)]
        billing: Billing,
    },
    /// Print shell commands that send an agent started without `visdom`
    /// through the AI Gateway: `source <(visdom env)`. The key is not in the
    /// output, only the command that gets it.
    Env {
        /// The agent the commands are for (see `visdom --help`).
        #[arg(long, value_enum)]
        agent: Option<Agent>,
    },
    /// Print what an agent started here with `visdom` is told about Visdom's
    /// tools: the tracing commands, the bazaar MCP server, the AI Gateway —
    /// each only if this session has it. Prints nothing when it has none.
    Instructions {
        /// The agent the text is for (see `visdom --help`).
        #[arg(long, value_enum)]
        agent: Option<Agent>,
        /// The tracing mode it is for (see `visdom --help`).
        #[arg(long, value_enum)]
        mode: Option<TracingMode>,
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
    fn the_surface_is_an_option_of_the_launch() {
        assert_eq!(
            parse(&["visdom", "--surface", "desktop"]).surface,
            Some(Surface::Desktop)
        );
        assert!(VisdomCli::try_parse_from(["visdom", "--surface", "web"]).is_err());
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
    fn who_pays_is_an_option_of_the_launch_init_and_doctor() {
        let cli = parse(&[
            "visdom",
            "--mode",
            "gateway",
            "--billing",
            "subscription",
            "init",
        ]);
        assert_eq!(cli.billing, Some(Billing::Subscription));
        assert!(matches!(cli.command, Some(VisdomCommand::Init { .. })));
        let cli = parse(&["visdom", "doctor", "--billing", "project"]);
        assert!(matches!(
            cli.command,
            Some(VisdomCommand::Doctor {
                billing: Some(Billing::Project),
                ..
            })
        ));
        assert!(VisdomCli::try_parse_from(["visdom", "--billing", "free"]).is_err());
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
            Some(VisdomCommand::GatewayKey {
                console,
                project,
                billing,
            }) => {
                assert_eq!(
                    (console.as_str(), project.as_str(), billing),
                    ("https://gw.example.com", "p-1", Billing::Project)
                );
            }
            _ => panic!("expected gateway-key"),
        }
        let cli = parse(&[
            "visdom",
            "gateway-key",
            "--console",
            "https://gw.example.com",
            "--project",
            "p-1",
            "--billing",
            "subscription",
        ]);
        assert!(matches!(
            cli.command,
            Some(VisdomCommand::GatewayKey {
                billing: Billing::Subscription,
                ..
            })
        ));
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
                mode: None,
                billing: None
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
