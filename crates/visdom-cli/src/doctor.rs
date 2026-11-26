//! Gathering what the checks need from this machine, running them, and
//! carrying out the fixes the user agreed to.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tracevault_cli::agent::Agent;
use tracevault_cli::credentials::Credential;

use crate::agents::{adapter_for, AgentAdapter, AgentPaths, McpScope, Workdir};
use crate::billing::{self, billing_use, Billing, BillingUse, ResolvedBilling};
use crate::components::gateway::GatewayPlan;
use crate::components::{
    bazaar, gateway as gateway_checks, tracing, CheckContext, CredentialState,
};
use crate::config::{self, RepoConfig, UserConfig};
use crate::finding::{command_line, needs_attention, render_text, Finding, Fix, FixAction};
use crate::fixes::{apply_fixes, FixExecutor, FixOutcome, FixPolicy};
use crate::gateway;
use crate::instructions::{self, Facts, Told};
use crate::mode::{configured_mode, effective_mode, ResolvedMode, TracingMode, MODE_ENV};
use crate::subscription::{self, KeyState};
use crate::suite::{self, PolicyState, SuiteMap};

/// The first of `candidates` that is set to something.
fn first_set(candidates: [Option<String>; 3]) -> Option<String> {
    candidates
        .into_iter()
        .flatten()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .find(|v| !v.is_empty())
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

/// The directory whose `.visdom/config.toml` applies in `cwd`: the top of the
/// git checkout, else — outside any checkout, in a folder of checkouts set up
/// as a workspace — the nearest directory above that has the file.
pub fn settings_dir(cwd: &Path) -> Option<PathBuf> {
    git_toplevel(cwd).or_else(|| {
        cwd.ancestors()
            .find(|dir| config::repo_config_path_in(dir).is_file())
            .map(Path::to_path_buf)
    })
}

pub const GATEWAY_URL_ENV: &str = "VISDOM_GATEWAY_URL";
pub const GATEWAY_PROJECT_ENV: &str = "VISDOM_GATEWAY_PROJECT";
pub const GATEWAY_MODEL_ENV: &str = "VISDOM_GATEWAY_MODEL";

/// Where the gateway settings of a session come from, each said as a person
/// would read it (`VISDOM_GATEWAY_URL in the environment`, a config file's
/// key, `published by the tracing server`). `None`: nobody set it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GatewaySources {
    pub console_url: Option<String>,
    pub project: Option<String>,
    pub model: Option<String>,
}

/// The first value that is set, and where it was found.
fn first_from<const N: usize>(
    candidates: [(Option<String>, String); N],
) -> (Option<String>, Option<String>) {
    candidates
        .into_iter()
        .find_map(|(value, source)| {
            value
                .filter(|v| !v.trim().is_empty())
                .map(|v| (Some(v), Some(source)))
        })
        .unwrap_or((None, None))
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
    /// `VISDOM_TRACING_MODE`.
    pub mode: Option<String>,
    /// `VISDOM_GATEWAY_URL`: the AI Gateway console.
    pub gateway_url: Option<String>,
    /// `VISDOM_CONTROL_PLANE_URL`.
    pub control_plane_url: Option<String>,
    /// `VISDOM_GATEWAY_PROJECT`: the gateway project (slug, id or name).
    pub gateway_project: Option<String>,
    /// `VISDOM_GATEWAY_MODEL`: the model alias the agent asks the gateway for.
    pub gateway_model: Option<String>,
    /// `VISDOM_GATEWAY_BILLING`: who pays for the calls through the gateway.
    pub gateway_billing: Option<String>,
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
            mode: set(MODE_ENV),
            gateway_url: set("VISDOM_GATEWAY_URL"),
            control_plane_url: set("VISDOM_CONTROL_PLANE_URL"),
            gateway_project: set(GATEWAY_PROJECT_ENV),
            gateway_model: set(GATEWAY_MODEL_ENV),
            gateway_billing: set(billing::BILLING_ENV),
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
    /// How this session is traced, and who decided.
    pub mode: ResolvedMode,
    pub policy: PolicyState,
    /// In gateway mode: what the session will use, or why it cannot.
    pub gateway: Option<Result<GatewayPlan, String>>,
    /// The suite's other products, as far as the tracing server says.
    pub suite: SuiteMap,
    /// The developer's Keycloak access token, for the suite's other products.
    /// Never an API key: that is the tracing server's alone.
    pub token: Option<String>,
    /// The model alias configured for this agent through the gateway.
    pub gateway_model: Option<String>,
    /// Where a missing bazaar registration is made: for this project
    /// directory, or (a machine-wide setup) for all of the user's.
    pub mcp_scope: McpScope,
    /// The gateway console's address, as far as anybody names one.
    pub console_url: Option<String>,
    /// Who set the gateway's address, project and model.
    pub gateway_sources: GatewaySources,
    /// Where visdom keeps its own state (`<config dir>`).
    pub config_root: PathBuf,
    /// Whether the agent is told about Visdom's tools (`agent_instructions`
    /// in the user config; yes unless that says no).
    pub agent_instructions: bool,
    /// Who pays for the agent's calls through the gateway, and who said so.
    pub billing: ResolvedBilling,
    /// Whether that can be used in this session's mode with this agent.
    pub billing_use: BillingUse,
    /// What [`Session::check_subscription`] found; empty until it runs.
    pub subscription: Vec<Finding>,
}

