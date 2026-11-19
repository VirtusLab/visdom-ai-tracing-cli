use std::io::IsTerminal;
use std::process::exit;

use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches};
use tracevault_cli::agent::Agent;
use tracevault_cli::cli::{run, Command, InitArgs};
use visdom_cli::cli::{VisdomCli, VisdomCommand};
use visdom_cli::components::CredentialState;
use visdom_cli::doctor::git_toplevel;
use visdom_cli::doctor::{fix, print_attention, Environment, RealExecutor, Session};
use visdom_cli::finding::FixAction;
use visdom_cli::finding::{exit_code, render_json, render_text};
use visdom_cli::fixes::FixExecutor;
use visdom_cli::fixes::{policy_for, FixPolicy};
use visdom_cli::{config, interactive};
use visdom_cli::{env_alias, launch};

fn main() {
    // Before the runtime starts its threads: this changes the environment.
    env_alias::apply();
    let matches = VisdomCli::command().get_matches();
    let cli = VisdomCli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    // `init --agent` has a default, so only the matches know whether it was typed.
    let init_agent_typed = matches
        .subcommand_matches("init")
        .is_some_and(|init| init.value_source("agent") == Some(ValueSource::CommandLine));
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("visdom: cannot start: {e}");
            exit(1);
        }
    };
    match cli.command {
        Some(VisdomCommand::Login(args)) => runtime.block_on(run(Command::Login(args))),
        Some(VisdomCommand::Logout) => runtime.block_on(run(Command::Logout)),
        Some(VisdomCommand::Trace { command }) => runtime.block_on(run(command)),
        Some(VisdomCommand::Init { mut args }) => {
            // `--global` and `--workspace` set up no single repository: they
            // are `tracevault init` as it is.
            let per_repo = !args.global && !args.workspace;
            if per_repo {
                let typed = init_agent_typed.then_some(args.agent).or(cli.agent);
                args.agent = init_agent(typed);
                if matches!(args.agent, Agent::ClaudeCode | Agent::Codex) {
                    init_login(&runtime, &args);
                }
            }
            let agent = args.agent;
            // Exits on failure.
            runtime.block_on(run(Command::Init(args)));
            if per_repo && matches!(agent, Agent::ClaudeCode | Agent::Codex) {
                init_bazaar(&runtime, agent);
            }
        }
        Some(VisdomCommand::Doctor {
            fix,
            json,
            yes,
            agent,
        }) => doctor(&runtime, agent.or(cli.agent), fix, yes || cli.yes, json),
        None => check_and_launch(runtime, cli.agent, cli.yes, &cli.agent_args),
    }
}

fn gather(env: &Environment, agent: Option<Agent>, runtime: &tokio::runtime::Runtime) -> Session {
    match Session::gather(env, agent, runtime) {
        Ok(session) => {
            for warning in &session.warnings {
                eprintln!("visdom: warning: {warning}");
            }
            session
        }
        Err(e) => {
            eprintln!("visdom: {e}");
            exit(2);
        }
    }
}

fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}

fn doctor(
    runtime: &tokio::runtime::Runtime,
    agent: Option<Agent>,
    fix_flag: bool,
    yes: bool,
    json: bool,
) -> ! {
    let env = Environment::current();
    let mut session = gather(&env, agent, runtime);
    let mut findings = session.collect();
    let policy = policy_for(fix_flag, yes, true, stdin_is_tty());
    if policy != FixPolicy::Never && findings.iter().any(|f| f.fix.is_some()) {
        // What is wrong, before asking to change it.
        print_attention(&findings);
        if fix(&findings, policy, &mut RealExecutor::new(json, policy)).applied {
            session = gather(&env, agent, runtime);
            findings = session.collect();
        }
    }
    if json {
        println!("{}", render_json(&findings));
    } else {
        print!("{}", render_text(&findings));
    }
    exit(exit_code(&findings))
}

/// The agent `visdom init` sets up: the one typed, else the one configured
/// (`VISDOM_AGENT`, repository config, user config), else — with somebody to
/// ask — the one they pick, else Claude Code.
fn init_agent(typed: Option<Agent>) -> Agent {
    if let Some(agent) = typed {
        return agent;
    }
    let env = Environment::current();
    let user = config::load_user_config_in(&env.config_root).unwrap_or_default();
    let repo = git_toplevel(&env.cwd)
        .and_then(|dir| config::load_repo_config_in(&dir).ok())
        .unwrap_or_default();
    match config::configured_agent(env.agent.as_deref(), &repo, &user) {
        Ok(Some(agent)) => agent,
        Ok(None) if interactive::is_interactive() => interactive::ask_agent(),
        Ok(None) => Agent::ClaudeCode,
        Err(e) => {
            eprintln!("visdom: {e}");
            exit(2);
        }
    }
}

