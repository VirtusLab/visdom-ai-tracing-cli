use std::io::IsTerminal;
use std::process::exit;

use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches};
use tracevault_cli::agent::Agent;
use tracevault_cli::cli::{run, Command, InitArgs};
use visdom_cli::agents::McpScope;
use visdom_cli::billing::{self, Billing, BillingUse};
use visdom_cli::cli::{VisdomCli, VisdomCommand};
use visdom_cli::components::CredentialState;
use visdom_cli::doctor::git_toplevel;
use visdom_cli::doctor::{fix, print_attention, Environment, RealExecutor, Session};
use visdom_cli::finding::FixAction;
use visdom_cli::finding::{exit_code, render_json, render_text};
use visdom_cli::fixes::FixExecutor;
use visdom_cli::fixes::{policy_for, FixPolicy};
use visdom_cli::gateway::{ensure_key, key_store_path_in, KeyStore};
use visdom_cli::instructions::Told;
use visdom_cli::interactive::InitScope;
use visdom_cli::mode::{
    configured_mode, ModeReason, TracingMode, TrafficPolicy, GATEWAY_SESSION_ENV, MODE_ENV,
};
use visdom_cli::suite::PolicyState;
use visdom_cli::{config, interactive};
use visdom_cli::{doctor as checks, env_alias, instructions, launch};