impl Session {
    /// Read the config files, choose the agent and gather the check context.
    /// The errors are an agent that cannot be chosen or launched, and a mode
    /// or billing in the environment that names none.
    pub fn gather(
        env: &Environment,
        agent_flag: Option<Agent>,
        mode_flag: Option<TracingMode>,
        billing_flag: Option<Billing>,
        runtime: &tokio::runtime::Runtime,
    ) -> Result<Session, String> {
        let mut warnings = Vec::new();
        let project_dir = git_toplevel(&env.cwd);
        let user: UserConfig = config::load_user_config_in(&env.config_root).unwrap_or_else(|e| {
            warnings.push(e);
            UserConfig::default()
        });
        let repo = settings_dir(&env.cwd)
            .as_deref()
            .map(config::load_repo_config_in)
            .transpose()
            .unwrap_or_else(|e| {
                warnings.push(e);
                None
            })
            .unwrap_or_default();
        let repo: RepoConfig = repo;
        let agent = config::resolve_agent(agent_flag, env.agent.as_deref(), &repo, &user)?;
        let chosen = configured_mode(
            mode_flag,
            env.mode.as_deref(),
            repo.tracing_mode,
            user.tracing_mode,
        )?;
        let billing = billing::configured_billing(
            billing_flag,
            env.gateway_billing.as_deref(),
            repo.gateway.billing,
            user.gateway.billing,
            &config::user_config_path_in(&env.config_root)
                .display()
                .to_string(),
        )?;

        let tracing_root = project_dir
            .as_deref()
            .map(|dir| tracevault_cli::paths::resolve_project_root(dir).root);
        let credential_root = tracing_root.as_deref().unwrap_or(&env.cwd);
        let (server_url, credential, session) =
            match tracevault_cli::credentials::resolve_credentials_quiet(credential_root) {
                Ok((url, Some(credential))) => {
                    let session = match credential {
                        Credential::Keycloak(_) => Some(credential),
                        Credential::ApiKey(_) => None,
                    };
                    (url, CredentialState::Present, session)
                }
                Ok((url, None)) => (url, missing_or_unreadable(), None),
                Err(e) => (None, CredentialState::Unusable(e.to_string()), None),
            };

        let http = http_client();
        // One question to the tracing server tells where everything else is.
        let published = match &server_url {
            Some(url) => runtime.block_on(suite::fetch_suite_map(&http, url)),
            None => Ok(SuiteMap::default()),
        };
        let bazaar_url = bazaar::resolve_bazaar_url(env.bazaar_url.clone(), &user, &published);
        // While that server is away, the rest of the suite is where it last
        // said: a policy that requires the gateway must not lapse with it.
        let suite_map = match &server_url {
            Some(url) => suite::suite_map_or_remembered(
                &published,
                url,
                &suite::suite_cache_path_in(&env.config_root),
            ),
            None => SuiteMap::default(),
        };

        // The developer's own token opens the suite's other products. Only a
        // Keycloak login has one; an API key is never sent anywhere else.
        let token = match (&server_url, session) {
            (Some(url), Some(session)) => runtime
                .block_on(
                    tracevault_cli::api_client::ApiClient::with_credential(url, Some(session))
                        .bearer_valid_for(MIN_TOKEN_SECONDS),
                )
                .ok()
                .flatten(),
            _ => None,
        };
        let control_plane_url = first_set([
            env.control_plane_url.clone(),
            user.control_plane_url.clone(),
            suite_map.control_plane_url.clone(),
        ]);
        let policy = runtime.block_on(suite::resolve_policy(
            &http,
            control_plane_url.as_deref(),
            token.as_deref(),
            &suite::policy_cache_path_in(&env.config_root),
        ));
        let mode = effective_mode(chosen, policy.policy());
        let billing_use = billing_use(&billing, &mode, agent);
        // The environment first, then the files, then what the server says:
        // a setting left out of the config is taken from the environment.
        let user_file = config::user_config_path_in(&env.config_root);
        let in_env = |name: &str| format!("{name} in the environment");
        let in_user = |key: &str| format!("{key} in {}", user_file.display());
        let (console_url, console_source) = first_from([
            (env.gateway_url.clone(), in_env(GATEWAY_URL_ENV)),
            (user.gateway.url.clone(), in_user("url under [gateway]")),
            (
                suite_map.ai_gateway_url.clone(),
                "published by the tracing server".to_string(),
            ),
        ]);
        let (project, project_source) = first_from([
            (env.gateway_project.clone(), in_env(GATEWAY_PROJECT_ENV)),
            (
                repo.gateway.project.clone(),
                "project under [gateway] in .visdom/config.toml".to_string(),
            ),
            (
                user.gateway.project.clone(),
                in_user("project under [gateway]"),
            ),
        ]);
        let (gateway_model, model_source) = first_from([
            (env.gateway_model.clone(), in_env(GATEWAY_MODEL_ENV)),
            (
                user.gateway.model_for(agent),
                in_user(&format!(
                    "{}_model under [gateway]",
                    config::agent_config_key(agent)
                )),
            ),
        ]);
        let gateway = (mode.mode == TracingMode::Gateway).then(|| {
            runtime.block_on(plan_gateway(
                &http,
                console_url.clone(),
                token.as_deref(),
                project.as_deref(),
            ))
        });

        let paths = AgentPaths {
            home: env.home.clone(),
            codex_home: env.codex_home.clone(),
        };
        let adapter = adapter_for(agent, paths, user.agents.program_for(agent))
            .ok_or_else(|| format!("visdom cannot launch {}", agent.tool_name()))?;
        Ok(Session {
            ctx: CheckContext {
                mode: mode.mode,
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
            mode,
            policy,
            gateway,
            suite: suite_map,
            token,
            gateway_model,
            mcp_scope: McpScope::Local,
            console_url,
            gateway_sources: GatewaySources {
                console_url: console_source,
                project: project_source,
                model: model_source,
            },
            config_root: env.config_root.clone(),
            agent_instructions: user.agent_instructions.unwrap_or(true),
            billing,
            billing_use,
            subscription: Vec::new(),
        })
    }

    /// Whether the agent is to be started through the gateway on the
    /// developer's own Claude subscription.
    pub fn on_subscription(&self) -> bool {
        self.mode.mode == TracingMode::Gateway
            && self.billing_use == BillingUse::Applies
            && self.billing.billing == Billing::Subscription
    }

    /// The model alias a subscription session would ask for: the configured
    /// one, else the first one named for the agent among those the gateway
    /// takes a subscription call on.
    pub fn subscription_model(&self) -> Option<String> {
        let eligible = match &self.gateway {
            Some(Ok(plan)) => plan.project.subscription_aliases().unwrap_or_default(),
            _ => Vec::new(),
        };
        gateway::pick_model(self.ctx.agent, self.gateway_model.as_deref(), &eligible)
    }

    /// Check that a subscription session can work here (see
    /// [`crate::subscription`]); [`Session::collect`] then reports it.
    /// `model` is the alias it would ask for, and `key` what is known of its
    /// key (`None`: not checked here). Nothing when the session is not one.
    pub fn check_subscription(&mut self, model: Option<&str>, key: Option<&KeyState>) {
        self.subscription.clear();
        if !self.on_subscription() {
            return;
        }
        if let Some(Ok(plan)) = &self.gateway {
            self.subscription
                .push(subscription::traffic_finding(plan.subscription_traffic));
        }
        self.subscription
            .push(subscription::login_finding(&subscription::claude_login(
                self.adapter.program(),
            )));
        if let Some(Ok(plan)) = &self.gateway {
            self.subscription.push(subscription::model_finding(
                model,
                self.gateway_sources.model.as_deref(),
                &plan.project,
                &format!("{}_model", config::agent_config_key(self.ctx.agent)),
            ));
        }
        if let Some(key) = key {
            self.subscription.push(subscription::key_finding(key));
        }
        self.subscription.push(subscription::settings_finding(
            &subscription::managed_settings_path(),
            &self.ctx.home,
            self.ctx.workdir.checkout.as_deref(),
        ));
    }

    /// The kept subscription key, tried on the LLM gateway; none is issued
    /// (`visdom doctor` changes nothing at the gateway).
    pub fn kept_subscription_key(&self, runtime: &tokio::runtime::Runtime, now: i64) -> KeyState {
        let (Some(Ok(plan)), Some(token)) = (&self.gateway, &self.token) else {
            return KeyState::NoneKept;
        };
        let store = gateway::KeyStore::at(gateway::key_store_path_in(&self.config_root));
        let kept = store.usable(
            &plan.console_url,
            &plan.project.id,
            &gateway::account_of(token),
            Billing::Subscription,
            now,
        );
        let Some(key) = kept else {
            return KeyState::NoneKept;
        };
        match runtime.block_on(gateway::models(&http_client(), &plan.llm_url, &key.secret)) {
            Ok(_) => KeyState::Accepted {
                expires_at: key.expires_at,
            },
            Err(refusal) if refusal.rejected => KeyState::KeptRefused,
            Err(refusal) => KeyState::Failed(refusal.why),
        }
    }

    /// Whether a subscription session cannot work here, as far as
    /// [`Session::check_subscription`] found.
    pub fn subscription_blocked(&self) -> bool {
        self.subscription
            .iter()
            .any(|f| f.severity == crate::finding::Severity::Error)
    }

    /// What an agent started now would have of Visdom's, `through_gateway`
    /// saying whether its LLM traffic goes through the AI Gateway.
    pub fn facts(&self, through_gateway: bool) -> Facts {
        Facts {
            tracing: tracing::working(&self.ctx),
            bazaar: bazaar::registered_name(
                self.bazaar_url.as_ref().ok().and_then(|url| url.as_deref()),
                self.adapter.as_ref(),
                &self.ctx.workdir,
            ),
            gateway: through_gateway,
        }
    }

    /// `args` for the agent, with what it is told about Visdom's tools, if
    /// it is told anything.
    pub fn agent_args(&self, args: &[String], through_gateway: bool) -> Told {
        if !self.agent_instructions {
            return Told::Nothing(args.to_vec());
        }
        let mut facts = self.facts(through_gateway);
        // The caller's own options may take the registered servers away from
        // this one session.
        let agent = self.ctx.agent;
        // What was found is of the directory `visdom` was run in: a Codex
        // sent to work in another one is told nothing of it.
        if agent == Agent::Codex && instructions::codex_works_elsewhere(args) {
            return Told::Nothing(args.to_vec());
        }
        facts.tracing &= !instructions::without_hooks(agent, args);
        let profile_changes_mcp = agent == Agent::Codex
            && instructions::codex_profile_changes_mcp(args, &self.ctx.codex_home);
        facts.bazaar = facts.bazaar.filter(|name| {
            !profile_changes_mcp && !instructions::without_registered_mcp(agent, args, name)
        });
        match instructions::compose(&facts) {
            Some(text) => instructions::agent_args(
                self.ctx.agent,
                &text,
                args,
                &self.ctx.codex_home,
                &self.ctx.workdir.start_dir,
            ),
            None => Told::Nothing(args.to_vec()),
        }
    }

    /// Tracing, then the mode and the gateway, then bazaar.
    pub fn collect(&self) -> Vec<Finding> {
        let mut findings = tracing::check(&self.ctx);
        findings.extend(gateway_checks::check(
            &self.mode,
            &self.policy,
            self.gateway.as_ref(),
            self.ctx.agent,
        ));
        findings.extend(gateway_checks::billing_finding(
            &self.billing,
            &self.billing_use,
        ));
        findings.extend(self.subscription.iter().cloned());
        findings.extend(bazaar::check(
            self.bazaar_url
                .as_ref()
                .map(|url| url.as_deref())
                .map_err(String::as_str),
            self.adapter.as_ref(),
            &self.ctx.workdir,
            &self.user_config_path,
            self.mcp_scope,
        ));
        findings
    }
}

/// The developer's Keycloak access token for the project containing
/// `start`, refreshed if needed. An error says why there is none.
pub fn person_token(start: &Path, runtime: &tokio::runtime::Runtime) -> Result<String, String> {
    let root = git_toplevel(start)
        .map(|dir| tracevault_cli::paths::resolve_project_root(&dir).root)
        .unwrap_or_else(|| start.to_path_buf());
    let login = "the AI Gateway needs a Visdom account login: run `visdom login`";
    match tracevault_cli::credentials::resolve_credentials_quiet(&root) {
        Ok((Some(url), Some(session @ Credential::Keycloak(_)))) => runtime
            .block_on(
                tracevault_cli::api_client::ApiClient::with_credential(&url, Some(session))
                    .bearer_valid_for(MIN_TOKEN_SECONDS),
            )
            .map_err(|e| format!("your Visdom login could not be used ({e}): run `visdom login`"))?
            .ok_or_else(|| login.to_string()),
        Ok(_) => Err(login.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// The account of the login stored for the project containing `start`, read
/// from the token as it is on disk: nobody is asked. `None` without a Visdom
/// account login.
pub fn stored_account(start: &Path) -> Option<String> {
    let root = git_toplevel(start)
        .map(|dir| tracevault_cli::paths::resolve_project_root(&dir).root)
        .unwrap_or_else(|| start.to_path_buf());
    match tracevault_cli::credentials::resolve_credentials_quiet(&root) {
        Ok((_, Some(Credential::Keycloak(session)))) => {
            Some(gateway::account_of(&session.access_token))
        }
        _ => None,
    }
}

/// How long the token handed to the suite's other products must still be
/// valid: enough for the few calls of one run.
const MIN_TOKEN_SECONDS: i64 = 60;

/// The HTTP client for the questions asked before an agent starts: nothing
/// here may hold the developer up for long.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap_or_default()
}

/// What a session through the gateway will use: the console, the LLM gateway
/// behind it and the project whose key the agent gets. Nothing is issued.
pub async fn plan_gateway(
    http: &reqwest::Client,
    console_url: Option<String>,
    token: Option<&str>,
    project: Option<&str>,
) -> Result<GatewayPlan, String> {
    let console_url = console_url.ok_or(
        "this installation names no AI Gateway: set url under [gateway] in your visdom config, \
         or VISDOM_GATEWAY_URL, to the gateway console's address",
    )?;
    if !bazaar::is_http_url(&console_url) {
        return Err(format!(
            "the AI Gateway address `{console_url}` is not an http(s) URL"
        ));
    }
    let token = token.ok_or(
        "the AI Gateway needs a Visdom account login (an API key does not work there): run \
         `visdom login`",
    )?;
    let settings = gateway::console_settings(http, &console_url, token).await?;
    let open = gateway::open_projects(http, &console_url, token).await?;
    let project = gateway::pick_project(project, &open)?;
    Ok(GatewayPlan {
        console_url,
        llm_url: settings.llm_url,
        project,
        subscription_traffic: settings.subscription_traffic,
    })
}

/// No credential was resolved: either there is none, or the credential file
/// is there and cannot be read or parsed (the library treats both alike).
fn missing_or_unreadable() -> CredentialState {
    use tracevault_cli::credentials::Credentials;
    match Credentials::existing_path() {
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
    fn settings_outside_a_checkout_are_the_nearest_folders_above() {
        let tmp = tempfile::tempdir().unwrap();
        let below = tmp.path().join("a").join("b");
        std::fs::create_dir_all(&below).unwrap();
        assert_eq!(settings_dir(&below), None);
        std::fs::create_dir_all(tmp.path().join(".visdom")).unwrap();
        std::fs::write(config::repo_config_path_in(tmp.path()), "").unwrap();
        assert_eq!(settings_dir(&below).as_deref(), Some(tmp.path()));
        // A checkout's own top level wins, with or without the file.
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&below)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(settings_dir(&below), git_toplevel(&below));
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
