//! Is this project's agent session going to reach AI Tracing?

use std::path::{Path, PathBuf};

use tracevault_cli::agent::Agent;
use tracevault_cli::commands::init::{has_tracevault_hooks, POST_COMMIT_MARKER, PRE_PUSH_MARKER};
use tracevault_cli::config::TracevaultConfig;

use super::{CheckContext, CredentialState};
use crate::finding::{Finding, Fix, FixAction, Severity};

const COMPONENT: &str = "tracing";

/// The same in both tracing modes. Through the AI Gateway the hooks still
/// matter: they bring the repository's policies into the session, link
/// commits to it and check policies before a push; only their capture is
/// switched off, for the launched agent, by `visdom` itself.
pub fn check(ctx: &CheckContext) -> Vec<Finding> {
    let mut findings = vec![auth(ctx)];
    let (Some(project_dir), Some(root)) = (&ctx.workdir.checkout, &ctx.workdir.primary) else {
        findings.push(Finding::new(
            COMPONENT,
            "tracing.repo",
            Severity::Info,
            "not in a repository: only sessions captured by a global install are traced here",
        ));
        return findings;
    };

    let global = global_hook_file(ctx);
    let global_installed = matches!(has_tracevault_hooks(&global), Ok(true));
    let init = || Fix {
        description: format!(
            "run `visdom trace init --agent {}` in {} (writes .tracevault/, the {} hooks and \
             the git hooks)",
            ctx.agent.tool_name(),
            root.display(),
            ctx.agent.label()
        ),
        action: FixAction::TracingInit {
            agent: ctx.agent,
            dir: root.clone(),
        },
    };

    let config = TracevaultConfig::try_load(root);
    let own_setup = !matches!(config, Ok(None));
    findings.push(if let Err(e) = &config {
        // It is there but the tracing library cannot use it; `init` refuses
        // to replace it, so there is nothing to offer.
        Finding::new(
            COMPONENT,
            "tracing.repo",
            Severity::Unknown,
            format!(
                "cannot use {} — {e}; fix or remove it",
                TracevaultConfig::config_path(root).display()
            ),
        )
    } else if own_setup {
        Finding::new(
            COMPONENT,
            "tracing.repo",
            Severity::Ok,
            format!("tracing is set up in {}", root.display()),
        )
    } else if global_installed {
        Finding::new(
            COMPONENT,
            "tracing.repo",
            Severity::Ok,
            "no .tracevault/ here: sessions are captured by the global install and bound to \
             the repository by its origin remote",
        )
    } else {
        Finding::new(
            COMPONENT,
            "tracing.repo",
            Severity::Error,
            format!("tracing is not set up in {}", root.display()),
        )
        .with_fix(init())
    });

    let hooks = agent_hooks(ctx, project_dir, root, &global);
    findings.push(if hooks.severity == Severity::Error {
        hooks.with_fix(init())
    } else {
        hooks
    });

    let git_hooks = missing_git_hooks(root);
    if let Err(unreadable) = &git_hooks {
        findings.push(Finding::new(
            COMPONENT,
            "tracing.git_hooks",
            Severity::Unknown,
            format!("could not tell whether the git hooks are installed — {unreadable}"),
        ));
    } else if let Ok(Some(missing)) = &git_hooks {
        let message = format!(
            "git {missing} hook{} not installed in {}: commits are not linked to sessions and \
             policies are not checked before a push",
            if missing.contains(" and ") { "s" } else { "" },
            root.join(".git/hooks").display()
        );
        findings.push(if global_installed && !own_setup {
            // The global install has no git hooks, by design: say so once,
            // without offering to turn the repository into a per-repo setup
            // at every launch.
            Finding::new(
                COMPONENT,
                "tracing.git_hooks",
                Severity::Info,
                format!(
                    "{message} (`visdom trace init` in {} adds them)",
                    root.display()
                ),
            )
        } else {
            Finding::new(COMPONENT, "tracing.git_hooks", Severity::Warn, message).with_fix(init())
        });
    } else {
        findings.push(Finding::new(
            COMPONENT,
            "tracing.git_hooks",
            Severity::Ok,
            "git hooks installed (pre-push, post-commit)",
        ));
    }
    findings
}