fn main() {
    // Before the runtime starts its threads: this changes the environment.
    env_alias::apply();
    // What this binary installs for an agent or git to run calls `visdom`.
    tracevault_cli::program::run_as_visdom();
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
        Some(VisdomCommand::Logout) => {
            // First: a failed logout exits.
            forget_gateway_keys();
            runtime.block_on(run(Command::Logout))
        }
        Some(VisdomCommand::Trace { command }) => runtime.block_on(run(command)),
        Some(VisdomCommand::Init { mut args }) => {
            // A repository, the whole machine or a folder of checkouts: said
            // with a flag, else asked. Whichever it is, the rest of the setup
            // (login, bazaar, the tracing mode, the gateway) is the same.
            let scope = init_scope(&args);
            let typed = init_agent_typed.then_some(args.agent).or(cli.agent);
            args.agent = init_agent(typed);
            let agent = args.agent;
            let launched = matches!(agent, Agent::ClaudeCode | Agent::Codex);
            if launched {
                init_login(&runtime, &args);
            }
            // A workspace has no hooks of its own: chosen from the question,
            // the machine-wide ones are installed with it.
            let also_machine =
                (scope.scope == InitScope::Workspace && scope.asked).then(|| machine_wide(&args));
            match scope.scope {
                InitScope::Repo => {}
                InitScope::Machine => args.global = true,
                InitScope::Workspace => init_workspace_args(&mut args),
            }
            // Exits on failure.
            runtime.block_on(run(Command::Init(args)));
            if let Some(machine) = also_machine {
                runtime.block_on(run(Command::Init(machine)));
            }
            if launched {
                init_bazaar(&runtime, agent, scope.scope);
                init_mode(&runtime, agent, cli.mode);
                init_gateway(&runtime, agent, cli.mode, cli.billing, scope.scope);
            }
        }
        Some(VisdomCommand::Doctor {
            fix,
            json,
            yes,
            agent,
            mode,
            billing,
        }) => doctor(
            &runtime,
            agent.or(cli.agent),
            mode.or(cli.mode),
            billing.or(cli.billing),
            fix,
            yes || cli.yes,
            json,
        ),
        Some(VisdomCommand::GatewayKey { console, project }) => {
            gateway_key(&runtime, &console, &project)
        }
        Some(VisdomCommand::Env { agent }) => {
            env_script(&runtime, agent.or(cli.agent), cli.billing)
        }
        Some(VisdomCommand::Instructions { agent, mode }) => {
            let session = gather(
                &Environment::current(),
                agent.or(cli.agent),
                mode.or(cli.mode),
                cli.billing,
                &runtime,
            );
            // Through the gateway when that is the mode and it can be used:
            // what a launch from here would do.
            let through_gateway = matches!(session.gateway, Some(Ok(_)));
            if let Some(text) = instructions::compose(&session.facts(through_gateway)) {
                print!("{text}");
            }
        }
        None => check_and_launch(
            runtime,
            cli.agent,
            cli.mode,
            cli.billing,
            cli.yes,
            !cli.no_instructions,
            &cli.agent_args,
        ),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Give the key to the agent that asked for it, which reads this command's
/// output: the key is that output, written as such and to nowhere else. A
/// reader that has gone away is a failure, not a panic.
fn hand_over(secret: &str) -> ! {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let written = out
        .write_all(secret.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush());
    exit(if written.is_ok() { 0 } else { 1 })
}

/// `visdom gateway-key`: the key a launched agent asks for. The kept key is
/// printed without asking anybody; only a missing or expiring one costs a
/// request. Nothing but the key goes to stdout.
fn gateway_key(runtime: &tokio::runtime::Runtime, console: &str, project: &str) -> ! {
    let env = Environment::current();
    let store = KeyStore::at(key_store_path_in(&env.config_root));
    let kept = checks::stored_account(&env.cwd)
        .and_then(|account| store.usable(console, project, &account, now()));
    if let Some(key) = kept {
        hand_over(&key.secret);
    }
    let issued = checks::person_token(&env.cwd, runtime).and_then(|token| {
        runtime.block_on(ensure_key(
            &launch::key_http_client(),
            console,
            &token,
            project,
            &store,
            now(),
            None,
        ))
    });
    match issued {
        Ok(ensured) => hand_over(&ensured.key.secret),
        Err(e) => {
            eprintln!("visdom: {e}");
            exit(1)
        }
    }
}

/// `visdom logout`: the gateway keys kept for this login go with it.
fn forget_gateway_keys() {
    let env = Environment::current();
    if let Err(e) = KeyStore::at(key_store_path_in(&env.config_root)).clear() {
        eprintln!("visdom: warning: the kept AI Gateway keys could not be removed: {e}");
    }
}

fn gather(
    env: &Environment,
    agent: Option<Agent>,
    mode: Option<TracingMode>,
    billing: Option<Billing>,
    runtime: &tokio::runtime::Runtime,
) -> Session {
    match Session::gather(env, agent, mode, billing, runtime) {
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
    billing: Option<Billing>,
    fix_flag: bool,
    yes: bool,
    json: bool,
) -> ! {
    let env = Environment::current();
    let mut session = gather(&env, agent, mode, billing, runtime);
    let mut findings = session.collect();
    let policy = policy_for(fix_flag, yes, true, stdin_is_tty());
    if policy != FixPolicy::Never && findings.iter().any(|f| f.fix.is_some()) {
        // What is wrong, before asking to change it.
        print_attention(&findings);
        if fix(&findings, policy, &mut RealExecutor::new(json, policy)).applied {
            session = gather(&env, agent, mode, billing, runtime);
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
    let repo = checks::settings_dir(&env.cwd)
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

/// What `init` is to set up, and whether somebody was asked.
struct ScopeChoice {
    scope: InitScope,
    asked: bool,
}

/// The scope of this `init`: `--global` or `--workspace` when given, the
/// repository when an option only a repository has is given or there is
/// nobody to ask, else the answer to the question — whose default is the
/// repository inside a checkout and the machine outside one.
fn init_scope(args: &InitArgs) -> ScopeChoice {
    let said = |scope| ScopeChoice {
        scope,
        asked: false,
    };
    if args.global {
        return said(InitScope::Machine);
    }
    if args.workspace {
        return said(InitScope::Workspace);
    }
    let repo_only = args.claude_settings.is_some() || args.no_gitignore;
    if repo_only || !interactive::is_interactive() {
        return said(InitScope::Repo);
    }
    let default = match git_toplevel(&Environment::current().cwd) {
        Some(_) => InitScope::Repo,
        None => InitScope::Machine,
    };
    match interactive::ask_scope(default) {
        Some(scope) => ScopeChoice { scope, asked: true },
        None => exit(1),
    }
}

/// `args` as a `--workspace` init: the project it records to is asked for
/// when not given. Options a workspace does not take stop here, said as they
/// would be on the command line.
fn init_workspace_args(args: &mut InitArgs) {
    let refused = [
        ("--claude-settings", args.claude_settings.is_some()),
        ("--no-gitignore", args.no_gitignore),
        ("--no-user-context", args.no_user_context),
        ("--user-context", args.user_context.is_some()),
        ("--claude-telemetry", args.claude_telemetry),
        ("--no-claude-telemetry", args.no_claude_telemetry),
    ];
    if let Some((option, _)) = refused.iter().find(|(_, given)| *given) {
        eprintln!("visdom: {option} cannot be used for a workspace");
        exit(2);
    }
    if args.project.is_none() {
        args.project = interactive::ask_text("Project the workspace's sessions are recorded to:");
    }
    if args.project.is_none() {
        eprintln!("visdom: a workspace needs a project: pass --workspace --project <name>");
        exit(2);
    }
    args.workspace = true;
}

/// `args` as the `--global` init that goes with a workspace.
fn machine_wide(args: &InitArgs) -> InitArgs {
    let mut machine = args.clone();
    machine.global = true;
    machine.workspace = false;
    machine.project = None;
    // Of a repository only.
    machine.server_url = None;
    machine
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
    let session = gather(&env, Some(args.agent), None, None, runtime);
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
fn init_bazaar(runtime: &tokio::runtime::Runtime, agent: Agent, scope: InitScope) {
    let env = Environment::current();
    // For a repository the registration is that project directory's; a
    // machine or a folder of checkouts has sessions in many directories.
    let mcp_scope = match scope {
        InitScope::Repo => McpScope::Local,
        InitScope::Machine | InitScope::Workspace => McpScope::User,
    };
    let mut session = gather(&env, Some(agent), None, None, runtime);
    session.mcp_scope = mcp_scope;
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
        let mut after = gather(&env, Some(agent), None, None, runtime);
        after.mcp_scope = mcp_scope;
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
    let session = gather(&env, Some(agent), flag, None, runtime);
    match session.mode.reason {
        ModeReason::Policy(TrafficPolicy::Enforced) => {
            println!(
                "This workspace requires the AI Gateway: `visdom` starts the agent through it."
            );
            return;
        }
        ModeReason::Policy(_) => return,
        ModeReason::Chosen => {
            // `visdom --mode … init` is a choice to keep, not one for this
            // run only: there is no agent to start here.
            if let Some(mode) = flag {
                save_mode(&session, mode);
                // Kept in the user config, which the environment and the
                // repository's config come before: a mode they name is the
                // one the next plain `visdom` runs in, not this one.
                let user = config::load_user_config_in(&env.config_root).unwrap_or_default();
                let repo = checks_repo_config(&env);
                let next = configured_mode(
                    None,
                    env.mode.as_deref(),
                    repo.tracing_mode,
                    user.tracing_mode,
                );
                if !matches!(next, Ok(Some(next)) if next == mode) {
                    let said_by = match env.mode {
                        Some(_) => format!("{MODE_ENV} in the environment"),
                        None => "tracing_mode in the repository's .visdom/config.toml".to_string(),
                    };
                    let other = match next {
                        Ok(Some(next)) => format!("`{}`", next.name()),
                        _ => "something else".to_string(),
                    };
                    eprintln!(
                        "visdom: `{mode}` was saved to your config, but {said_by} says {other} \
                         and comes first: a plain `visdom` here would not start in {mode} mode. \
                         Change it there, or pass --mode {mode} each time.",
                        mode = mode.name()
                    );
                    exit(1);
                }
            }
            return;
        }
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
    let repo = checks::settings_dir(&env.cwd)
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
    save_mode(&session, mode);
}

/// The repository's own visdom config for the directory `visdom` runs in.
fn checks_repo_config(env: &Environment) -> config::RepoConfig {
    checks::settings_dir(&env.cwd)
        .and_then(|dir| config::load_repo_config_in(&dir).ok())
        .unwrap_or_default()
}

fn save_mode(session: &Session, mode: TracingMode) {
    match interactive::save_tracing_mode(&session.user_config_path, mode) {
        Ok(()) => eprintln!(
            "Saved tracing_mode = \"{}\" to {}",
            mode.name(),
            session.user_config_path.display()
        ),
        Err(e) => {
            // The next `visdom` would not know the choice: not a setup done.
            eprintln!("visdom: the tracing mode could not be saved: {e}");
            eprintln!("  Set it yourself: export {MODE_ENV}={}", mode.name());
            exit(1);
        }
    }
}

/// Write a gateway setting init has settled to the config at `path`. One
/// that cannot be written ends init: the next `visdom` would not find it,
/// and the setup would only look done.
fn save_gateway(path: &std::path::Path, key: &str, value: &str) {
    match interactive::save_gateway_setting(path, key, value) {
        Ok(()) => eprintln!(
            "Saved {key} = \"{value}\" under [gateway] to {}",
            path.display()
        ),
        Err(e) => {
            eprintln!("visdom: the AI Gateway is not set up: {key} could not be saved: {e}");
            eprintln!(
                "  Set it yourself: {}",
                launch::gateway_setting_env(key).unwrap_or(key)
            );
            exit(1);
        }
    }
}

/// The end of `visdom init` when sessions go through the AI Gateway (chosen
/// just now, earlier, or required by the workspace): leave it ready to use.
/// The project and the model alias are settled — asked for when there is a
/// choice and somebody to ask — and written to the user config, a personal
/// key is issued and tried on the LLM gateway, and what was set up is shown
/// with where each setting comes from. A gateway that cannot be made ready
/// makes init fail, with what to set.
fn init_gateway(
    runtime: &tokio::runtime::Runtime,
    agent: Agent,
    flag: Option<TracingMode>,
    billing_flag: Option<Billing>,
    scope: InitScope,
) {
    let env = Environment::current();
    // The gateway project of a folder of checkouts is that folder's, kept in
    // its own `.visdom/config.toml`; otherwise it is the developer's.
    // (A workspace may be a checkout's top level; the file is there then.)
    let workspace_dir = (scope == InitScope::Workspace)
        .then(|| git_toplevel(&env.cwd).unwrap_or_else(|| env.cwd.clone()));
    // Whether the folder names its project itself already: one inherited
    // from the developer's own config is not the folder's, and would follow
    // every later change of theirs.
    let workspace_names_project = workspace_dir.as_deref().is_some_and(|dir| {
        config::load_repo_config_in(dir)
            .is_ok_and(|folder| folder.gateway.project.is_some_and(|p| !p.trim().is_empty()))
    });
    let workspace_file = workspace_dir.as_deref().map(config::repo_config_path_in);
    let for_workspace = workspace_file.is_some();
    let project_file =
        &workspace_file.unwrap_or_else(|| config::user_config_path_in(&env.config_root));
    let mut session = gather(&env, Some(agent), flag, billing_flag, runtime);
    if let BillingUse::Rejected(why) = &session.billing_use {
        eprintln!("visdom: {why}");
        exit(2);
    }
    if session.mode.mode != TracingMode::Gateway {
        return;
    }
    let asking = interactive::is_interactive();
    // Right after the mode: who pays for Claude Code's calls through it.
    if agent == Agent::ClaudeCode && init_billing(&session, billing_flag, asking) {
        session = gather(&env, Some(agent), flag, billing_flag, runtime);
    }
    // Several projects: the choice is the developer's.
    if asking && session.gateway_sources.project.is_none() {
        let open = match (&session.console_url, &session.token) {
            (Some(console), Some(token)) => runtime
                .block_on(visdom_cli::gateway::open_projects(
                    &checks::http_client(),
                    console,
                    token,
                ))
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        if open.len() > 1 {
            let names: Vec<String> = open
                .iter()
                .map(|p| format!("{} ({})", p.slug, p.name))
                .collect();
            if let Some(i) =
                interactive::ask_choice("Which AI Gateway project is this for?", &names)
            {
                save_gateway(project_file, "project", &open[i].slug);
                session = gather(&env, Some(agent), flag, billing_flag, runtime);
            }
        }
    }
    let visdom = std::env::current_exe().unwrap_or_else(|_| "visdom".into());
    let mut prepared = match launch::prepare_gateway(&session, runtime, &visdom, now()) {
        Ok(prepared) => prepared,
        Err(why) => {
            eprintln!("visdom: the AI Gateway is not ready: {why}");
            eprintln!(
                "{}",
                launch::gateway_settings_hint(&session.user_config_path)
            );
            exit(1);
        }
    };
    let Some(Ok(plan)) = &session.gateway else {
        return;
    };
    let mut sources = session.gateway_sources.clone();
    let in_user_config = |key: &str| {
        Some(format!(
            "{key} under [gateway] in {}",
            session.user_config_path.display()
        ))
    };
    if sources.project.is_none() || (for_workspace && !workspace_names_project) {
        // The only project open today: written down, so that being given
        // access to another one later does not leave the choice open.
        save_gateway(project_file, "project", &plan.project.slug);
        sources.project = Some(format!(
            "project under [gateway] in {}",
            project_file.display()
        ));
        // A folder's new config file comes before one further up, which may
        // be where the gateway mode was chosen: said in the new file too, or
        // the next `visdom` here would start with hooks.
        let chosen_in_files = session.mode.reason == ModeReason::Chosen && flag.is_none();
        if for_workspace
            && chosen_in_files
            && gather(&env, Some(agent), None, None, runtime).mode.mode != TracingMode::Gateway
        {
            if let Err(e) = interactive::save_tracing_mode(project_file, TracingMode::Gateway) {
                eprintln!("visdom: the tracing mode could not be saved: {e}");
                exit(1);
            }
            eprintln!(
                "Saved tracing_mode = \"gateway\" to {}",
                project_file.display()
            );
        }
    }
    let model_key = format!("{}_model", config::agent_config_key(agent));
    if sources.model.is_none() {
        // Nobody named a model: the one picked for the agent, else theirs to
        // pick. Written down either way, so it does not change by itself.
        let picked = prepared.launch.model.clone().or_else(|| {
            asking
                .then(|| {
                    interactive::ask_choice(
                        &format!(
                            "Which model alias should {} ask the AI Gateway for?",
                            agent.label()
                        ),
                        &prepared.available,
                    )
                })
                .flatten()
                .map(|i| prepared.available[i].clone())
        });
        if let Some(model) = picked {
            save_gateway(&session.user_config_path, &model_key, &model);
            sources.model = in_user_config(&model_key);
            prepared.launch.model = Some(model);
        }
    }
    if let Err(why) = launch::usable_model(
        agent,
        prepared.launch.model.as_deref(),
        sources.model.as_deref(),
        &plan.project.slug,
        &prepared.available,
    ) {
        eprintln!("visdom: the AI Gateway is not ready: {why}");
        exit(1);
    }
    print!(
        "{}",
        launch::gateway_summary(
            agent,
            &session.mode,
            plan,
            &sources,
            prepared.launch.model.as_deref(),
            &session.billing,
            &session.user_config_path,
        )
    );
}

/// Who pays for Claude Code's calls through the gateway, in `visdom init`:
/// `--billing` is saved to the user config without asking; with nothing
/// chosen anywhere and somebody to ask, the answer is. A saved value that the
/// environment or the repository's config overrules stops init, as for the
/// mode. `true` when something was saved.
fn init_billing(session: &Session, flag: Option<Billing>, asking: bool) -> bool {
    let billing = match flag {
        Some(billing) => billing,
        None if session.billing.source == billing::BillingSource::Default && asking => {
            match interactive::ask_billing() {
                Some(billing) => billing,
                None => return false,
            }
        }
        None => return false,
    };
    let path = &session.user_config_path;
    save_gateway(path, "billing", billing.name());
    let env = Environment::current();
    let user = config::load_user_config_in(&env.config_root).unwrap_or_default();
    let repo = checks_repo_config(&env);
    let next = billing::configured_billing(
        None,
        env.gateway_billing.as_deref(),
        repo.gateway.billing,
        user.gateway.billing,
        &path.display().to_string(),
    );
    match next {
        Ok(next) if next.billing == billing => true,
        next => {
            let said_by = match next {
                Ok(next) => next.describe(),
                Err(e) => e,
            };
            eprintln!(
                "visdom: billing = \"{billing}\" was saved to your config, but {said_by} comes \
                 first: a plain `visdom` here would not use it. Change it there, or pass \
                 --billing {billing} each time."
            );
            exit(1);
        }
    }
}

/// `visdom env`: the shell commands for an agent started without `visdom`.
/// Only they go to stdout.
fn env_script(
    runtime: &tokio::runtime::Runtime,
    agent: Option<Agent>,
    billing_flag: Option<Billing>,
) -> ! {
    let env = Environment::current();
    // Asking for these is asking for the gateway, whatever the mode at rest.
    let session = gather(
        &env,
        agent,
        Some(TracingMode::Gateway),
        billing_flag,
        runtime,
    );
    if session.mode.mode != TracingMode::Gateway {
        eprintln!(
            "visdom: the workspace's AI traffic policy keeps sessions off the AI Gateway: \
             there is nothing to set"
        );
        exit(1);
    }
    stop_on_billing(&session);
    let visdom = std::env::current_exe().unwrap_or_else(|_| "visdom".into());
    let script = launch::prepare_gateway(&session, runtime, &visdom, now()).and_then(|prepared| {
        let project = match &session.gateway {
            Some(Ok(plan)) => plan.project.slug.clone(),
            _ => String::new(),
        };
        // Nothing checks the gateway again before an agent started by hand:
        // a setup that would ask for a model the project refuses is not one
        // to hand out.
        launch::usable_model(
            session.ctx.agent,
            prepared.launch.model.as_deref(),
            session.gateway_sources.model.as_deref(),
            &project,
            &prepared.available,
        )?;
        session.adapter.env_script(&prepared.launch, &project)
    });
    match script {
        Ok(script) => {
            print!("{script}");
            exit(0)
        }
        Err(why) => {
            eprintln!("visdom: the AI Gateway is not ready: {why}");
            eprintln!(
                "{}",
                launch::gateway_settings_hint(&session.user_config_path)
            );
            exit(1)
        }
    }
}

/// The agent's arguments with what it is told about Visdom's tools. Where
/// that is left to a configuration file of the agent's, the developer is told
/// which, once.
fn told_args(session: &Session, agent_args: &[String], through_gateway: bool) -> Vec<String> {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    match session.agent_args(agent_args, through_gateway) {
        Told::With(args) | Told::Nothing(args) => args,
        Told::LeftTo { file, args } => {
            if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!(
                    "visdom: {} sets {}'s developer instructions, and visdom does not replace \
                     them: the agent is not told about Visdom's tools. `visdom instructions` \
                     prints the text to add there.",
                    file.display(),
                    session.ctx.agent.label()
                );
            }
            args
        }
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
        let mut cmd = session
            .adapter
            .launch(&told_args(session, agent_args, false));
        // Said outright, so nothing left in the environment (this may be an
        // agent's own shell, inside a gateway session) can switch the hooks'
        // capture off for a session that does not use the gateway.
        cmd.env(MODE_ENV, TracingMode::Hooks.name());
        cmd.env_remove(GATEWAY_SESSION_ENV);
        cmd
    };
    if session.mode.mode != TracingMode::Gateway {
        return hooks(session);
    }
    stop_on_billing(session);
    let visdom = std::env::current_exe().unwrap_or_else(|_| "visdom".into());
    let prepared = launch::prepare_gateway(session, runtime, &visdom, now()).and_then(|prepared| {
        session
            .adapter
            .launch_via_gateway(
                &told_args(session, agent_args, true),
                &prepared.launch,
                &|name| std::env::var(name).ok(),
            )
            .map(|cmd| (cmd, prepared.notes))
    });
    match prepared {
        Ok((cmd, notes)) => {
            for note in notes {
                eprintln!("visdom: warning: {note}");
            }
            cmd
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

/// Stop before an agent is started through the gateway in a way the billing
/// does not allow: a combination that cannot work (exit 2, like a mode that
/// names none), or the subscription, which this version cannot start the
/// agent on yet — rather than let the project pay without being asked to.
fn stop_on_billing(session: &Session) {
    if let BillingUse::Rejected(why) = &session.billing_use {
        eprintln!("visdom: {why}");
        exit(2);
    }
    if session.billing_use == BillingUse::Applies {
        if let Some(why) = billing::launch_unavailable(&session.billing) {
            eprintln!("visdom: {why}");
            exit(1);
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
    billing: Option<Billing>,
    yes: bool,
    instructions: bool,
    agent_args: &[String],
) -> ! {
    let env = Environment::current();
    let mut session = gather(&env, agent, mode, billing, &runtime);
    let findings = session.collect();
    print_attention(&findings);
    let policy = policy_for(false, yes, false, stdin_is_tty());
    if fix(&findings, policy, &mut RealExecutor::new(false, policy)).applied {
        session = gather(&env, agent, mode, billing, &runtime);
        // What is still open, once, above the agent.
        print_attention(&session.collect());
    } else if policy == FixPolicy::Never && findings.iter().any(|f| f.fix.is_some()) {
        eprintln!("visdom: run `visdom doctor --fix` (or `visdom --yes`) to apply the fixes");
    }
    // After the fixes: what was just set up is what the agent is told about.
    session.agent_instructions &= instructions;
    // A billing that cannot be used here (a subscription outside the
    // gateway, or for Codex) is a setting to correct, in hooks mode too. Why
    // was said with the findings above.
    if matches!(session.billing_use, BillingUse::Rejected(_)) {
        eprintln!("visdom: the agent is not started: correct the billing (gateway.billing above)");
        exit(2);
    }
    // In the directory visdom was run in, like starting the agent by hand.
    let cmd = agent_command(&session, &runtime, agent_args);
    // The agent takes over this process: no runtime threads may outlive it.
    drop(runtime);
    launch::exec_agent(cmd, session.ctx.agent, &session.user_config_path)
}
