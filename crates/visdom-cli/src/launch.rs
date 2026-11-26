//! Handing the terminal over to the agent.

use std::path::Path;
use std::process::Command;

use tracevault_cli::agent::Agent;
use tracevault_cli::config::TracevaultConfig;

use crate::agents::via_gateway::{GatewayLaunch, CLAUDE_SETTINGS_PREFIX};
use crate::billing::{Billing, BillingUse};
use crate::config::agent_config_key;
use crate::doctor::Session;
use crate::gateway::{self, KeyStore};

pub use crate::agents::via_gateway::shell_quote;

/// The command a launched agent runs to get its gateway key: this `visdom`,
/// asked for the key of one console and project, of the kind `billing`.
pub fn key_helper_command(
    visdom: &Path,
    console_url: &str,
    project_id: &str,
    billing: Billing,
) -> String {
    let kind = match billing {
        Billing::Project => String::new(),
        Billing::Subscription => format!(" --billing {}", billing.name()),
    };
    format!(
        "{} gateway-key --console {} --project {}{kind}",
        shell_quote(&visdom.to_string_lossy()),
        shell_quote(console_url),
        shell_quote(project_id)
    )
}

/// Where a launch keeps what must not be on the agent's command line: the
/// user's state directory (`$XDG_STATE_HOME`, `~/.local/state`), else
/// visdom's config directory.
pub fn private_dir(config_root: &Path) -> std::path::PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| config_root.to_path_buf())
        .join("visdom")
}

/// Who pays for a gateway session: the setting where it applies, else the
/// project.
pub fn session_billing(session: &Session) -> Billing {
    match session.billing_use {
        BillingUse::Applies => session.billing.billing,
        _ => Billing::Project,
    }
}

/// The HTTP client for getting a key: the agent does not start without it,
/// so it may take longer than a check.
pub fn key_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_default()
}

/// A gateway session ready to start.
pub struct Prepared {
    pub launch: GatewayLaunch,
    /// What the developer should know (no model alias to ask for).
    pub notes: Vec<String>,
    /// The model aliases the key may call.
    pub available: Vec<String>,
}

/// Everything the agent needs to run through the gateway: the developer's
/// key (the kept one, or a new one), a model alias and the ids that attribute
/// the session. The second value holds what the developer should know (no
/// model alias to ask for).
///
/// The LLM gateway is asked for its models with the key before anything is
/// started: an agent pointed at a gateway that does not answer, or does not
/// take the key, would run with nothing recording it. A kept key the gateway
/// refuses (a newer one was issued elsewhere, which revoked it) is replaced
/// once.
pub fn prepare_gateway(
    session: &Session,
    runtime: &tokio::runtime::Runtime,
    visdom: &Path,
    now: i64,
) -> Result<Prepared, String> {
    let plan = match &session.gateway {
        Some(Ok(plan)) => plan,
        Some(Err(why)) => return Err(why.clone()),
        None => return Err("this session is not in gateway mode".to_string()),
    };
    let token = session
        .token
        .as_deref()
        .ok_or("the AI Gateway needs a Visdom account login: run `visdom login`")?;
    let http = key_http_client();
    let billing = session_billing(session);
    let store = KeyStore::at(gateway::key_store_path_in(&session.config_root));
    let key_unless = |rejected: Option<&str>| {
        runtime.block_on(gateway::ensure_key(
            &http,
            &plan.console_url,
            token,
            &plan.project.id,
            billing,
            &store,
            now,
            rejected,
        ))
    };
    let models = |key: &str| runtime.block_on(gateway::models(&http, &plan.llm_url, key));
    let mut ensured = key_unless(None)?;
    let available = match models(&ensured.key.secret) {
        Ok(available) => available,
        Err(refusal) if refusal.rejected && !ensured.issued => {
            ensured = key_unless(Some(&ensured.key.secret))?;
            models(&ensured.key.secret).map_err(|e| e.why)?
        }
        Err(e) => return Err(e.why),
    };
    let key = ensured.key;
    let mut notes: Vec<String> = Vec::new();
    let model = gateway::pick_model(
        session.ctx.agent,
        session.gateway_model.as_deref(),
        &available,
    );
    if let Some(configured) = model
        .as_deref()
        .filter(|m| !available.iter().any(|a| a == m))
    {
        notes.push(format!(
            "the model alias `{configured}` is not among those your key may call in the project \
             `{}` ({}): the gateway will refuse it",
            plan.project.slug,
            available.join(", ")
        ));
    }
    if model.is_none() {
        notes.push(format!(
            "no AI Gateway model alias was chosen for {}: it asks for its own default, which \
             the project `{}` may not allow (set {}_model under [gateway] in your visdom config, \
             or VISDOM_GATEWAY_MODEL)",
            session.ctx.agent.label(),
            plan.project.slug,
            agent_config_key(session.ctx.agent)
        ));
    }
    // The repository a new session's events go under, as far as it is known
    // before the session exists: the checkout's own, else the user's default
    // (`tracevault repo switch --user`), as the capture hook has it.
    let repo_id = session
        .ctx
        .workdir
        .primary
        .as_deref()
        .and_then(|root| TracevaultConfig::try_load(root).ok().flatten())
        .and_then(|config| config.repo_id)
        .filter(|id| !id.trim().is_empty())
        .or_else(|| tracevault_cli::user_default::load().map(|binding| binding.repo_id))
        .filter(|id| !id.trim().is_empty());
    Ok(Prepared {
        launch: GatewayLaunch {
            llm_url: plan.llm_url.clone(),
            key: key.secret,
            key_helper: key_helper_command(visdom, &plan.console_url, &plan.project.id, billing),
            model,
            repo_id,
            session_id: uuid::Uuid::new_v4().to_string(),
            billing,
            private_dir: private_dir(&session.config_root),
        },
        notes,
        available,
    })
}