fn auth(ctx: &CheckContext) -> Finding {
    const ID: &str = "tracing.auth";
    match (&ctx.credential, &ctx.server_url) {
        (CredentialState::Present, Some(url)) => {
            Finding::new(COMPONENT, ID, Severity::Ok, format!("logged in to {url}"))
        }
        (CredentialState::Present, None) => {
            Finding::new(COMPONENT, ID, Severity::Ok, "credential found")
        }
        (CredentialState::Unreadable(path), _) => Finding::new(
            COMPONENT,
            ID,
            Severity::Unknown,
            format!("could not tell whether you are logged in — {path} cannot be read or parsed"),
        ),
        (CredentialState::Unusable(reason), _) => {
            Finding::new(COMPONENT, ID, Severity::Error, reason.clone())
        }
        (CredentialState::Missing, Some(url)) => Finding::new(
            COMPONENT,
            ID,
            Severity::Error,
            format!("not logged in to {url}"),
        )
        .with_fix(Fix {
            description: format!(
                "run `visdom login --server-url {url}` (signs you in through the browser)"
            ),
            action: FixAction::Login {
                server_url: url.clone(),
            },
        }),
        (CredentialState::Missing, None) => Finding::new(
            COMPONENT,
            ID,
            Severity::Error,
            "not logged in — run `visdom login --server-url <url>`",
        ),
    }
}

/// The user-level hook file of the agent, which applies to every session.
fn global_hook_file(ctx: &CheckContext) -> PathBuf {
    match ctx.agent {
        Agent::Codex => ctx.codex_home.join("hooks.json"),
        _ => ctx.home.join(".claude").join("settings.json"),
    }
}

/// Whether the agent's hooks are installed anywhere the agent loads them
/// from. An `Error` finding means they are not; the caller attaches the fix.
fn agent_hooks(ctx: &CheckContext, project_dir: &Path, root: &Path, global: &Path) -> Finding {
    const ID: &str = "tracing.agent_hooks";
    let names: &[&str] = match ctx.agent {
        Agent::Codex => &[".codex/hooks.json"],
        _ => &[".claude/settings.json", ".claude/settings.local.json"],
    };
    // The checkout the agent starts in, then the primary checkout (where
    // `init` writes them), then the global install.
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in [project_dir, root] {
        for name in names {
            let file = dir.join(name);
            if !files.contains(&file) {
                files.push(file);
            }
        }
    }
    files.push(global.to_path_buf());

    let mut unreadable = Vec::new();
    for file in &files {
        match has_tracevault_hooks(file) {
            Ok(true) => {
                return Finding::new(
                    COMPONENT,
                    ID,
                    Severity::Ok,
                    format!("{} hooks installed ({})", ctx.agent.label(), file.display()),
                )
            }
            Ok(false) => {}
            Err(e) => unreadable.push(format!("{}: {e}", file.display())),
        }
    }
    if !unreadable.is_empty() {
        return Finding::new(
            COMPONENT,
            ID,
            Severity::Unknown,
            format!(
                "could not tell whether the {} hooks are installed — {}",
                ctx.agent.label(),
                unreadable.join("; ")
            ),
        );
    }
    Finding::new(
        COMPONENT,
        ID,
        Severity::Error,
        format!(
            "{} hooks are not installed: sessions in this project are not traced",
            ctx.agent.label()
        ),
    )
}

