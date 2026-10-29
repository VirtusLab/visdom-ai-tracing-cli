use clap::Parser;
use std::env;

mod agent;
mod api_client;
mod auto_bind;
mod billing;
mod commands;
mod config;
mod context;
mod credentials;
mod hooks;
mod install_state;
mod oidc;
mod paths;
mod resolution;
mod session_state;
mod subagents;
#[cfg(test)]
mod test_helpers;
mod transcript;
mod user_default;
mod user_project_default;

use commands::project::ProjectCmd;
use commands::repo::RepoCmd;

#[derive(Parser)]
#[command(name = "tracevault", version, about = "AI code governance platform")]
enum Cli {
    /// Initialize TraceVault in current repository
    Init {
        /// TraceVault server URL for repo registration
        #[arg(long)]
        server_url: Option<String>,
        /// Where to install Claude Code hooks: `shared` (.claude/settings.json,
        /// typically committed) or `local` (.claude/settings.local.json,
        /// personal/git-ignored). When omitted, prompts interactively if stdin
        /// is a TTY, otherwise defaults to `shared`.
        #[arg(long, value_enum)]
        claude_settings: Option<commands::init::ClaudeSettingsTarget>,
        /// Skip updating .gitignore. Use this when your project manages
        /// .gitignore separately or you want to commit the Claude settings files.
        #[arg(long)]
        no_gitignore: bool,
        /// Disable the cross-repo user-level context for this project. Also
        /// applies to `--global`, where it disables the user-level context
        /// instead of the per-repo one.
        #[arg(long)]
        no_user_context: bool,
        /// Enable the user-level context and read it from this explicit path
        /// (conflicts with --no-user-context). Also applies to `--global`,
        /// where it sets the path the user-level context is read from.
        #[arg(long, conflicts_with = "no_user_context")]
        user_context: Option<String>,
        /// Install TraceVault once for ALL sessions of the chosen --agent on
        /// this machine, instead of initializing the current repo: Claude Code
        /// hooks into ~/.claude/ (settings.json + CLAUDE.md), Codex hooks into
        /// ~/.codex/ (hooks.json + AGENTS.md), the GSD extension via
        /// `gsd install` (global), or the OpenCode plugin into the user config
        /// dir's opencode/plugins/. Does not require git. Also writes a
        /// user-level context config (honoring --no-user-context /
        /// --user-context). Conflicts with the per-repo-only flags
        /// --server-url, --claude-settings and --no-gitignore.
        #[arg(
            long,
            conflicts_with_all = ["server_url", "claude_settings", "no_gitignore"]
        )]
        global: bool,
        /// Which agent to install hooks for. The default installs Claude Code
        /// hooks (.claude/settings.json or settings.local.json); `codex` writes
        /// .codex/hooks.json; `gsd` (alias `gsd2`) registers the GSD extension
        /// with `gsd install -l`; `opencode` writes
        /// .opencode/plugins/tracevault.ts. With --global, the user-level
        /// equivalents instead.
        #[arg(long, value_enum, default_value = "claude-code")]
        agent: crate::agent::Agent,
        /// Export Claude Code's own per-request cost to TraceVault over
        /// OpenTelemetry (the default): writes the OTel log exporter settings
        /// and an `otelHeadersHelper` (`tracevault otel-headers`) into the
        /// USER-level ~/.claude/settings.json, per-repo or with --global. An
        /// existing exporter setup is left untouched (collector instructions
        /// are printed instead). Without either flag, an interactive init asks
        /// (default yes), and a non-interactive one enables it unless you
        /// opted out before.
        #[arg(long, conflicts_with = "no_claude_telemetry")]
        claude_telemetry: bool,
        /// Opt out of Claude Code telemetry: removes TraceVault's entries from
        /// ~/.claude/settings.json and records the choice, so neither a later
        /// `init` nor a CLI upgrade turns it back on.
        #[arg(long)]
        no_claude_telemetry: bool,
    },
    /// Claude Code telemetry (cost over OpenTelemetry, on by default): `on`,
    /// `off` (remembered across upgrades) or `status`.
    Telemetry {
        #[arg(value_enum)]
        action: TelemetryAction,
    },
    /// Print the Authorization header for Claude Code's OpenTelemetry export,
    /// as JSON, from the TraceVault credential store. Installed as
    /// `otelHeadersHelper` by `tracevault init --claude-telemetry` — not
    /// intended to be run manually.
    #[command(name = "otel-headers", hide = true)]
    OtelHeaders {
        /// The server telemetry is exported to: the credential must be for it.
        #[arg(long)]
        server_url: Option<String>,
    },
    /// Show current session status
    Status {
        /// Session to inspect for the workspace binding; defaults to
        /// $TRACEVAULT_SESSION_ID, else the most recent session that has a
        /// workspace binding.
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Stream hook events to server in real-time.
    /// Installed into .claude/settings.json by `tracevault init` and invoked
    /// by Claude Code on every tool event — not intended to be run manually.
    #[command(hide = true)]
    Stream {
        #[arg(long)]
        event: String,
        /// Which agent this hook belongs to (sets the request's tool + protocol
        /// version). Defaults to claude-code so existing installs are unchanged.
        #[arg(long, value_enum, default_value = "claude-code")]
        agent: crate::agent::Agent,
    },
    /// SessionStart hook: exports the session id, binds an unbound session to
    /// the repo of its checkout's origin remote, and injects the bound repo's
    /// policies as additionalContext. Installed by `tracevault init` (per-repo
    /// or --global) into the Claude Code settings or the Codex hooks.json —
    /// not intended to be run manually.
    #[command(name = "session-start", hide = true)]
    SessionStart,
    /// UserPromptSubmit hook: reinforces the bound repo's policies as
    /// additionalContext when the effective repo has changed since the last
    /// injection. Installed by `tracevault init` (per-repo or --global) into
    /// the Claude Code settings or the Codex hooks.json — not intended to be
    /// run manually.
    #[command(name = "user-prompt", hide = true)]
    UserPrompt,
    /// Check session policies before pushing
    Check,
    /// Sync repo remote URL with the TraceVault server
    Sync,
    /// Show local session statistics
    Stats,
    /// Log in to a TraceVault server
    Login {
        /// TraceVault server URL
        #[arg(long)]
        server_url: String,
        /// Do not try to open a browser; just print the URL.
        /// Useful inside Docker / CI / SSH without X11.
        #[arg(long)]
        no_browser: bool,
    },
    /// Log out from the TraceVault server
    Logout,
    /// Push commit metadata to the server.
    /// Installed into .git/hooks/post-commit by `tracevault init` and
    /// invoked by git after every commit — not intended to be run manually.
    #[command(hide = true)]
    CommitPush,
    /// Force-sync all pending events to server
    Flush,
    /// Verify commits are registered and sealed on the TraceVault server
    Verify {
        /// Comma-separated list of commit SHAs
        #[arg(long)]
        commits: Option<String>,
        /// Git commit range (e.g. abc1234..def5678)
        #[arg(long)]
        range: Option<String>,
    },
    /// Open (or re-open) a verification phase for the current session.
    ///
    /// A verification phase declares that the agent has finished making changes
    /// and is now running quality checks. Only tool calls made after this point
    /// are evaluated by verification_phase-scoped policies. Calling this again
    /// resets the phase, discarding earlier verification events.
    ///
    /// In single-agent setups the current session is detected automatically.
    /// In multi-agent setups, pass --session-id to target the correct session.
    #[command(name = "verify-start")]
    VerifyStart {
        /// Explicit session ID to open the window for. When omitted, the
        /// invoking session ($TRACEVAULT_SESSION_ID) is used if it has events in
        /// this worktree, else the session is detected by worktree.
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Print agent-readable instructions derived from active policies.
    ///
    /// Fetches the rendered instructions from the server for the current repo
    /// and prints them to stdout. Designed to be invoked from CLAUDE.md (or
    /// equivalent) at session start so the agent's behaviour matches the
    /// policies configured on the TraceVault server.
    #[command(name = "agent-policies")]
    AgentPolicies,
    /// LLM proxy commands.
    Proxy {
        #[command(subcommand)]
        cmd: ProxyCmd,
    },
    /// Manage the active context (flow ID, labels, params) attached to events.
    Context {
        #[command(subcommand)]
        action: ContextAction,
    },
    /// Bind/inspect the repo a detached session is working on (workspace mode).
    Repo {
        #[command(subcommand)]
        cmd: RepoCmd,
    },
    /// Bind/inspect the project a detached session is attributed to.
    Project {
        #[command(subcommand)]
        cmd: ProjectCmd,
    },
}

/// Sub-actions for `tracevault context`.
#[derive(clap::Subcommand)]
enum ContextAction {
    /// Set the context, replacing any previously saved context entirely.
    ///
    /// In a linked worktree the per-worktree file is written by default.
    /// In the primary checkout the global file is written.
    /// Use --global to force the global file from anywhere.
    ///
    /// Omitted dimensions are left empty. This clears labels and params that
    /// are not re-specified.
    Set {
        /// Active flow ID to associate with events
        #[arg(long)]
        flow: Option<String>,
        /// Labels to attach (repeatable; comma-separated values accepted)
        #[arg(long)]
        label: Vec<String>,
        /// Params to attach, in key=value form (repeatable)
        #[arg(long)]
        param: Vec<String>,
        /// Write to the global context file regardless of worktree scope
        #[arg(long)]
        global: bool,
        /// Write to the resolved user-context file instead (wins over --global)
        #[arg(long)]
        user: bool,
    },
    /// Update the existing context, merging changes in.
    ///
    /// In a linked worktree the per-worktree file is updated by default.
    /// Use --global to force the global file from anywhere.
    ///
    /// Sets --flow if provided; unions --label with existing; inserts/overwrites
    /// --param keys; removes --remove-label / --remove-param entries.
    Update {
        /// New flow ID (replaces current flow if provided)
        #[arg(long)]
        flow: Option<String>,
        /// Labels to add (repeatable; comma-separated values accepted)
        #[arg(long)]
        label: Vec<String>,
        /// Params to add or overwrite, in key=value form (repeatable)
        #[arg(long)]
        param: Vec<String>,
        /// Labels to remove (repeatable; comma-separated values accepted)
        #[arg(long = "remove-label")]
        remove_label: Vec<String>,
        /// Param keys to remove (repeatable)
        #[arg(long = "remove-param")]
        remove_param: Vec<String>,
        /// Write to the global context file regardless of worktree scope
        #[arg(long)]
        global: bool,
        /// Write to the resolved user-context file instead (wins over --global)
        #[arg(long)]
        user: bool,
    },
    /// Show the current context, layer by layer (pretty JSON).
    ///
    /// Prints up to four sections: User (always; its header says whether the
    /// user layer is enabled and which file it reads), Global (repo-wide),
    /// This worktree (linked only), and Effective (the merged result that the
    /// hook stamps, each value annotated with the layer it came from).
    Show,
    /// Clear all context (flow, labels, params).
    ///
    /// In a linked worktree the per-worktree file is cleared by default.
    /// Use --global to force clearing the global file from anywhere.
    Clear {
        /// Clear the global context file regardless of worktree scope
        #[arg(long)]
        global: bool,
        /// Clear the resolved user-context file instead (wins over --global)
        #[arg(long)]
        user: bool,
    },
    /// Enable/disable or point the project's user-level context source.
    ///
    /// The mode flags are mutually exclusive: at most one may be given.
    #[command(group(clap::ArgGroup::new("source_mode").multiple(false)))]
    Source {
        /// Enable the user context at the default path
        #[arg(long, group = "source_mode")]
        enable: bool,
        /// Disable the user context
        #[arg(long, group = "source_mode")]
        disable: bool,
        /// Enable and read the user context from this explicit path
        #[arg(long, group = "source_mode")]
        path: Option<String>,
        /// Enable and clear any explicit path back to the default location
        #[arg(long, group = "source_mode")]
        default: bool,
    },
}

#[derive(clap::Subcommand)]
enum ProxyCmd {
    /// Print the proxy URL and setup instructions for AI tools (Claude Code,
    /// GSD2, Cursor, Codex CLI, etc.).
    Info,
}

/// Write the user-level context config during a `--global` init and print the
/// standard summary. Every agent's `--global` arm ends with this identical
/// bootstrap, so it lives in one place. Exits the process on a fatal flag error,
/// mirroring the per-arm behavior it replaces.
fn finish_global_user_context(no_user_context: bool, user_context: Option<String>) {
    let requested = match config::UserContext::from_init_flags(no_user_context, user_context) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };
    match commands::init::write_global_user_config_in(&config::tv_config_root(), requested) {
        Ok(active) => {
            println!(
                "User-level context config: {}",
                config::user_config_path().display()
            );
            match active {
                Some(ctx) => println!("User-level context file: {}", ctx.display()),
                None => println!("User-level context is disabled (`--no-user-context`)."),
            }
            println!(
                "Edit it with `tracevault context set --user …`; disable with \
                 `tracevault init --global --no-user-context`."
            );
        }
        Err(e) => eprintln!("Warning: could not write user-level context config: {e}"),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum TelemetryAction {
    On,
    Off,
    Status,
}

/// What the user asked for Claude Code telemetry.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TelemetryChoice {
    /// `--claude-telemetry` / `telemetry on`.
    On,
    /// `--no-claude-telemetry` / `telemetry off`.
    Off,
    /// Neither flag: on, unless the user opted out before (VIS-1109).
    Default,
}

impl TelemetryChoice {
    fn from_flags(on: bool, off: bool) -> Self {
        match (on, off) {
            (true, _) => Self::On,
            (_, true) => Self::Off,
            _ => Self::Default,
        }
    }
}

/// Record in the install state that this machine's telemetry is settled at
/// this version, so the upgrade step does not redo it. Best-effort.
fn record_telemetry_choice(opt_out: Option<bool>) {
    let result = install_state::update_in(&config::tv_config_root(), |s| {
        s.configured_version = Some(install_state::CURRENT_VERSION.to_string());
        if let Some(opt_out) = opt_out {
            s.claude_telemetry_opt_out = opt_out;
            s.telemetry_auto_enabled = None;
        }
    });
    if let Err(e) = result {
        eprintln!("Warning: could not record the Claude Code telemetry choice: {e}");
    }
}

/// Configure Claude Code telemetry after a successful `init` (or for
/// `telemetry on|off`). On by default (VIS-1109): `Default` enables it
/// unless the user opted out before, asking first in a terminal. `Off`
/// removes our settings and records the opt-out, which only an explicit `On`
/// clears. `Default` never turns on telemetry while Claude Code is set to
/// export content (prompt text etc.). A failure exits non-zero only when the
/// user asked explicitly; an existing exporter setup is not a failure (it is
/// left alone, by design).
fn finish_claude_telemetry(
    choice: TelemetryChoice,
    project_root: &std::path::Path,
    server_flag: Option<&str>,
) {
    use commands::claude_telemetry::{content_logging_in, is_off, prompt, Environment};
    let Some(claude_dir) = dirs::home_dir().map(|h| h.join(".claude")) else {
        eprintln!("Error: cannot determine home directory");
        std::process::exit(1);
    };
    let config_root = config::tv_config_root();
    let env = Environment::current(&claude_dir);
    let explicit = choice != TelemetryChoice::Default;
    let content = || content_logging_in(&claude_dir, &env);
    // Ask before taking the lock below: the answer may take a while.
    let mut choice = choice;
    if choice == TelemetryChoice::Default
        && !install_state::load_in(&config_root).claude_telemetry_opt_out
        && is_off(&claude_dir)
        && content().is_empty()
        && std::io::IsTerminal::is_terminal(&std::io::stdin())
        && !prompt().unwrap_or(true)
    {
        choice = TelemetryChoice::Off;
    }
    // The upgrade step changes the same settings and state (VIS-1109).
    let _lock = match install_state::Lock::take(&config_root, std::time::Duration::from_secs(10)) {
        Ok(lock) => lock,
        Err(e) if explicit => {
            eprintln!("Error: Claude Code telemetry not changed: {e}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Warning: Claude Code telemetry not configured: {e}");
            return;
        }
    };
    let state = install_state::load_in(&config_root);
    let off = |claude_dir: &std::path::Path| match commands::claude_telemetry::uninstall(
        claude_dir, &env,
    ) {
        Ok(removed) => {
            if removed {
                println!(
                    "Claude Code telemetry is off (removed TraceVault's settings from {})",
                    claude_dir.join("settings.json").display()
                );
            } else {
                println!("Claude Code telemetry is off");
            }
            record_telemetry_choice(Some(true));
        }
        Err(e) => {
            eprintln!("Error: could not turn Claude Code telemetry off: {e}");
            std::process::exit(1);
        }
    };
    match choice {
        TelemetryChoice::Off => return off(&claude_dir),
        TelemetryChoice::Default if state.claude_telemetry_opt_out => {
            println!(
                "Claude Code telemetry stays off, as you chose earlier \
                 (`tracevault telemetry on` to enable it)"
            );
            return;
        }
        TelemetryChoice::Default if is_off(&claude_dir) && !content().is_empty() => {
            // Not recorded: nothing was decided.
            eprintln!(
                "Warning: Claude Code telemetry not turned on: Claude Code is set to export \
                 content ({}); run `tracevault telemetry on` to send it to TraceVault anyway",
                content().join("; ")
            );
            return;
        }
        _ => {}
    }
    let result = commands::claude_telemetry::telemetry_server_url(project_root, server_flag)
        .and_then(|url| {
            commands::claude_telemetry::install(&claude_dir, &url, &env)
                .map(|outcome| (url, outcome))
                .map_err(|e| e.to_string())
        });
    match result {
        Ok((url, commands::claude_telemetry::InstallOutcome::Configured)) => {
            println!(
                "Claude Code telemetry exports to {} (configured in {}; restart Claude Code to \
                 apply; `tracevault telemetry off` to disable)",
                commands::claude_telemetry::logs_endpoint(&url),
                claude_dir.join("settings.json").display()
            );
            record_telemetry_choice(Some(false));
        }
        Ok((url, commands::claude_telemetry::InstallOutcome::Foreign(reasons))) => {
            eprintln!(
                "{}",
                commands::claude_telemetry::collector_instructions(&url, &reasons)
            );
            record_telemetry_choice(explicit.then_some(false));
        }
        Err(e) if explicit => {
            eprintln!("Error: Claude Code telemetry not configured: {e}");
            std::process::exit(1);
        }
        // Not recorded: the upgrade step retries in a later session.
        Err(e) => eprintln!("Warning: Claude Code telemetry not configured: {e}"),
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli {
        Cli::Init {
            server_url,
            claude_settings,
            no_gitignore,
            no_user_context,
            user_context,
            global,
            agent,
            claude_telemetry,
            no_claude_telemetry,
        } => {
            if (claude_telemetry || no_claude_telemetry) && agent != crate::agent::Agent::ClaudeCode
            {
                eprintln!(
                    "Error: --claude-telemetry and --no-claude-telemetry only apply to \
                     --agent claude-code (use `tracevault telemetry off` instead)"
                );
                std::process::exit(1);
            }
            let telemetry = TelemetryChoice::from_flags(claude_telemetry, no_claude_telemetry);
            // Reject flag combinations that don't apply to the selected agent
            // (e.g. --claude-settings with --agent codex) rather than silently
            // ignoring them.
            if let Err(e) = commands::init::validate_init_flags(agent, claude_settings.is_some()) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
            if global {
                // Explicit match (not `matches!(.., Codex)` + fallthrough) so a
                // future agent variant can't silently fall through to the Claude
                // Code branch and report success while installing nothing for it.
                match agent {
                    crate::agent::Agent::Codex => {
                        let codex_dir = match dirs::home_dir() {
                            Some(home) => home.join(".codex"),
                            None => {
                                eprintln!("Error: cannot determine home directory");
                                std::process::exit(1);
                            }
                        };
                        match commands::init::install_global_codex_hooks(&codex_dir) {
                            Ok(()) => {
                                println!(
                                    "Installed TraceVault Codex hooks in {}",
                                    codex_dir.join("hooks.json").display()
                                );
                                println!(
                                    "Added workspace-mode instructions to {}",
                                    codex_dir.join("AGENTS.md").display()
                                );
                                println!(
                                    "These apply to ALL Codex CLI sessions on this machine, not just this repo."
                                );
                                println!("{}", commands::init::codex_hook_trust_notice());
                            }
                            Err(e) => {
                                eprintln!("Error: {e}");
                                std::process::exit(1);
                            }
                        }

                        finish_global_user_context(no_user_context, user_context);
                        return;
                    }
                    crate::agent::Agent::Gsd => {
                        // GSD (pi) loads extensions from a user-global
                        // ~/.gsd/extensions/ directory, so a global install
                        // just runs `gsd install` without `-l`, registering
                        // the extension for ALL GSD sessions on this
                        // machine rather than one repo.
                        match commands::init::install_gsd_extension(None) {
                            Ok(()) => {
                                println!(
                                    "This applies to ALL GSD sessions on this machine, not just this repo."
                                );
                            }
                            Err(e) => {
                                eprintln!("Error: {e}");
                                std::process::exit(1);
                            }
                        }

                        finish_global_user_context(no_user_context, user_context);
                        return;
                    }
                    crate::agent::Agent::ClaudeCode => {
                        let claude_dir = match dirs::home_dir() {
                            Some(home) => home.join(".claude"),
                            None => {
                                eprintln!("Error: cannot determine home directory");
                                std::process::exit(1);
                            }
                        };
                        match commands::init::install_global_hooks(&claude_dir) {
                            Ok(()) => {
                                println!(
                                    "Installed TraceVault hooks in {}",
                                    claude_dir.join("settings.json").display()
                                );
                                println!("Updated {}", claude_dir.join("CLAUDE.md").display());
                                println!(
                                    "These apply to ALL Claude Code sessions on this machine, not just this repo."
                                );
                            }
                            Err(e) => {
                                eprintln!("Error: {e}");
                                std::process::exit(1);
                            }
                        }

                        finish_global_user_context(no_user_context, user_context);
                        let cwd = env::current_dir().expect("Cannot determine current directory");
                        finish_claude_telemetry(telemetry, &cwd, None);
                        return;
                    }
                    crate::agent::Agent::OpenCode => {
                        // OpenCode auto-loads plugins from the user-global
                        // ~/.config/opencode/plugins/ directory, so a global
                        // install writes there directly, applying to ALL
                        // OpenCode sessions on this machine rather than one
                        // repo.
                        if let Err(e) = commands::init::install_opencode_plugin(None) {
                            eprintln!("Error: {e}");
                            std::process::exit(1);
                        }
                        println!(
                            "This applies to ALL OpenCode sessions on this machine, not just this repo."
                        );

                        finish_global_user_context(no_user_context, user_context);
                        return;
                    }
                }
            }

            let cwd = env::current_dir().expect("Cannot determine current directory");
            let user_context =
                match config::UserContext::from_init_flags(no_user_context, user_context) {
                    // `None`: no flag, so `init` keeps the stored value
                    // (on by default when there is none).
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("Error: {e}");
                        std::process::exit(1);
                    }
                };
            match commands::init::init_in_directory(
                &cwd,
                server_url.as_deref(),
                claude_settings,
                no_gitignore,
                user_context,
                agent,
            )
            .await
            {
                Ok(entry) => {
                    println!("TraceVault initialized in {}", cwd.display());
                    if entry.is_empty() {
                        // GSD (pi): the extension lives in the user-global
                        // ~/.gsd/extensions/tracevault/, not in the repo — there's
                        // no per-repo hook file to report or gitignore.
                        println!(
                            "{} extension installed (~/.gsd/extensions/tracevault/)",
                            agent.label()
                        );
                        println!("Git hooks installed (pre-push, post-commit)");
                        println!("Added .tracevault/ to .gitignore");
                    } else {
                        println!("{} hooks installed ({entry})", agent.label());
                        println!("Git hooks installed (pre-push, post-commit)");
                        println!("Added .tracevault/ and {entry} to .gitignore");
                    }
                    println!(
                        "Nothing needs to be committed — all TraceVault files are local only."
                    );
                    println!(
                        "Other contributors run `tracevault init` to set up their own local hooks."
                    );
                    if agent == crate::agent::Agent::ClaudeCode {
                        finish_claude_telemetry(telemetry, &cwd, server_url.as_deref());
                    }
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Cli::Status { session_id } => {
            let cwd = env::current_dir().expect("Cannot determine current directory");
            let project_root = crate::paths::resolve_project_root(&cwd).root;
            let effective = commands::status::effective_session_id(
                session_id,
                std::env::var("TRACEVAULT_SESSION_ID").ok(),
            );
            let code =
                commands::status::run_status(&project_root, &cwd, effective.as_deref()).await;
            if code != 0 {
                std::process::exit(code);
            }
        }
        Cli::OtelHeaders { server_url } => {
            let cwd = env::current_dir().expect("Cannot determine current directory");
            match commands::claude_telemetry::otel_headers(&cwd, server_url.as_deref()).await {
                Ok(headers) => println!("{headers}"),
                Err(e) => {
                    eprintln!("tracevault otel-headers: {e}");
                    std::process::exit(1);
                }
            }
        }
        Cli::Telemetry { action } => {
            let cwd = env::current_dir().expect("Cannot determine current directory");
            let project_root = crate::paths::resolve_project_root(&cwd).root;
            match action {
                TelemetryAction::On => {
                    finish_claude_telemetry(TelemetryChoice::On, &project_root, None)
                }
                TelemetryAction::Off => {
                    finish_claude_telemetry(TelemetryChoice::Off, &project_root, None)
                }
                TelemetryAction::Status => {
                    let server_url = crate::credentials::resolve_credentials_quiet(&project_root)
                        .ok()
                        .and_then(|(url, _)| url);
                    println!(
                        "{}",
                        commands::status::telemetry_status_line(server_url.as_deref())
                    );
                }
            }
        }
        Cli::Stream { event, agent } => {
            if let Err(e) = commands::stream::run_stream(&event, agent).await {
                eprintln!("Stream error: {e}");
            }
        }
        Cli::SessionStart => {
            // The hook itself always prints a valid HookOutput JSON payload
            // before returning; an Err here is a genuine last-resort case
            // (e.g. stdin unreadable) and — same as `Stream` — must never
            // turn into a non-zero exit, which would block the Claude Code
            // session from starting.
            if let Err(e) = commands::session_start::run().await {
                eprintln!("SessionStart error: {e}");
            }
        }
        Cli::UserPrompt => {
            // Same reasoning as `SessionStart`: this hook always prints a
            // valid HookOutput JSON payload before returning; an Err here
            // must never turn into a non-zero exit, which would block the
            // prompt from being submitted.
            if let Err(e) = commands::user_prompt::run().await {
                eprintln!("UserPromptSubmit error: {e}");
            }
        }
        Cli::Check => {
            let cwd = env::current_dir().expect("Cannot determine current directory");
            let project_root = crate::paths::resolve_project_root(&cwd).root;
            if let Err(e) = commands::check::check_policies(&project_root, &cwd).await {
                eprintln!("Check error: {e}");
                std::process::exit(1);
            }
        }
        Cli::Sync => {
            let cwd = env::current_dir().expect("Cannot determine current directory");
            let project_root = crate::paths::resolve_project_root(&cwd).root;
            if let Err(e) = commands::sync::sync_repo(&project_root).await {
                eprintln!("Sync error: {e}");
            }
        }
        Cli::Stats => {
            let cwd = env::current_dir().expect("Cannot determine current directory");
            let project_root = crate::paths::resolve_project_root(&cwd).root;
            if let Err(e) = commands::stats::show_stats(&project_root) {
                eprintln!("Stats error: {e}");
            }
        }
        Cli::Login {
            server_url,
            no_browser,
        } => {
            if let Err(e) = commands::login::login(&server_url, no_browser).await {
                eprintln!("Login error: {e}");
                // Non-zero exit matters here: a scripted `tracevault login &&
                // ...`, and above all the "account has no `tracevault`
                // grant" case (which saves credentials but is NOT a usable
                // login), must not look like success.
                std::process::exit(1);
            }
        }
        Cli::Logout => {
            if let Err(e) = commands::logout::logout().await {
                eprintln!("Logout error: {e}");
                std::process::exit(1);
            }
        }
        Cli::CommitPush => {
            let cwd = env::current_dir().expect("Cannot determine current directory");
            let project_root = crate::paths::resolve_project_root(&cwd).root;
            if let Err(e) = commands::commit_push::run_commit_push(&project_root, &cwd).await {
                eprintln!("Commit push error: {e}");
            }
        }
        Cli::Flush => {
            let cwd = env::current_dir().expect("Cannot determine current directory");
            let project_root = crate::paths::resolve_project_root(&cwd).root;
            if let Err(e) = commands::flush::run_flush(&project_root).await {
                eprintln!("Flush error: {e}");
            }
        }
        Cli::Verify { commits, range } => {
            let cwd = env::current_dir().expect("Cannot determine current directory");
            let project_root = crate::paths::resolve_project_root(&cwd).root;
            if let Err(e) =
                commands::verify::verify(&project_root, &cwd, commits.as_deref(), range.as_deref())
                    .await
            {
                eprintln!("Verify error: {e}");
                std::process::exit(1);
            }
        }
        Cli::VerifyStart { session_id } => {
            let cwd = env::current_dir().expect("Cannot determine current directory");
            let project_root = crate::paths::resolve_project_root(&cwd).root;
            let env_session_id = std::env::var("TRACEVAULT_SESSION_ID").ok();
            if let Err(e) = commands::verification_phase::open_verification_phase(
                &project_root,
                &cwd,
                session_id.as_deref(),
                env_session_id.as_deref(),
            )
            .await
            {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Cli::AgentPolicies => {
            let cwd = env::current_dir().expect("Cannot determine current directory");
            let project_root = crate::paths::resolve_project_root(&cwd).root;
            if let Err(e) = commands::agent_policies::run(&project_root).await {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Cli::Proxy { cmd } => match cmd {
            ProxyCmd::Info => {
                let code = commands::proxy::run_proxy_info();
                if code != 0 {
                    std::process::exit(code);
                }
            }
        },
        Cli::Context { action } => {
            let cwd = env::current_dir().expect("Cannot determine current directory");
            let result = match action {
                ContextAction::Set {
                    flow,
                    label,
                    param,
                    global,
                    user,
                } => commands::context::run_set(&cwd, flow, label, param, global, user),
                ContextAction::Update {
                    flow,
                    label,
                    param,
                    remove_label,
                    remove_param,
                    global,
                    user,
                } => commands::context::run_update(
                    &cwd,
                    flow,
                    label,
                    param,
                    remove_label,
                    remove_param,
                    global,
                    user,
                ),
                ContextAction::Show => commands::context::run_show(&cwd),
                ContextAction::Clear { global, user } => {
                    commands::context::run_clear(&cwd, global, user)
                }
                ContextAction::Source {
                    enable,
                    disable,
                    path,
                    default,
                } => commands::context::run_source(&cwd, enable, disable, path, default),
            };
            if let Err(e) = result {
                eprintln!("Context error: {e}");
                std::process::exit(1);
            }
        }
        Cli::Repo { cmd } => {
            let cwd = env::current_dir().expect("Cannot determine current directory");
            let project_root = crate::paths::resolve_project_root(&cwd).root;
            if let Err(e) = commands::repo::run(cmd, &project_root, &cwd).await {
                eprintln!("Repo error: {e}");
                std::process::exit(1);
            }
        }
        Cli::Project { cmd } => {
            let cwd = env::current_dir().expect("Cannot determine current directory");
            let project_root = crate::paths::resolve_project_root(&cwd).root;
            if let Err(e) = commands::project::run(cmd, &project_root, &cwd).await {
                eprintln!("Project error: {e}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;

    #[test]
    fn init_user_context_flags_conflict() {
        // `--no-user-context` and `--user-context <path>` are mutually exclusive
        // and must be rejected up front rather than silently prioritizing one.
        // `Cli` has no `Debug` impl, so match instead of `expect_err`.
        let err = match Cli::try_parse_from([
            "tracevault",
            "init",
            "--no-user-context",
            "--user-context",
            "/tmp/ctx.json",
        ]) {
            Ok(_) => panic!("both flags together must be a parse error"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn init_user_context_flags_are_ok_alone() {
        assert!(Cli::try_parse_from(["tracevault", "init", "--no-user-context"]).is_ok());
        assert!(
            Cli::try_parse_from(["tracevault", "init", "--user-context", "/tmp/ctx.json"]).is_ok()
        );
    }

    #[test]
    fn context_source_mode_flags_conflict() {
        // The `source` mode flags are mutually exclusive; two together must be a
        // parse-time conflict rather than relying on run_source precedence.
        for extra in [["--enable", "--disable"], ["--disable", "--default"]] {
            let err = match Cli::try_parse_from(
                ["tracevault", "context", "source"].into_iter().chain(extra),
            ) {
                Ok(_) => panic!("{extra:?} together must be a parse error"),
                Err(e) => e,
            };
            assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        }
        // --path with --disable also conflicts.
        let err = match Cli::try_parse_from([
            "tracevault",
            "context",
            "source",
            "--path",
            "/tmp/ctx.json",
            "--disable",
        ]) {
            Ok(_) => panic!("--path with --disable must be a parse error"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn context_source_single_mode_is_ok() {
        assert!(Cli::try_parse_from(["tracevault", "context", "source", "--enable"]).is_ok());
        assert!(Cli::try_parse_from([
            "tracevault",
            "context",
            "source",
            "--path",
            "/tmp/ctx.json"
        ])
        .is_ok());
    }

    /// Rendered `--help` of the subcommand at `path`, whitespace-collapsed so
    /// assertions don't depend on line wrapping.
    fn long_help(path: &[&str]) -> String {
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        let mut sub = &mut cmd;
        for name in path {
            sub = sub
                .find_subcommand_mut(name)
                .unwrap_or_else(|| panic!("no subcommand {name}"));
        }
        sub.render_long_help()
            .to_string()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn init_help_names_every_agent_install_target() {
        let help = long_help(&["init"]);
        for needle in [
            ".codex/hooks.json",
            "gsd install -l",
            ".opencode/plugins/tracevault.ts",
            "~/.codex/",
            "OpenCode plugin",
            "--server-url, --claude-settings and --no-gitignore",
        ] {
            assert!(
                help.contains(needle),
                "init --help lacks {needle:?}: {help}"
            );
        }
        assert!(!help.contains("for ALL Claude Code sessions"), "{help}");
        // The opencode value's clap-naming note is an implementation comment.
        assert!(!help.contains("kebab-case"), "{help}");
    }

    #[test]
    fn hook_command_help_says_per_repo_init_installs_them() {
        for name in ["session-start", "user-prompt"] {
            let help = long_help(&[name]);
            assert!(help.contains("per-repo or --global"), "{name}: {help}");
            assert!(help.contains("Codex hooks.json"), "{name}: {help}");
        }
    }

    #[test]
    fn context_show_help_lists_the_user_section() {
        let help = long_help(&["context", "show"]);
        assert!(help.contains("User (always;"), "{help}");
        assert!(!help.contains("three sections"), "{help}");
    }
}
