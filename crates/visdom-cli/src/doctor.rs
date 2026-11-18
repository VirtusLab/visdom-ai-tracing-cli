//! Gathering what the checks need from this machine, running them, and
//! carrying out the fixes the user agreed to.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tracevault_cli::agent::Agent;

use crate::agents::{adapter_for, AgentAdapter, AgentPaths, Workdir};
use crate::components::{bazaar, tracing, CheckContext, CredentialState};
use crate::config::{self, UserConfig};
use crate::finding::{command_line, needs_attention, render_text, Finding, Fix, FixAction};
use crate::fixes::{apply_fixes, FixExecutor, FixOutcome, FixPolicy};
use crate::mode::resolve_tracing_mode;

/// Tracing first, then bazaar.
pub fn collect(
    ctx: &CheckContext,
    adapter: &dyn AgentAdapter,
    bazaar_url: Result<Option<&str>, &str>,
    user_config_path: &Path,
) -> Vec<Finding> {
    let mut findings = tracing::check(ctx);
    findings.extend(bazaar::check(
        bazaar_url,
        adapter,
        &ctx.workdir,
        user_config_path,
    ));
    findings
}

/// The canonical top level of the git checkout containing `from`.
pub fn git_toplevel(from: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(from)
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let path = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    Some(path.canonicalize().unwrap_or(path))
}

/// The process environment a [`Session`] is gathered from.
pub struct Environment {
    pub cwd: PathBuf,
    pub home: PathBuf,
    /// The platform config directory (`~/.config` on Linux).
    pub config_root: PathBuf,
    pub codex_home: PathBuf,
    /// `VISDOM_AGENT`.
    pub agent: Option<String>,
    /// `VISDOM_BAZAAR_URL`.
    pub bazaar_url: Option<String>,
}

impl Environment {
    pub fn current() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        let set = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        Environment {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            config_root: dirs::config_dir().unwrap_or_else(|| home.join(".config")),
            codex_home: set("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex")),
            agent: set("VISDOM_AGENT"),
            bazaar_url: set("VISDOM_BAZAAR_URL"),
            home,
        }
    }
}

/// One run's view of the project: which agent, and everything the checks read.
pub struct Session {
    pub ctx: CheckContext,
    pub adapter: Box<dyn AgentAdapter>,
    /// `Err`: the server that should know it could not be asked (the reason).
    pub bazaar_url: Result<Option<String>, String>,
    pub user_config_path: PathBuf,
    /// Problems reading visdom's own config files; the run goes on without them.
    pub warnings: Vec<String>,
}

impl Session {
    /// Read the config files, choose the agent and gather the check context.
    /// The only error is an agent that cannot be chosen or launched.
    pub fn gather(
        env: &Environment,
        agent_flag: Option<Agent>,
        runtime: &tokio::runtime::Runtime,
    ) -> Result<Session, String> {
        let mut warnings = Vec::new();
        let project_dir = git_toplevel(&env.cwd);
        let user: UserConfig = config::load_user_config_in(&env.config_root).unwrap_or_else(|e| {
            warnings.push(e);
            UserConfig::default()
        });
        let repo = project_dir
            .as_deref()
            .map(config::load_repo_config_in)
            .transpose()
            .unwrap_or_else(|e| {
                warnings.push(e);
                None
            })
            .unwrap_or_default();
        let agent = config::resolve_agent(agent_flag, env.agent.as_deref(), &repo, &user)?;

        let tracing_root = project_dir
            .as_deref()
            .map(|dir| tracevault_cli::paths::resolve_project_root(dir).root);
        let credential_root = tracing_root.as_deref().unwrap_or(&env.cwd);
        let (server_url, credential) =
            match tracevault_cli::credentials::resolve_credentials_quiet(credential_root) {
                Ok((url, Some(_))) => (url, CredentialState::Present),
                Ok((url, None)) => (url, missing_or_unreadable()),
                Err(e) => (None, CredentialState::Unusable(e.to_string())),
            };

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap_or_default();
        let bazaar_url = runtime.block_on(bazaar::resolve_bazaar_url(
            env.bazaar_url.clone(),
            &user,
            server_url.as_deref(),
            &http,
        ));

        let paths = AgentPaths {
            home: env.home.clone(),
            codex_home: env.codex_home.clone(),
        };
        let adapter = adapter_for(agent, paths, user.agents.program_for(agent))
            .ok_or_else(|| format!("visdom cannot launch {}", agent.tool_name()))?;
        Ok(Session {
            ctx: CheckContext {
                mode: resolve_tracing_mode(project_dir.as_deref()),
                workdir: Workdir {
                    start_dir: env.cwd.clone(),
                    checkout: project_dir,
                    primary: tracing_root,
                },
                home: env.home.clone(),
                codex_home: env.codex_home.clone(),
                agent,
                server_url,
                credential,
            },
            adapter,
            bazaar_url,
            user_config_path: config::user_config_path_in(&env.config_root),
            warnings,
        })
    }

