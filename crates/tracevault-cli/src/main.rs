use clap::Parser;
use std::env;

mod api_client;
mod commands;
mod config;
mod context;
mod credentials;
mod hooks;
mod paths;
#[cfg(test)]
mod test_helpers;

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
        /// Disable the cross-repo user-level context for this project
        #[arg(long)]
        no_user_context: bool,
        /// Enable the user-level context and read it from this explicit path
        #[arg(long)]
        user_context: Option<String>,
    },
    /// Show current session status
    Status,
    /// Stream hook events to server in real-time.
    /// Installed into .claude/settings.json by `tracevault init` and invoked
    /// by Claude Code on every tool event — not intended to be run manually.
    #[command(hide = true)]
    Stream {
        #[arg(long)]
        event: String,
    },
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
        } => {
            let cwd = env::current_dir().expect("Cannot determine current directory");
            let user_context = match (no_user_context, user_context) {
                (true, _) => config::UserContext::Toggle(false),
                (false, Some(p)) => config::UserContext::Path(p),
                (false, None) => config::UserContext::Toggle(true),
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
        Cli::Status => {
            let cwd = env::current_dir().expect("Cannot determine current directory");
            let project_root = crate::paths::resolve_project_root(&cwd).root;
            let code = commands::status::run_status(&project_root).await;
            if code != 0 {
                std::process::exit(code);
            }
        }
        Cli::Stream { event } => {
            if let Err(e) = commands::stream::run_stream(&event).await {
                eprintln!("Stream error: {e}");
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
                } => commands::context::run_set(&cwd, flow, label, param, global),
                ContextAction::Update {
                    flow,
                    label,
                    param,
                    remove_label,
                    remove_param,
                    global,
                } => commands::context::run_update(
                    &cwd,
                    flow,
                    label,
                    param,
                    remove_label,
                    remove_param,
                    global,
                ),
                ContextAction::Show => commands::context::run_show(&cwd),
                ContextAction::Clear { global } => commands::context::run_clear(&cwd, global),
            };
            if let Err(e) = result {
                eprintln!("Context error: {e}");
                std::process::exit(1);
            }
        }
    }
}
