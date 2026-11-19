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
use visdom_cli::gateway::{ensure_key, key_store_path_in, KeyStore};
use visdom_cli::mode::{configured_mode, ModeReason, TracingMode, TrafficPolicy, MODE_ENV};
use visdom_cli::suite::PolicyState;
use visdom_cli::{config, interactive};
use visdom_cli::{doctor as checks, env_alias, launch};

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
                init_mode(&runtime, agent, cli.mode);
            }
        }
        Some(VisdomCommand::Doctor {
            fix,
            json,
            yes,
            agent,
            mode,
        }) => doctor(
            &runtime,
            agent.or(cli.agent),
            mode.or(cli.mode),
            fix,
            yes || cli.yes,
            json,
        ),
        Some(VisdomCommand::GatewayKey { console, project }) => {
            gateway_key(&runtime, &console, &project)
        }
        None => check_and_launch(runtime, cli.agent, cli.mode, cli.yes, &cli.agent_args),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `visdom gateway-key`: the key a launched agent asks for. The kept key is
/// printed without asking anybody; only a missing or expiring one costs a
/// request. Nothing but the key goes to stdout.
fn gateway_key(runtime: &tokio::runtime::Runtime, console: &str, project: &str) -> ! {
    let env = Environment::current();
    let store = KeyStore::at(key_store_path_in(&env.config_root));
    if let Some(key) = store.usable(console, project, now()) {
        println!("{}", key.secret);
        exit(0);
    }
    let issued = checks::person_token(&env.cwd, runtime).and_then(|token| {
        runtime.block_on(ensure_key(
            &launch::key_http_client(),
            console,
            &token,
            project,
            &store,
            now(),
        ))
    });
    match issued {
        Ok((key, warning)) => {
            if let Some(warning) = warning {
                eprintln!("visdom: warning: {warning}");
            }
            println!("{}", key.secret);
            exit(0)
        }
        Err(e) => {
            eprintln!("visdom: {e}");
            exit(1)
        }
    }
}