/// The environment variable that gives the gateway setting `key` of the user
/// config, for when the file cannot be written.
pub fn gateway_setting_env(key: &str) -> Option<&'static str> {
    match key {
        "project" => Some("export VISDOM_GATEWAY_PROJECT=<project>"),
        "url" => Some("export VISDOM_GATEWAY_URL=<console address>"),
        "billing" => Some("export VISDOM_GATEWAY_BILLING=<project|subscription>"),
        key if key.ends_with("_model") => Some("export VISDOM_GATEWAY_MODEL=<model alias>"),
        _ => None,
    }
}

/// Why there is no model alias for `agent` to ask the gateway for, and what
/// to set: without one the agent asks for its own default, which a project's
/// allow-list of aliases does not have.
pub fn no_model(agent: Agent, project: &str, available: &[String]) -> String {
    let key = format!("{}_model", agent_config_key(agent));
    let choice = match available.is_empty() {
        true => format!("the project `{project}` lets your key call no model alias at all"),
        false => format!(
            "none of the model aliases your key may call in the project `{project}` is named \
             for {} ({})",
            agent.label(),
            available.join(", ")
        ),
    };
    format!(
        "{choice}: set {key} under [gateway] in your visdom config, or VISDOM_GATEWAY_MODEL, to \
         the alias to use"
    )
}

/// Whether `model` is one the agent can ask the gateway for: an alias the
/// key may call. `Err` says what is wrong and what to set — no alias at all
/// ([`no_model`]), or one (set at `source`) that the project does not have.
pub fn usable_model(
    agent: Agent,
    model: Option<&str>,
    source: Option<&str>,
    project: &str,
    available: &[String],
) -> Result<(), String> {
    let Some(model) = model else {
        return Err(no_model(agent, project, available));
    };
    if available.iter().any(|alias| alias == model) {
        return Ok(());
    }
    let from = source.map(|s| format!(" ({s})")).unwrap_or_default();
    let have = match available.is_empty() {
        true => "it lets your key call none".to_string(),
        false => format!("your key may call: {}", available.join(", ")),
    };
    Err(format!(
        "the model alias `{model}`{from} is not one the project `{project}` has — {have}"
    ))
}

/// Where the gateway's settings are made, for somebody whose gateway is not
/// ready or who wants another one.
pub fn gateway_settings_hint(user_config: &Path) -> String {
    format!(
        "  Settings: `url`, `project`, `claude_code_model`, `codex_model`, `billing` under \
         [gateway] and `tracing_mode` in {}.\n  \
         Left out there, they are taken from the environment — VISDOM_GATEWAY_URL, \
         VISDOM_GATEWAY_PROJECT, VISDOM_GATEWAY_MODEL, VISDOM_GATEWAY_BILLING, \
         VISDOM_TRACING_MODE (which win over the file when set) — and the gateway's address \
         from the tracing server.",
        user_config.display()
    )
}

