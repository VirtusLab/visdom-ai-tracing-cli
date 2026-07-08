use clap::Parser;
use std::env;

mod api_client;
mod commands;
mod config;
mod context;
mod credentials;
mod hooks;
mod paths;
mod resolution;
mod session_state;
#[cfg(test)]
mod test_helpers;
mod user_default;

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
        /// Install TraceVault hooks once into ~/.claude/ for ALL Claude Code
        /// sessions, instead of initializing the current repo. Does not
        /// require git. Also writes a user-level context config (honoring
        /// --no-user-context / --user-context below). Conflicts with the
        /// per-repo-only flags below, which this mode has no use for.
        #[arg(
            long,
            conflicts_with_all = ["server_url", "claude_settings", "no_gitignore"]
        )]
        global: bool,
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
    },
    /// SessionStart hook: exports the session id and injects the bound repo's
    /// policies as additionalContext. Installed into .claude/settings.json by
    /// `tracevault init --global` — not intended to be run manually.
    #[command(name = "session-start", hide = true)]
    SessionStart,
    /// UserPromptSubmit hook: reinforces the bound repo's policies as
    /// additionalContext when the effective repo has changed since the last
    /// injection. Installed into .claude/settings.json by
    /// `tracevault init --global` — not intended to be run manually.
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
        /// Explicit session ID to open the window for. When omitted, the most
        /// recently active session under .tracevault/sessions/ is used.
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
    /// Show the current context (path + pretty JSON).
    ///
    /// Prints three sections: Global (repo-wide), This worktree (linked only),
    /// and Effective (the merged result that the hook stamps).
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
        } => {
            if global {
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

                let requested =
                    match config::UserContext::from_init_flags(no_user_context, user_context) {
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!("Error: {e}");
                            std::process::exit(1);
                        }
                    };
                match commands::init::write_global_user_config_in(
                    &config::tv_config_root(),
                    requested,
                ) {
                    Ok(active) => {
                        println!(
                            "User-level context config: {}",
                            config::user_config_path().display()
                        );
                        match active {
                            Some(ctx) => println!("User-level context file: {}", ctx.display()),
                            None => {
                                println!("User-level context is disabled (`--no-user-context`).")
                            }
                        }
                        println!(
                            "Edit it with `tracevault context set --user …`; disable with \
                             `tracevault init --global --no-user-context`."
                        );
                    }
                    Err(e) => eprintln!("Warning: could not write user-level context config: {e}"),
                }
                return;
            }

            let cwd = env::current_dir().expect("Cannot determine current directory");
            let user_context =
                match config::UserContext::from_init_flags(no_user_context, user_context) {
                    Ok(r) => r.unwrap_or(config::UserContext::Toggle(true)),
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
            )
            .await
            {
                Ok(target) => {
                    let entry = target.gitignore_entry();
                    println!("TraceVault initialized in {}", cwd.display());
                    println!("Claude Code hooks installed ({entry})");
                    println!("Git hooks installed (pre-push, post-commit)");
                    println!("Added .tracevault/ and {entry} to .gitignore");
                    println!(
                        "Nothing needs to be committed — all TraceVault files are local only."
                    );
                    println!(
                        "Other contributors run `tracevault init` to set up their own local hooks."
                    );
                }
                Err(e) => eprintln!("Error: {e}"),
            }
        }
        Cli::Status { session_id } => {
            let cwd = env::current_dir().expect("Cannot determine current directory");
            let project_root = crate::paths::resolve_project_root(&cwd).root;
            let effective = commands::status::effective_session_id(
                session_id,
                std::env::var("TRACEVAULT_SESSION_ID").ok(),
            );
            let code = commands::status::run_status(&project_root, effective.as_deref()).await;
            if code != 0 {
                std::process::exit(code);
            }
        }
        Cli::Stream { event } => {
            if let Err(e) = commands::stream::run_stream(&event).await {
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
            }
        }
        Cli::Logout => {
            if let Err(e) = commands::logout::logout().await {
                eprintln!("Logout error: {e}");
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
            if let Err(e) = commands::verification_phase::open_verification_phase(
                &project_root,
                &cwd,
                session_id.as_deref(),
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
}