fn gather(
    env: &Environment,
    agent: Option<Agent>,
    mode: Option<TracingMode>,
    runtime: &tokio::runtime::Runtime,
) -> Session {
    match Session::gather(env, agent, mode, runtime) {
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
    mode: Option<TracingMode>,
    fix_flag: bool,
    yes: bool,
    json: bool,
) -> ! {
    let env = Environment::current();
    let mut session = gather(&env, agent, mode, runtime);
    let mut findings = session.collect();
    let policy = policy_for(fix_flag, yes, true, stdin_is_tty());
    if policy != FixPolicy::Never && findings.iter().any(|f| f.fix.is_some()) {
        // What is wrong, before asking to change it.
        print_attention(&findings);
        if fix(&findings, policy, &mut RealExecutor::new(json, policy)).applied {
            session = gather(&env, agent, mode, runtime);
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
    let session = gather(&env, Some(args.agent), None, runtime);
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
    let mut session = gather(&env, Some(agent), None, runtime);
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
        let mut after = gather(&env, Some(agent), None, runtime);
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

/// The last part of `visdom init`: how the developer's sessions are traced.
/// Asked only when it is theirs to choose — nothing chosen yet, a gateway to
/// choose, and no workspace policy deciding it — and remembered in their
/// config. A policy that decides is said instead.
fn init_mode(runtime: &tokio::runtime::Runtime, agent: Agent, flag: Option<TracingMode>) {
    let env = Environment::current();
    let session = gather(&env, Some(agent), flag, runtime);
    match session.mode.reason {
        ModeReason::Policy(TrafficPolicy::Enforced) => {
            println!(
                "This workspace requires the AI Gateway: `visdom` starts the agent through it."
            );
            return;
        }
        ModeReason::Policy(_) | ModeReason::Chosen => return,
        ModeReason::Default => {}
    }
    let user = config::load_user_config_in(&env.config_root).unwrap_or_default();
    let gateway_known = env.gateway_url.is_some()
        || user.gateway.url.is_some()
        || session.suite.ai_gateway_url.is_some();
    let free_choice = matches!(
        session.policy,
        PolicyState::Fresh(TrafficPolicy::PerAgent)
            | PolicyState::Remembered(TrafficPolicy::PerAgent, _)
    );
    if !gateway_known || !free_choice || !interactive::is_interactive() {
        return;
    }
    // Asked again here only to rule out a choice made outside the user config.
    let repo = git_toplevel(&env.cwd)
        .and_then(|dir| config::load_repo_config_in(&dir).ok())
        .unwrap_or_default();
    if !matches!(
        configured_mode(
            flag,
            env.mode.as_deref(),
            repo.tracing_mode,
            user.tracing_mode
        ),
        Ok(None)
    ) {
        return;
    }
    let Some(mode) = interactive::ask_mode() else {
        return;
    };
    match interactive::save_tracing_mode(&session.user_config_path, mode) {
        Ok(()) => eprintln!("Saved to {}", session.user_config_path.display()),
        Err(e) => eprintln!("visdom: warning: tracing mode not saved: {e}"),
    }
}

/// The agent's command for this session: through the AI Gateway when that is
/// the mode and the gateway is ready. A gateway that is not ready stops the
/// launch only when the workspace requires it; a developer who merely chose
/// it gets their agent, traced by hooks.
fn agent_command(
    session: &Session,
    runtime: &tokio::runtime::Runtime,
    agent_args: &[String],
) -> std::process::Command {
    let hooks = |session: &Session| {
        let mut cmd = session.adapter.launch(agent_args);
        // Said outright, so a `gateway` left in the environment cannot switch
        // the hooks' capture off for a session that does not use the gateway.
        cmd.env(MODE_ENV, TracingMode::Hooks.name());
        cmd
    };
    if session.mode.mode != TracingMode::Gateway {
        return hooks(session);
    }
    let visdom = std::env::current_exe().unwrap_or_else(|_| "visdom".into());
    match launch::prepare_gateway(session, runtime, &visdom, now()) {
        Ok((gateway, notes)) => {
            for note in notes {
                eprintln!("visdom: warning: {note}");
            }
            session
                .adapter
                .launch_via_gateway(agent_args, &gateway, &|name| std::env::var(name).ok())
        }
        Err(why) => {
            // A gateway that could not be planned was already reported with
            // the findings; say here only what was not.
            if matches!(session.gateway, Some(Ok(_))) {
                eprintln!("visdom: {why}");
            }
            if matches!(
                session.mode.reason,
                ModeReason::Policy(TrafficPolicy::Enforced)
            ) {
                eprintln!(
                    "visdom: the workspace requires the AI Gateway, so the agent is not started"
                );
                exit(1);
            }
            eprintln!("visdom: starting the agent with hooks tracing instead");
            hooks(session)
        }
    }
}

/// Plain `visdom`: check, offer the fixes, start the agent. Findings never
/// stop the agent from starting; only a gateway the workspace requires and
/// that cannot be used does.
fn check_and_launch(
    runtime: tokio::runtime::Runtime,
    agent: Option<Agent>,
    mode: Option<TracingMode>,
    yes: bool,
    agent_args: &[String],
) -> ! {
    let env = Environment::current();
    let mut session = gather(&env, agent, mode, &runtime);
    let findings = session.collect();
    print_attention(&findings);
    let policy = policy_for(false, yes, false, stdin_is_tty());
    if fix(&findings, policy, &mut RealExecutor::new(false, policy)).applied {
        session = gather(&env, agent, mode, &runtime);
        // What is still open, once, above the agent.
        print_attention(&session.collect());
    } else if policy == FixPolicy::Never && findings.iter().any(|f| f.fix.is_some()) {
        eprintln!("visdom: run `visdom doctor --fix` (or `visdom --yes`) to apply the fixes");
    }
    // In the directory visdom was run in, like starting the agent by hand.
    let cmd = agent_command(&session, &runtime, agent_args);
    // The agent takes over this process: no runtime threads may outlive it.
    drop(runtime);
    launch::exec_agent(cmd, session.ctx.agent, &session.user_config_path)
}