    pub fn collect(&self) -> Vec<Finding> {
        collect(
            &self.ctx,
            self.adapter.as_ref(),
            self.bazaar_url
                .as_ref()
                .map(|url| url.as_deref())
                .map_err(String::as_str),
            &self.user_config_path,
        )
    }
}

/// No credential was resolved: either there is none, or the credential file
/// is there and cannot be read or parsed (the library treats both alike).
fn missing_or_unreadable() -> CredentialState {
    use tracevault_cli::credentials::Credentials;
    match Credentials::path() {
        Ok(path) if path.exists() && Credentials::load().is_none() => {
            CredentialState::Unreadable(path.display().to_string())
        }
        _ => CredentialState::Missing,
    }
}

/// Carries out fixes by running commands: `visdom` itself for tracing and
/// login, the agent's own CLI for its MCP configuration.
pub struct RealExecutor {
    /// This executable.
    pub visdom: PathBuf,
    /// Send the commands' stdout to stderr (when stdout carries JSON).
    pub stdout_to_stderr: bool,
    /// Give the commands no stdin, so none of them can stop to ask anything:
    /// with `--yes` the fix shown is applied with its defaults.
    pub no_questions: bool,
}

impl RealExecutor {
    pub fn new(stdout_to_stderr: bool, policy: FixPolicy) -> Self {
        RealExecutor {
            visdom: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("visdom")),
            stdout_to_stderr,
            no_questions: policy == FixPolicy::All,
        }
    }

    fn run(&self, program: &str, args: &[String], cwd: Option<&Path>) -> Result<(), String> {
        let line = command_line(program, args);
        let mut cmd = Command::new(program);
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        if self.stdout_to_stderr {
            cmd.stdout(Stdio::from(std::io::stderr()));
        }
        if self.no_questions {
            cmd.stdin(Stdio::null());
        }
        match cmd.status() {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(format!("`{line}` failed ({status})")),
            Err(e) => Err(format!("cannot run `{line}`: {e}")),
        }
    }
}

impl FixExecutor for RealExecutor {
    fn execute(&mut self, action: &FixAction) -> Result<(), String> {
        let visdom = self.visdom.to_string_lossy().into_owned();
        match action {
            FixAction::TracingInit { agent, dir } => self.run(
                &visdom,
                &["trace", "init", "--agent", agent.tool_name()].map(String::from),
                Some(dir),
            ),
            FixAction::Login { server_url } => self.run(
                &visdom,
                &["login", "--server-url", server_url].map(String::from),
                None,
            ),
            FixAction::RunCommand { program, args, cwd } => self.run(program, args, cwd.as_deref()),
            FixAction::Sequence { steps } => steps.iter().try_for_each(|step| self.execute(step)),
        }
    }
}

/// Whether a line read at the prompt is a yes. Enter alone is; end of input
/// (nothing read, not even a newline) and a read error are not.
pub fn is_yes(read: std::io::Result<usize>, line: &str) -> bool {
    match read {
        Ok(0) | Err(_) => false,
        Ok(_) => matches!(line.trim(), "" | "y" | "Y" | "yes"),
    }
}

/// Show one fix on the terminal and read the answer.
pub fn ask_on_terminal(fix: &Fix) -> bool {
    eprint!("Apply: {}? [Y/n] ", fix.description);
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    let read = std::io::stdin().lock().read_line(&mut answer);
    is_yes(read, &answer)
}

/// How a round of fixes went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixSummary {
    /// Something was changed, so the checks are worth running again.
    pub applied: bool,
    /// A fix that was meant to be applied failed.
    pub failed: bool,
}