/// What `visdom init` set up for the gateway, and where each setting comes
/// from.
pub fn gateway_summary(
    agent: Agent,
    mode: &crate::mode::ResolvedMode,
    plan: &crate::components::gateway::GatewayPlan,
    sources: &crate::doctor::GatewaySources,
    model: Option<&str>,
    billing: &crate::billing::ResolvedBilling,
    user_config: &Path,
) -> String {
    use crate::mode::ModeReason;
    let mode_source = match mode.reason {
        ModeReason::Policy(policy) => format!(
            "required by the workspace's AI traffic policy, `{}`",
            policy.name()
        ),
        _ => "your choice".to_string(),
    };
    let or = |source: &Option<String>, otherwise: &str| {
        source.clone().unwrap_or_else(|| otherwise.to_string())
    };
    let rows = [
        ("mode", "gateway".to_string(), mode_source),
        (
            "console",
            plan.console_url.clone(),
            or(&sources.console_url, "not set"),
        ),
        (
            "LLM gateway",
            plan.llm_url.clone(),
            "named by the console".to_string(),
        ),
        (
            "project",
            plan.project.slug.clone(),
            or(&sources.project, "the only project open to you"),
        ),
        (
            "model",
            model.unwrap_or("the agent's own default").to_string(),
            match model {
                Some(_) => or(
                    &sources.model,
                    "the first alias the key may call that is named for the agent",
                ),
                None => "no alias is set, and the project may refuse the default".to_string(),
            },
        ),
        (
            "billing",
            match billing.billing {
                crate::billing::Billing::Project => "project (gateway key)",
                crate::billing::Billing::Subscription => "my Claude subscription",
            }
            .to_string(),
            billing.source.describe(),
        ),
        match billing.billing {
            crate::billing::Billing::Project => (
                "personal key",
                "issued, and accepted by the LLM gateway".to_string(),
                "kept for you only, renewed by `visdom`".to_string(),
            ),
            crate::billing::Billing::Subscription => (
                "key",
                "a subscription key, accepted by the LLM gateway".to_string(),
                "kept for you only, renewed by `visdom` a week before it expires".to_string(),
            ),
        },
    ];
    let width = rows
        .iter()
        .map(|(_, value, _)| value.len())
        .max()
        .unwrap_or(0);
    let mut out = format!(
        "AI Gateway ready for {}: `visdom` starts it through the gateway.\n",
        agent.label()
    );
    for (name, value, source) in rows {
        out.push_str(&format!("  {name:<13}{value:<width$}  ({source})\n"));
    }
    out.push_str(&gateway_settings_hint(user_config));
    out.push_str(
        "\n  To send an agent you start without `visdom` through the gateway as well:  \
         source <(visdom env)\n",
    );
    out
}

/// What to tell the user when the agent's executable cannot be started.
pub fn launch_error_message(
    program: &str,
    error: &std::io::Error,
    agent: Agent,
    user_config: &Path,
) -> String {
    format!(
        "visdom: cannot start {program}: {error} (set agents.{} in {} to use another executable)",
        agent_config_key(agent),
        user_config.display()
    )
}

/// Replace this process with the agent, so the terminal, signals and exit
/// code are the agent's. Returns only by exiting: 127 when the executable
/// cannot be started.
pub fn exec_agent(mut cmd: Command, agent: Agent, user_config: &Path) -> ! {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let error = replace_process(&mut cmd);
    eprintln!(
        "{}",
        launch_error_message(&program, &error, agent, user_config)
    );
    std::process::exit(127)
}

/// Run the agent as a child and exit with its status, removing
/// `private_file` (the settings that hold a key) once it has exited. Used
/// instead of [`exec_agent`] when there is such a file: a replaced process
/// could not remove it.
///
/// The terminal's interrupt and quit go to the agent, which shares this
/// process's group; here they are only waited out. A termination or hang-up
/// sent to `visdom` itself is passed on to the agent. Files left by a
/// `visdom` that did not get to remove its own (killed outright) are removed
/// first.
pub fn run_agent_then_remove(
    mut cmd: Command,
    private_file: std::path::PathBuf,
    agent: Agent,
    user_config: &Path,
) -> ! {
    if let Some(dir) = private_file.parent() {
        remove_stale_private_files(dir);
    }
    let program = cmd.get_program().to_string_lossy().into_owned();
    signals::wait_them_out();
    let status = cmd.spawn().and_then(|mut child| {
        signals::pass_on_to(child.id());
        child.wait()
    });
    let _ = std::fs::remove_file(&private_file);
    match status {
        Ok(status) => std::process::exit(exit_code(status)),
        Err(error) => {
            eprintln!(
                "{}",
                launch_error_message(&program, &error, agent, user_config)
            );
            std::process::exit(127)
        }
    }
}

/// The exit status a shell would report for `status`.
fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    status.code().unwrap_or(1)
}

/// Remove the Claude Code settings files in `dir` whose `visdom` process is
/// gone: they hold a key, and nothing else would remove them.
pub fn remove_stale_private_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let pid = name
            .strip_prefix(CLAUDE_SETTINGS_PREFIX)
            .and_then(|rest| rest.split_once('-'))
            .and_then(|(pid, _)| pid.parse::<u32>().ok());
        if pid.is_some_and(|pid| !signals::is_running(pid)) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(unix)]