/// Before `init`, with somebody to ask: when not logged in, ask for the
/// tracing server (unless `--server-url` or an earlier setup names it) and log
/// in, instead of failing with "log in first". Without a terminal nothing
/// happens here and `init` reports what is missing.
fn init_login(runtime: &tokio::runtime::Runtime, args: &InitArgs) {
    if !interactive::is_interactive() {
        return;
    }
    let env = Environment::current();
    let session = gather(&env, Some(args.agent), runtime);
    if session.ctx.credential != CredentialState::Missing {
        return;
    }
    let known = args.server_url.clone().or(session.ctx.server_url);
    let Some(server_url) = known.or_else(|| {
        eprintln!("You are not logged in to Visdom.");
        interactive::ask_url("Tracing server URL:")
    }) else {
        // Nothing given: `init` says what is needed.
        return;
    };
    let login = FixAction::Login { server_url };
    if let Err(e) = RealExecutor::new(false, FixPolicy::Ask).execute(&login) {
        eprintln!("visdom: {e}");
        exit(1);
    }
}

/// The second half of `visdom init`: set up bazaar for the agent. Asking for
/// `init` is asking for this, so the registration is shown and applied, not
/// offered. What init cannot do (an unknown bazaar URL, an entry it will not
/// replace) is reported; a registration that fails makes init fail.
fn init_bazaar(runtime: &tokio::runtime::Runtime, agent: Agent) {
    let env = Environment::current();
    let mut session = gather(&env, Some(agent), runtime);
    // Nobody knows the bazaar URL: ask, and remember the answer for next time.
    if matches!(session.bazaar_url, Ok(None)) && interactive::is_interactive() {
        if let Some(url) = interactive::ask_url("Bazaar MCP URL (Enter to skip):") {
            match interactive::save_bazaar_url(&session.user_config_path, &url) {
                Ok(()) => eprintln!("Saved to {}", session.user_config_path.display()),
                Err(e) => eprintln!("visdom: warning: bazaar URL not saved: {e}"),
            }
            session.bazaar_url = Ok(Some(url));
        }
    }
    let findings = session.collect();
    let bazaar: Vec<_> = findings
        .iter()
        .filter(|f| f.component == "bazaar")
        .cloned()
        .collect();
    let policy = FixPolicy::All;
    let summary = fix(&bazaar, policy, &mut RealExecutor::new(false, policy));
    let findings = if summary.applied {
        let mut after = gather(&env, Some(agent), runtime);
        // The URL just typed counts even if it could not be saved.
        if matches!(after.bazaar_url, Ok(None)) {
            after.bazaar_url = session.bazaar_url.clone();
        }
        after.collect()
    } else {
        findings
    };
    print_attention(&findings);
    if summary.failed {
        exit(1);
    }
    if findings
        .iter()
        .any(|f| f.id == "bazaar.mcp" && f.severity == visdom_cli::finding::Severity::Ok)
    {
        println!("Bazaar MCP registered for {}", agent.label());
    }
}

/// Plain `visdom`: check, offer the fixes, start the agent. Findings never
/// stop the agent from starting.
fn check_and_launch(
    runtime: tokio::runtime::Runtime,
    agent: Option<Agent>,
    yes: bool,
    agent_args: &[String],
) -> ! {
    let env = Environment::current();
    let mut session = gather(&env, agent, &runtime);
    let findings = session.collect();
    print_attention(&findings);
    let policy = policy_for(false, yes, false, stdin_is_tty());
    if fix(&findings, policy, &mut RealExecutor::new(false, policy)).applied {
        session = gather(&env, agent, &runtime);
        // What is still open, once, above the agent.
        print_attention(&session.collect());
    } else if policy == FixPolicy::Never && findings.iter().any(|f| f.fix.is_some()) {
        eprintln!("visdom: run `visdom doctor --fix` (or `visdom --yes`) to apply the fixes");
    }
    // The agent takes over this process: no runtime threads may outlive it.
    drop(runtime);
    // In the directory visdom was run in, like starting the agent by hand.
    let cmd = session.adapter.launch(agent_args);
    launch::exec_agent(cmd, session.ctx.agent, &session.user_config_path)
}