/// Offer the fixes of `findings` under `policy`, reporting each failure on
/// stderr. A fix applied without asking is still shown first.
pub fn fix(findings: &[Finding], policy: FixPolicy, exec: &mut dyn FixExecutor) -> FixSummary {
    if policy == FixPolicy::All {
        let mut shown: Vec<&str> = Vec::new();
        for fix in findings.iter().filter_map(|f| f.fix.as_ref()) {
            if !shown.contains(&fix.description.as_str()) {
                eprintln!("visdom: applying: {}", fix.description);
                shown.push(&fix.description);
            }
        }
    }
    let outcomes: Vec<FixOutcome> = apply_fixes(findings, policy, &mut ask_on_terminal, exec);
    let mut reported: Vec<&str> = Vec::new();
    for error in outcomes.iter().filter_map(|o| o.error.as_deref()) {
        if !reported.contains(&error) {
            eprintln!("visdom: fix failed: {error}");
            reported.push(error);
        }
    }
    FixSummary {
        applied: outcomes.iter().any(|o| o.applied),
        failed: outcomes.iter().any(|o| o.error.is_some()),
    }
}

/// What a developer sees before the agent starts (or after `init`): the
/// findings that need attention, on stderr. Nothing when all is well.
pub fn print_attention(findings: &[Finding]) {
    let attention = needs_attention(findings);
    if !attention.is_empty() {
        eprint!("{}", render_text(&attention));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_of_input_at_the_prompt_is_a_no() {
        assert!(is_yes(Ok(1), "\n"));
        assert!(is_yes(Ok(2), "y\n"));
        assert!(is_yes(Ok(4), "yes\n"));
        assert!(!is_yes(Ok(2), "n\n"));
        assert!(!is_yes(Ok(6), "maybe\n"));
        // Ctrl-D: nothing was read.
        assert!(!is_yes(Ok(0), ""));
        assert!(!is_yes(Err(std::io::Error::other("closed")), ""));
    }

    /// With `--yes` a fix must not stop to ask: its command gets no stdin.
    #[test]
    fn no_questions_gives_commands_an_empty_stdin() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("read");
        let mut exec = RealExecutor {
            visdom: PathBuf::from("visdom"),
            stdout_to_stderr: true,
            no_questions: true,
        };
        let script = format!("cat > '{}'", out.display());
        let result = exec.execute(&FixAction::RunCommand {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), script],
            cwd: None,
        });
        assert_eq!(result, Ok(()));
        assert_eq!(std::fs::read(&out).unwrap(), b"");
        assert!(RealExecutor::new(false, FixPolicy::All).no_questions);
        assert!(!RealExecutor::new(false, FixPolicy::Ask).no_questions);
    }

    #[test]
    fn git_toplevel_is_none_outside_a_repository() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(git_toplevel(tmp.path()), None);
    }

    #[test]
    fn git_toplevel_from_a_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        assert!(status.success());
        let sub = tmp.path().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(git_toplevel(&sub), Some(tmp.path().canonicalize().unwrap()));
    }

    #[test]
    fn executor_reports_a_failing_and_a_missing_command() {
        let mut exec = RealExecutor {
            visdom: PathBuf::from("visdom"),
            stdout_to_stderr: true,
            no_questions: false,
        };
        let run = |program: &str, args: &[&str]| FixAction::RunCommand {
            program: program.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            cwd: None,
        };
        assert_eq!(exec.execute(&run("true", &[])), Ok(()));
        let err = exec.execute(&run("false", &[])).unwrap_err();
        assert!(err.contains("`false` failed"), "{err}");
        let err = exec
            .execute(&run("/nonexistent/visdom-test-program", &["x"]))
            .unwrap_err();
        assert!(err.contains("cannot run"), "{err}");
    }

    #[test]
    fn sequence_stops_at_the_first_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("ran");
        let mut exec = RealExecutor {
            visdom: PathBuf::from("visdom"),
            stdout_to_stderr: true,
            no_questions: false,
        };
        let step = |program: &str, args: Vec<String>| FixAction::RunCommand {
            program: program.to_string(),
            args,
            cwd: None,
        };
        let result = exec.execute(&FixAction::Sequence {
            steps: vec![
                step("false", vec![]),
                step("touch", vec![marker.to_string_lossy().into_owned()]),
            ],
        });
        assert!(result.is_err());
        assert!(!marker.exists(), "the step after a failure must not run");
    }

    #[test]
    fn run_command_uses_its_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let mut exec = RealExecutor {
            visdom: PathBuf::from("visdom"),
            stdout_to_stderr: true,
            no_questions: false,
        };
        let result = exec.execute(&FixAction::RunCommand {
            program: "touch".to_string(),
            args: vec!["here".to_string()],
            cwd: Some(tmp.path().to_path_buf()),
        });
        assert_eq!(result, Ok(()));
        assert!(tmp.path().join("here").exists());
    }
}