/// `"pre-push"`, `"post-commit"` or `"pre-push and post-commit"` — whichever
/// lacks the tracing block — or `None` when both have it. A hook file that
/// exists but cannot be read is an error naming it: it may well have the block.
fn missing_git_hooks(root: &Path) -> Result<Option<String>, String> {
    let hooks = root.join(".git").join("hooks");
    let mut missing: Vec<&str> = Vec::new();
    for (name, marker) in [
        ("pre-push", PRE_PUSH_MARKER),
        ("post-commit", POST_COMMIT_MARKER),
    ] {
        let path = hooks.join(name);
        match std::fs::read(&path) {
            Ok(bytes) if String::from_utf8_lossy(&bytes).contains(marker) => {}
            Ok(_) => missing.push(name),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => missing.push(name),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    Ok((!missing.is_empty()).then(|| missing.join(" and ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::Workdir;
    use std::fs;
    use tempfile::TempDir;

    struct Fixture {
        repo: TempDir,
        home: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let repo = TempDir::new().unwrap();
            let status = std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(repo.path())
                .status()
                .unwrap();
            assert!(status.success());
            Fixture {
                repo,
                home: TempDir::new().unwrap(),
            }
        }

        fn ctx(&self, agent: Agent) -> CheckContext {
            CheckContext {
                workdir: Workdir::checkout(self.repo.path()),
                home: self.home.path().to_path_buf(),
                codex_home: self.home.path().join(".codex"),
                agent,
                mode: crate::mode::TracingMode::Hooks,
                server_url: Some("https://tv.example.com".to_string()),
                credential: CredentialState::Present,
            }
        }

        fn write(&self, rel: &str, content: &str) {
            let path = self.repo.path().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        fn wire_config(&self) {
            self.write(".tracevault/config.toml", "server_url = \"https://tv\"\n");
        }

        fn wire_git_hooks(&self) {
            self.write(
                ".git/hooks/pre-push",
                &format!("#!/bin/sh\n{PRE_PUSH_MARKER}\n"),
            );
            self.write(
                ".git/hooks/post-commit",
                &format!("#!/bin/sh\n{POST_COMMIT_MARKER}\n"),
            );
        }
    }

    const CLAUDE_HOOKS: &str = r#"{"hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"tracevault stream --event stop"}]}]}}"#;
    const CODEX_HOOKS: &str = r#"{"hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"tracevault stream --event stop --agent codex"}]}]}}"#;

    fn by_id<'a>(findings: &'a [Finding], id: &str) -> &'a Finding {
        findings
            .iter()
            .find(|f| f.id == id)
            .unwrap_or_else(|| panic!("no finding {id} in {findings:?}"))
    }

    fn is_init(finding: &Finding, agent: Agent, dir: &Path) -> bool {
        matches!(&finding.fix, Some(Fix { action: FixAction::TracingInit { agent: a, dir: d }, .. })
            if *a == agent && d == dir)
    }

    #[test]
    fn fully_wired_repo_is_all_ok() {
        let fx = Fixture::new();
        fx.wire_config();
        fx.wire_git_hooks();
        fx.write(".claude/settings.local.json", CLAUDE_HOOKS);
        let findings = check(&fx.ctx(Agent::ClaudeCode));
        assert_eq!(findings.len(), 4, "{findings:?}");
        for finding in &findings {
            assert_eq!(finding.severity, Severity::Ok, "{finding:?}");
            assert!(finding.fix.is_none());
        }
    }

    #[test]
    fn outside_repo_is_info_without_fix() {
        let fx = Fixture::new();
        let mut ctx = fx.ctx(Agent::ClaudeCode);
        ctx.workdir = Workdir::plain(fx.home.path());
        let findings = check(&ctx);
        let ids: Vec<&str> = findings.iter().map(|f| f.id).collect();
        assert_eq!(ids, ["tracing.auth", "tracing.repo"]);
        let repo = by_id(&findings, "tracing.repo");
        assert_eq!(repo.severity, Severity::Info);
        assert!(repo.message.contains("not in a repository"));
        assert!(findings.iter().all(|f| f.fix.is_none()));
    }

    #[test]
    fn missing_agent_hooks_offers_init() {
        let fx = Fixture::new();
        fx.wire_config();
        fx.wire_git_hooks();
        let findings = check(&fx.ctx(Agent::ClaudeCode));
        let hooks = by_id(&findings, "tracing.agent_hooks");
        assert_eq!(hooks.severity, Severity::Error);
        assert!(
            is_init(hooks, Agent::ClaudeCode, fx.repo.path()),
            "{hooks:?}"
        );
        assert!(hooks
            .fix
            .as_ref()
            .unwrap()
            .description
            .contains("visdom trace init --agent claude-code"));
        assert_eq!(by_id(&findings, "tracing.repo").severity, Severity::Ok);
    }

    #[test]
    fn nothing_wired_offers_one_shared_init() {
        let fx = Fixture::new();
        let findings = check(&fx.ctx(Agent::ClaudeCode));
        let fixes: Vec<&FixAction> = findings
            .iter()
            .filter_map(|f| f.fix.as_ref().map(|fix| &fix.action))
            .collect();
        assert_eq!(fixes.len(), 3, "repo, agent hooks, git hooks: {findings:?}");
        assert!(fixes.iter().all(|a| *a == fixes[0]), "one init fixes all");
    }

    #[test]
    fn global_claude_hooks_satisfy_agent_hooks() {
        let fx = Fixture::new();
        fs::create_dir_all(fx.home.path().join(".claude")).unwrap();
        fs::write(fx.home.path().join(".claude/settings.json"), CLAUDE_HOOKS).unwrap();
        let findings = check(&fx.ctx(Agent::ClaudeCode));
        assert_eq!(
            by_id(&findings, "tracing.agent_hooks").severity,
            Severity::Ok
        );
        // With a global install a repo needs no .tracevault/ of its own.
        let repo = by_id(&findings, "tracing.repo");
        assert_eq!(repo.severity, Severity::Ok);
        assert!(repo.message.contains("global install"), "{repo:?}");
        // The global install has no git hooks: worth knowing, nothing to fix.
        let git = by_id(&findings, "tracing.git_hooks");
        assert_eq!(git.severity, Severity::Info);
        assert!(git.fix.is_none());
        assert!(findings.iter().all(|f| f.fix.is_none()), "{findings:?}");
    }

    #[test]
    fn codex_checks_codex_files_not_claude() {
        let fx = Fixture::new();
        fx.wire_config();
        fx.write(".claude/settings.json", CLAUDE_HOOKS);
        let findings = check(&fx.ctx(Agent::Codex));
        let hooks = by_id(&findings, "tracing.agent_hooks");
        assert_eq!(hooks.severity, Severity::Error);
        assert!(is_init(hooks, Agent::Codex, fx.repo.path()));

        fx.write(".codex/hooks.json", CODEX_HOOKS);
        let findings = check(&fx.ctx(Agent::Codex));
        assert_eq!(
            by_id(&findings, "tracing.agent_hooks").severity,
            Severity::Ok
        );

        // And the other way round: Codex hooks do not serve Claude Code.
        let other = Fixture::new();
        other.write(".codex/hooks.json", CODEX_HOOKS);
        let findings = check(&other.ctx(Agent::ClaudeCode));
        assert_eq!(
            by_id(&findings, "tracing.agent_hooks").severity,
            Severity::Error
        );
    }

    #[test]
    fn global_codex_hooks_are_read_from_codex_home() {
        let fx = Fixture::new();
        fs::create_dir_all(fx.home.path().join(".codex")).unwrap();
        fs::write(fx.home.path().join(".codex/hooks.json"), CODEX_HOOKS).unwrap();
        let findings = check(&fx.ctx(Agent::Codex));
        assert_eq!(
            by_id(&findings, "tracing.agent_hooks").severity,
            Severity::Ok
        );
    }

    #[test]
    fn unreadable_hook_file_is_unknown_without_fix() {
        let fx = Fixture::new();
        fx.wire_config();
        fx.write(".claude/settings.json", "{ not json");
        let findings = check(&fx.ctx(Agent::ClaudeCode));
        let hooks = by_id(&findings, "tracing.agent_hooks");
        assert_eq!(hooks.severity, Severity::Unknown);
        assert!(hooks.fix.is_none());
        assert!(hooks.message.contains("settings.json"), "{hooks:?}");
    }

    #[test]
    fn missing_git_hooks_is_warn() {
        let fx = Fixture::new();
        fx.wire_config();
        fx.write(".claude/settings.json", CLAUDE_HOOKS);
        let findings = check(&fx.ctx(Agent::ClaudeCode));
        let git = by_id(&findings, "tracing.git_hooks");
        assert_eq!(git.severity, Severity::Warn);
        assert!(
            git.message.contains("pre-push and post-commit hooks"),
            "{git:?}"
        );
        assert!(is_init(git, Agent::ClaudeCode, fx.repo.path()));

        // A hook file without the tracing block does not count.
        fx.write(".git/hooks/pre-push", "#!/bin/sh\nexit 0\n");
        fx.write(
            ".git/hooks/post-commit",
            &format!("#!/bin/sh\n{POST_COMMIT_MARKER}\n"),
        );
        let findings = check(&fx.ctx(Agent::ClaudeCode));
        let git = by_id(&findings, "tracing.git_hooks");
        assert!(git.message.contains("git pre-push hook not"), "{git:?}");
    }

    #[test]
    fn malformed_tracing_config_is_unknown_without_fix() {
        let fx = Fixture::new();
        fx.wire_git_hooks();
        fx.write(".claude/settings.json", CLAUDE_HOOKS);
        fx.write(".tracevault/config.toml", "server_url = [truncated");
        let findings = check(&fx.ctx(Agent::ClaudeCode));
        let repo = by_id(&findings, "tracing.repo");
        assert_eq!(repo.severity, Severity::Unknown, "{repo:?}");
        assert!(repo.fix.is_none());
        assert!(repo.message.contains("config.toml"), "{repo:?}");
    }

    #[test]
    fn unreadable_git_hook_is_unknown_without_fix() {
        let fx = Fixture::new();
        fx.wire_config();
        fx.wire_git_hooks();
        fx.write(".claude/settings.json", CLAUDE_HOOKS);
        // A directory where the hook file should be: it exists, and reading
        // it fails (for every user, root included).
        fs::remove_file(fx.repo.path().join(".git/hooks/pre-push")).unwrap();
        fs::create_dir(fx.repo.path().join(".git/hooks/pre-push")).unwrap();
        let findings = check(&fx.ctx(Agent::ClaudeCode));
        let git = by_id(&findings, "tracing.git_hooks");
        assert_eq!(git.severity, Severity::Unknown, "{git:?}");
        assert!(git.fix.is_none());
        assert!(git.message.contains("pre-push"), "{git:?}");

        // Not UTF-8 is still readable: the marker is found in it.
        fs::remove_dir(fx.repo.path().join(".git/hooks/pre-push")).unwrap();
        let mut bytes = vec![0xff, 0xfe, b'\n'];
        bytes.extend_from_slice(PRE_PUSH_MARKER.as_bytes());
        fs::write(fx.repo.path().join(".git/hooks/pre-push"), bytes).unwrap();
        let findings = check(&fx.ctx(Agent::ClaudeCode));
        assert_eq!(by_id(&findings, "tracing.git_hooks").severity, Severity::Ok);
    }

    /// In a linked worktree the tracing state belongs to the primary checkout:
    /// that is where it is looked for and where `init` is offered.
    #[test]
    fn linked_worktree_checks_the_primary_checkout() {
        let fx = Fixture::new();
        let worktree = TempDir::new().unwrap();
        let mut ctx = fx.ctx(Agent::ClaudeCode);
        ctx.workdir = Workdir {
            start_dir: worktree.path().to_path_buf(),
            checkout: Some(worktree.path().to_path_buf()),
            primary: Some(fx.repo.path().to_path_buf()),
        };

        let findings = check(&ctx);
        let hooks = by_id(&findings, "tracing.agent_hooks");
        assert!(
            is_init(hooks, Agent::ClaudeCode, fx.repo.path()),
            "{hooks:?}"
        );

        fx.wire_config();
        fx.wire_git_hooks();
        fx.write(".claude/settings.local.json", CLAUDE_HOOKS);
        let findings = check(&ctx);
        assert!(
            findings.iter().all(|f| f.severity == Severity::Ok),
            "{findings:?}"
        );
    }

    #[test]
    fn no_credentials_is_error_with_login_fix() {
        let fx = Fixture::new();
        let mut ctx = fx.ctx(Agent::ClaudeCode);
        ctx.credential = CredentialState::Missing;
        let findings = check(&ctx);
        let auth = by_id(&findings, "tracing.auth");
        assert_eq!(auth.severity, Severity::Error);
        assert_eq!(
            auth.fix.as_ref().map(|f| &f.action),
            Some(&FixAction::Login {
                server_url: "https://tv.example.com".to_string()
            })
        );

        // Without a known server there is nothing to run, only to say.
        ctx.server_url = None;
        let findings = check(&ctx);
        let auth = by_id(&findings, "tracing.auth");
        assert_eq!(auth.severity, Severity::Error);
        assert!(auth.fix.is_none());
        assert!(auth.message.contains("visdom login --server-url <url>"));
    }

    #[test]
    fn unreadable_credential_file_is_unknown_without_login_fix() {
        let fx = Fixture::new();
        let mut ctx = fx.ctx(Agent::ClaudeCode);
        ctx.credential = CredentialState::Unreadable("/cfg/tracevault/credentials.json".into());
        let findings = check(&ctx);
        let auth = by_id(&findings, "tracing.auth");
        assert_eq!(auth.severity, Severity::Unknown);
        assert!(auth.fix.is_none());
        assert!(auth.message.contains("credentials.json"), "{auth:?}");
    }

    #[test]
    fn unusable_credential_is_error_with_its_reason() {
        let fx = Fixture::new();
        let mut ctx = fx.ctx(Agent::ClaudeCode);
        ctx.credential = CredentialState::Unusable("logged in to another server".to_string());
        let findings = check(&ctx);
        let auth = by_id(&findings, "tracing.auth");
        assert_eq!(auth.severity, Severity::Error);
        assert_eq!(auth.message, "logged in to another server");
        assert!(auth.fix.is_none());
    }

    /// An API key from the environment is a credential like any other: the
    /// wiring reports it as `Present`, and the check asks for no login.
    #[test]
    fn api_key_env_counts_as_credential() {
        let fx = Fixture::new();
        let mut ctx = fx.ctx(Agent::ClaudeCode);
        ctx.credential = CredentialState::Present;
        ctx.server_url = None;
        let findings = check(&ctx);
        let auth = by_id(&findings, "tracing.auth");
        assert_eq!(auth.severity, Severity::Ok);
        assert!(auth.fix.is_none());
    }

    #[test]
    fn the_setup_is_checked_the_same_in_gateway_mode() {
        let fx = Fixture::new();
        let hooks = check(&fx.ctx(Agent::ClaudeCode));
        let mut ctx = fx.ctx(Agent::ClaudeCode);
        ctx.mode = crate::mode::TracingMode::Gateway;
        let gateway = check(&ctx);
        let ids = |findings: &[Finding]| -> Vec<(&'static str, Severity)> {
            findings.iter().map(|f| (f.id, f.severity)).collect()
        };
        assert_eq!(ids(&gateway), ids(&hooks));
        assert!(!gateway.is_empty());
    }
}
