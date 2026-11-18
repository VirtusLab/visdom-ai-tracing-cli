use std::io::IsTerminal;
use std::process::exit;

use clap::Parser;
use tracevault_cli::agent::Agent;
use tracevault_cli::cli::{run, Command};
use visdom_cli::cli::{VisdomCli, VisdomCommand};
use visdom_cli::doctor::{fix, print_attention, Environment, RealExecutor, Session};
use visdom_cli::finding::{exit_code, render_json, render_text};
use visdom_cli::fixes::{policy_for, FixPolicy};
use visdom_cli::{env_alias, launch};

fn main() {
    // Before the runtime starts its threads: this changes the environment.
    env_alias::apply();
    let cli = VisdomCli::parse();
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
        Some(VisdomCommand::Init { args, yes }) => {
            let agent = args.agent;
            // `--global` and `--workspace` set up no single repository.
            let per_repo = !args.global && !args.workspace;
            // Exits on failure.
            runtime.block_on(run(Command::Init(args)));
            if per_repo && matches!(agent, Agent::ClaudeCode | Agent::Codex) {
                check_after_init(&runtime, agent, yes || cli.yes);
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
        if fix(&findings, policy, &mut RealExecutor::new(json)) {
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

fn check_after_init(runtime: &tokio::runtime::Runtime, agent: Agent, yes: bool) {
    let env = Environment::current();
    let session = gather(&env, Some(agent), runtime);
    let findings = session.collect();
    print_attention(&findings);
    let policy = policy_for(false, yes, false, stdin_is_tty());
    if fix(&findings, policy, &mut RealExecutor::new(false)) {
        print_attention(&gather(&env, Some(agent), runtime).collect());
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
    if fix(&findings, policy, &mut RealExecutor::new(false)) {
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