mod signals {
    use std::sync::atomic::{AtomicI32, Ordering};

    static CHILD: AtomicI32 = AtomicI32::new(0);

    extern "C" fn on_signal(signal: libc::c_int) {
        if signal == libc::SIGTERM || signal == libc::SIGHUP {
            let child = CHILD.load(Ordering::Relaxed);
            if child > 0 {
                // SAFETY: kill is async-signal-safe.
                unsafe {
                    libc::kill(child, signal);
                }
            }
        }
    }

    /// Handle the signals that would end `visdom` before the agent: a
    /// handler (not "ignore") is reset to the default in the started agent.
    pub fn wait_them_out() {
        for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: a zeroed sigaction with a handler that only calls
            // async-signal-safe functions.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
                action.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
    }

    pub fn pass_on_to(child: u32) {
        CHILD.store(i32::try_from(child).unwrap_or(0), Ordering::Relaxed);
    }

    /// Whether process `pid` exists (or may: one of another user's).
    pub fn is_running(pid: u32) -> bool {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        // SAFETY: signal 0 only checks that the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        alive || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}

#[cfg(not(unix))]
mod signals {
    pub fn wait_them_out() {}
    pub fn pass_on_to(_child: u32) {}
    /// Not known here: a file is left rather than removed under a running
    /// session.
    pub fn is_running(_pid: u32) -> bool {
        true
    }
}

#[cfg(unix)]
fn replace_process(cmd: &mut Command) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    cmd.exec()
}

/// Without `exec`: run the agent as a child and exit with its status.
#[cfg(not(unix))]
fn replace_process(cmd: &mut Command) -> std::io::Error {
    match cmd.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_error_names_the_program_and_the_config_key() {
        let error = std::io::Error::new(std::io::ErrorKind::NotFound, "No such file");
        let message = launch_error_message(
            "claude",
            &error,
            Agent::ClaudeCode,
            Path::new("/home/u/.config/visdom/config.toml"),
        );
        assert!(
            message.contains("cannot start claude: No such file"),
            "{message}"
        );
        assert!(message.contains("agents.claude_code"), "{message}");
        assert!(
            message.contains("/home/u/.config/visdom/config.toml"),
            "{message}"
        );
        let message = launch_error_message("codex", &error, Agent::Codex, Path::new("/c"));
        assert!(message.contains("agents.codex"), "{message}");
    }

    #[test]
    fn the_key_helper_is_one_safe_shell_command() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(
            key_helper_command(
                Path::new("/opt/my tools/visdom"),
                "https://gw.example.com",
                "p-1; rm -rf ~",
                Billing::Project
            ),
            "'/opt/my tools/visdom' gateway-key --console 'https://gw.example.com' --project \
             'p-1; rm -rf ~'"
        );
        assert_eq!(
            key_helper_command(Path::new("/v"), "https://gw", "p-1", Billing::Subscription),
            "'/v' gateway-key --console 'https://gw' --project 'p-1' --billing subscription"
        );
    }

    #[cfg(unix)]
    #[test]
    fn settings_files_of_a_visdom_that_is_gone_are_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // A pid no process has (above Linux's and macOS's maximum).
        let gone = dir.join("claude-settings-4194304999-abc.json");
        let mine = dir.join(format!("claude-settings-{}-abc.json", std::process::id()));
        let other = dir.join("notes-1.json");
        for file in [&gone, &mine, &other] {
            std::fs::write(file, "{}").unwrap();
        }
        remove_stale_private_files(dir);
        assert!(!gone.exists());
        assert!(mine.exists(), "this process runs");
        assert!(other.exists(), "not a settings file");
        remove_stale_private_files(&dir.join("missing"));
    }

    #[cfg(unix)]
    #[test]
    fn a_shell_reports_a_signalled_agent_as_128_plus_the_signal() {
        let status = Command::new("sh")
            .arg("-c")
            .arg("kill -TERM $$")
            .status()
            .unwrap();
        assert_eq!(exit_code(status), 128 + 15);
        let status = Command::new("sh").arg("-c").arg("exit 3").status().unwrap();
        assert_eq!(exit_code(status), 3);
    }

    /// The helper command survives a real shell as the three values it holds.
    #[cfg(unix)]
    #[test]
    fn a_shell_reads_the_quoted_values_back() {
        let tricky = ["it's", "a b", "$HOME `id` \"x\"", "p-1; echo no"];
        for value in tricky {
            let output = Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {}", shell_quote(value)))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&output.stdout), value);
        }
    }
}
