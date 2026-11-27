//! Whether a Claude Code session paid by the developer's own Claude
//! subscription (`billing = subscription`) can work through this deployment's
//! AI Gateway, checked before it is started and at the end of `visdom init`:
//!
//! 1. the gateway takes subscription traffic (`gateway.subscription`),
//! 2. Claude Code is signed in with a claude.ai subscription
//!    (`gateway.claude_login`),
//! 3. the model alias is one the gateway takes a subscription call on
//!    (`gateway.subscription_model`),
//! 4. a subscription key is issued (or kept) and the gateway accepts it
//!    (`gateway.subscription_key`),
//! 5. nothing in Claude Code's settings takes the session over from that
//!    login (`gateway.claude_settings`).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::finding::{Finding, Severity};
use crate::gateway::{OpenProject, SubscriptionTraffic};

const COMPONENT: &str = "gateway";

/// What the developer is told when the platform does not take subscription
/// traffic: printed word for word (the product owner's wording).
pub const NOT_SUPPORTED: &str = "Your platform configuration doesn't support subscription traffic \
                                 through visdom gateway. Contact your platform administrator or \
                                 use Claude API key or agent hooks";

/// The commands behind the two ways out [`NOT_SUPPORTED`] names.
pub const ALTERNATIVES: &str = "Claude API key (the project pays): visdom --billing project init; \
                                agent hooks: visdom --mode hooks init";

/// Whether `why` is [`NOT_SUPPORTED`], which is then printed on its own.
pub fn is_not_supported(why: &str) -> bool {
    why.starts_with(NOT_SUPPORTED)
}

/// `gateway.subscription`: whether the gateway takes subscription traffic.
/// Denied, or not said (a gateway from before the setting), is
/// [`NOT_SUPPORTED`] on the first line.
pub fn traffic_finding(traffic: SubscriptionTraffic) -> Finding {
    const ID: &str = "gateway.subscription";
    let why = match traffic {
        SubscriptionTraffic::Allowed => {
            return Finding::new(
                COMPONENT,
                ID,
                Severity::Ok,
                "the AI Gateway takes calls paid by a Claude subscription",
            )
        }
        SubscriptionTraffic::Denied => "the AI Gateway is set to deny subscription traffic",
        SubscriptionTraffic::Unpublished => {
            "the AI Gateway does not say that it takes subscription traffic (a version from \
             before it could)"
        }
    };
    Finding::new(
        COMPONENT,
        ID,
        Severity::Error,
        format!("{NOT_SUPPORTED}\n      ({why}) {ALTERNATIVES}"),
    )
}

/// How Claude Code is signed in, as `claude auth status` says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaudeLogin {
    /// A claude.ai login, with its plan when Claude Code names it.
    Subscription {
        plan: Option<String>,
    },
    SignedOut,
    /// Signed in another way (an Anthropic Console key, a cloud provider).
    Other {
        method: String,
    },
    /// Could not be told; the text says why.
    Unknown(String),
}

/// Read what `claude auth status` printed. A claude.ai login stored beside
/// a key that takes precedence elsewhere (a key helper in the developer's
/// settings, which `visdom` sets aside) still counts as the login.
pub fn read_auth_status(stdout: &str) -> ClaudeLogin {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Status {
        #[serde(default)]
        logged_in: bool,
        #[serde(default)]
        auth_method: Option<String>,
        #[serde(default)]
        subscription_type: Option<String>,
        #[serde(default)]
        stored_claude_ai_login: bool,
    }
    let Ok(status) = serde_json::from_str::<Status>(stdout.trim()) else {
        return ClaudeLogin::Unknown(
            "`claude auth status` did not answer in JSON (a Claude Code too old for it?)"
                .to_string(),
        );
    };
    let method = status.auth_method.unwrap_or_default();
    let plan = status
        .subscription_type
        .clone()
        .filter(|p| !p.trim().is_empty());
    // A claude.ai account without a paid plan has no subscription to pay
    // with. (A plan Claude Code does not name is taken as one: it names it
    // for every claude.ai login it can use.)
    if status.logged_in
        && method == "claude.ai"
        && plan
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case("free"))
    {
        return ClaudeLogin::Other {
            method: "a claude.ai account without a paid plan".to_string(),
        };
    }
    if status.logged_in && (method == "claude.ai" || status.stored_claude_ai_login) {
        return ClaudeLogin::Subscription {
            plan: status.subscription_type.filter(|p| !p.trim().is_empty()),
        };
    }
    if !status.logged_in || method.is_empty() || method == "none" {
        return ClaudeLogin::SignedOut;
    }
    ClaudeLogin::Other { method }
}

/// How long `claude auth status` may take.
const AUTH_STATUS_TIMEOUT: Duration = Duration::from_secs(10);

/// Ask Claude Code (`program`) how it is signed in, in the environment a
/// session `visdom` starts would have: without the keys and provider
/// switches the launch removes, which would otherwise be reported instead of
/// the login.
pub fn claude_login(program: &str) -> ClaudeLogin {
    // The settings the launch sets aside are set aside here too: a key
    // helper in the developer's settings would otherwise be reported
    // (`authMethod: api_key_helper`, Claude Code 2.1.332, tried by hand),
    // not the login the session uses.
    let mut overrides = serde_json::Map::new();
    for name in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"] {
        overrides.insert(name.to_string(), "".into());
    }
    for switch in crate::agents::via_gateway::CLAUDE_OTHER_PROVIDERS {
        overrides.insert(switch.to_string(), "0".into());
    }
    let settings = serde_json::json!({ "apiKeyHelper": "", "env": overrides }).to_string();
    let mut cmd = Command::new(program);
    cmd.arg("--settings")
        .arg(settings)
        .args(["auth", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for name in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"]
        .into_iter()
        .chain(crate::agents::via_gateway::CLAUDE_OTHER_PROVIDERS)
    {
        cmd.env_remove(name);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return ClaudeLogin::Unknown(format!("`{program} auth status` could not be run: {e}"))
        }
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < AUTH_STATUS_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(25))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return ClaudeLogin::Unknown(format!(
                    "`{program} auth status` did not answer within {} seconds",
                    AUTH_STATUS_TIMEOUT.as_secs()
                ));
            }
        }
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        use std::io::Read;
        let _ = stdout.read_to_string(&mut out);
    }
    // Signed out, it exits 1 and still prints its JSON.
    read_auth_status(&out)
}

/// `gateway.claude_login`: whether Claude Code has the claude.ai login the
/// subscription is paid through.
pub fn login_finding(login: &ClaudeLogin) -> Finding {
    const ID: &str = "gateway.claude_login";
    const SIGN_IN: &str = "start `claude` and run /login with your claude.ai account";
    match login {
        ClaudeLogin::Subscription { plan } => Finding::new(
            COMPONENT,
            ID,
            Severity::Ok,
            match plan {
                Some(plan) => {
                    format!("Claude Code is signed in with a claude.ai subscription ({plan})")
                }
                None => "Claude Code is signed in with a claude.ai subscription".to_string(),
            },
        ),
        ClaudeLogin::SignedOut => Finding::new(
            COMPONENT,
            ID,
            Severity::Error,
            format!(
                "Claude Code is not signed in, so there is no subscription to pay for its calls: \
                 {SIGN_IN}"
            ),
        ),
        ClaudeLogin::Other { method } => Finding::new(
            COMPONENT,
            ID,
            Severity::Error,
            format!(
                "Claude Code is signed in with {}, not a claude.ai subscription: {SIGN_IN}",
                match method.as_str() {
                    "api_key" => "an API key (Anthropic Console)".to_string(),
                    said if said.starts_with("a ") => said.to_string(),
                    other => format!("`{other}`"),
                }
            ),
        ),
        // Nothing shows a subscription to pay with: not a setup to accept.
        ClaudeLogin::Unknown(why) => Finding::new(
            COMPONENT,
            ID,
            Severity::Error,
            format!(
                "whether Claude Code is signed in with a claude.ai subscription could not be \
                 checked: {why}. Make sure Claude Code is installed and recent (`claude \
                 update`), then {SIGN_IN}"
            ),
        ),
    }
}

/// A model alias Claude Code will ask for, and who set it (`None`: the
/// alias `visdom` configured or picked).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelUse {
    pub model: Option<String>,
    pub source: Option<String>,
}

/// The `env` of every Claude Code settings layer that applies over the
/// environment a session is started in, the one that wins first: the
/// settings an organisation manages, the caller's own `--settings` (the
/// later one first), the project's `.claude/settings.local.json` and
/// `.claude/settings.json`, the user's `settings.json`. `visdom`'s launch
/// sets the model as environment, so any of these that names one wins.
pub struct ClaudeEnv {
    layers: Vec<(String, serde_json::Map<String, serde_json::Value>)>,
}

impl ClaudeEnv {
    pub fn read(
        managed: &Path,
        home: &Path,
        project_dir: Option<&Path>,
        agent_args: &[String],
    ) -> ClaudeEnv {
        let mut files: Vec<(String, Option<serde_json::Map<String, serde_json::Value>>)> =
            vec![(managed.display().to_string(), read_json(managed))];
        // Merged as the launch merges them: a later one wins key by key, so
        // its `env` replaces an earlier `env` as a whole.
        let (given, _) = crate::agents::via_gateway::split_settings(agent_args);
        if !given.is_empty() {
            let mut merged = serde_json::Map::new();
            for value in &given {
                if let Ok(settings) = crate::agents::via_gateway::read_settings(value) {
                    merged.extend(settings);
                }
            }
            files.push(("--settings given to Claude Code".to_string(), Some(merged)));
        }
        if let Some(dir) = project_dir {
            for name in [".claude/settings.local.json", ".claude/settings.json"] {
                let path = dir.join(name);
                files.push((path.display().to_string(), read_json(&path)));
            }
        }
        let user = claude_config_dir(home).join("settings.json");
        files.push((user.display().to_string(), read_json(&user)));
        let layers = files
            .into_iter()
            .filter_map(|(at, settings)| {
                let env = settings?.get("env")?.as_object()?.clone();
                Some((at, env))
            })
            .collect();
        ClaudeEnv { layers }
    }

    /// The value of `name` a session would see, and where it comes from:
    /// a settings layer, else `process` (the environment).
    pub fn get(
        &self,
        name: &str,
        process: &dyn Fn(&str) -> Option<String>,
    ) -> Option<(String, String)> {
        self.layers
            .iter()
            .find_map(|(at, env)| {
                env.get(name)
                    .and_then(|v| v.as_str())
                    .map(|v| (v.to_string(), format!("`env` in {at}")))
            })
            .or_else(|| process(name).map(|v| (v, "the environment".to_string())))
    }
}

/// The aliases a session started now would ask for: the main model (the
/// caller's `--model`, else `ANTHROPIC_MODEL` as the session sees it — a
/// settings layer, else the environment, which the launch keeps — else
/// `configured`) and the small one (`ANTHROPIC_DEFAULT_HAIKU_MODEL`, else
/// `configured`). `env` gives a variable's value and where it is set.
pub fn models_in_use(
    configured: Option<&str>,
    agent_args: &[String],
    env: &dyn Fn(&str) -> Option<(String, String)>,
) -> Vec<ModelUse> {
    let set = |v: Option<String>| v.filter(|v| !v.trim().is_empty());
    let named = |name: &str| {
        env(name)
            .filter(|(v, _)| !v.trim().is_empty())
            .map(|(model, at)| ModelUse {
                model: Some(model),
                source: Some(format!("{name} in {at}")),
            })
    };
    let from_args = agent_args
        .iter()
        .take_while(|a| *a != "--")
        .enumerate()
        .find_map(|(i, arg)| {
            if arg == "--model" {
                agent_args.get(i + 1).cloned()
            } else {
                arg.strip_prefix("--model=").map(String::from)
            }
        });
    let ours = || ModelUse {
        model: configured.map(String::from),
        source: None,
    };
    let main = match set(from_args) {
        Some(model) => ModelUse {
            model: Some(model),
            source: Some("--model given to Claude Code".to_string()),
        },
        None => named("ANTHROPIC_MODEL").unwrap_or_else(ours),
    };
    let small = named("ANTHROPIC_DEFAULT_HAIKU_MODEL").unwrap_or_else(ours);
    let mut uses = vec![main];
    // Background calls, the model aliases and subagents may each name
    // another model; the launch keeps whatever names one.
    let others = OTHER_MODEL_SETTINGS.iter().filter_map(|name| named(name));
    for used in std::iter::once(small).chain(others) {
        if !uses.contains(&used) {
            uses.push(used);
        }
    }
    uses
}

/// Claude Code's other settings that name a model a session may ask for,
/// beside `ANTHROPIC_MODEL` and `ANTHROPIC_DEFAULT_HAIKU_MODEL`. The list is
/// Claude Code's: a setting it gains has to be added here.
pub const OTHER_MODEL_SETTINGS: [&str; 4] = [
    "ANTHROPIC_SMALL_FAST_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "CLAUDE_CODE_SUBAGENT_MODEL",
];

/// `gateway.subscription_model`: whether every alias of `uses` is one the
/// gateway takes a subscription call on, in `project`. `source` is where
/// `visdom`'s own alias was set; `config_key` is the setting that names it.
/// A gateway that does not publish which aliases qualify cannot be relied on
/// to keep a subscription call off another provider: an error too.
pub fn model_finding(
    uses: &[ModelUse],
    source: Option<&str>,
    project: &OpenProject,
    config_key: &str,
) -> Finding {
    const ID: &str = "gateway.subscription_model";
    let Some(eligible) = project.subscription_aliases() else {
        return Finding::new(
            COMPONENT,
            ID,
            Severity::Error,
            format!(
                "the AI Gateway does not say which model aliases of the project `{}` take calls \
                 paid by a Claude subscription (a version from before it could), so none can be \
                 relied on: ask your platform administrator to update it, or use --billing \
                 project",
                project.slug
            ),
        );
    };
    for used in uses {
        let finding = one_model_finding(
            used.model.as_deref(),
            used.source.as_deref().or(source),
            used.source.is_some(),
            &eligible,
            project,
            config_key,
        );
        if finding.severity != Severity::Ok {
            return finding;
        }
    }
    let names: Vec<String> = uses
        .iter()
        .filter_map(|u| u.model.as_deref())
        .map(|m| format!("`{m}`"))
        .collect();
    Finding::new(
        COMPONENT,
        ID,
        Severity::Ok,
        format!(
            "the model alias {} takes calls paid by a Claude subscription",
            names.join(" and ")
        ),
    )
}

/// [`model_finding`] for one alias. `outside` says it was set outside
/// visdom's config (the environment, the agent's options).
fn one_model_finding(
    model: Option<&str>,
    source: Option<&str>,
    outside: bool,
    eligible: &[String],
    project: &OpenProject,
    config_key: &str,
) -> Finding {
    const ID: &str = "gateway.subscription_model";
    let set = match (outside, source) {
        (true, Some(source)) => format!("change {source}"),
        _ => format!(
            "set {config_key} under [gateway] in your visdom config, or VISDOM_GATEWAY_MODEL"
        ),
    };
    if eligible.is_empty() {
        return Finding::new(
            COMPONENT,
            ID,
            Severity::Error,
            format!(
                "no model alias of the project `{}` takes calls paid by a Claude subscription \
                 (only aliases served by Anthropic directly do): ask a project operator for one, \
                 or use --billing project",
                project.slug
            ),
        );
    }
    let names = eligible.join(", ");
    match model {
        Some(model) if eligible.iter().any(|alias| alias == model) => Finding::new(
            COMPONENT,
            ID,
            Severity::Ok,
            format!("the model alias `{model}` takes calls paid by a Claude subscription"),
        ),
        Some(model) => Finding::new(
            COMPONENT,
            ID,
            Severity::Error,
            format!(
                "the model alias `{model}`{} does not take calls paid by a Claude subscription \
                 (only aliases served by Anthropic directly do; the gateway refuses it): {set} to \
                 one that does — {names}",
                source.map(|s| format!(" ({s})")).unwrap_or_default()
            ),
        ),
        None => Finding::new(
            COMPONENT,
            ID,
            Severity::Error,
            format!(
                "no model alias is set, and Claude Code's own default is not one the gateway \
                 takes a subscription call on: {set} to one that does — {names}"
            ),
        ),
    }
}

/// The aliases of `available` the gateway takes a subscription call on in
/// `project`; none when it does not say.
pub fn eligible_of(project: &OpenProject, available: &[String]) -> Vec<String> {
    let eligible = project.subscription_aliases().unwrap_or_default();
    available
        .iter()
        .filter(|alias| eligible.contains(alias))
        .cloned()
        .collect()
}

/// The day (UTC) Unix seconds `at` fall on.
fn day(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| at.to_string())
}

/// The state of the subscription key, as far as it was checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyState {
    /// The gateway took it (`GET /v1/models`); it expires at these Unix
    /// seconds.
    Accepted { expires_at: i64 },
    /// None is kept yet, and none was issued here (`visdom doctor`).
    NoneKept,
    /// The kept one was refused; `visdom` issues another when it starts.
    KeptRefused,
    /// None could be had, or the gateway refused it; the text says why.
    Failed(String),
}

/// `gateway.subscription_key`, with the expiry date.
pub fn key_finding(state: &KeyState) -> Finding {
    const ID: &str = "gateway.subscription_key";
    match state {
        KeyState::Accepted { expires_at } => Finding::new(
            COMPONENT,
            ID,
            Severity::Ok,
            format!(
                "your subscription key is accepted by the AI Gateway; it expires on {} and \
                 `visdom` renews it a week before",
                day(*expires_at)
            ),
        ),
        KeyState::NoneKept => Finding::new(
            COMPONENT,
            ID,
            Severity::Info,
            "no subscription key is kept yet: `visdom` gets one when it starts Claude Code",
        ),
        KeyState::KeptRefused => Finding::new(
            COMPONENT,
            ID,
            Severity::Warn,
            "the AI Gateway no longer accepts your kept subscription key: `visdom` gets a new \
             one when it starts Claude Code",
        ),
        KeyState::Failed(why) => Finding::new(COMPONENT, ID, Severity::Error, why.clone()),
    }
}

/// Where Claude Code reads the settings an organisation manages, which come
/// after everything `visdom` passes. `VISDOM_CLAUDE_MANAGED_SETTINGS` names
/// another file (an unusual installation, and the tests).
pub fn managed_settings_path() -> PathBuf {
    if let Some(path) = std::env::var_os("VISDOM_CLAUDE_MANAGED_SETTINGS") {
        return PathBuf::from(path);
    }
    if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json")
    } else if cfg!(windows) {
        PathBuf::from(r"C:\Program Files\ClaudeCode\managed-settings.json")
    } else {
        PathBuf::from("/etc/claude-code/managed-settings.json")
    }
}

/// Claude Code's own settings directory: `CLAUDE_CONFIG_DIR`, else
/// `~/.claude`.
fn claude_config_dir(home: &Path) -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"))
}

fn read_json(path: &Path) -> Option<serde_json::Map<String, serde_json::Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&text) {
        Ok(serde_json::Value::Object(map)) => Some(map),
        _ => None,
    }
}

fn is_set(value: Option<&serde_json::Value>) -> bool {
    value
        .and_then(|v| v.as_str())
        .is_some_and(|v| !v.trim().is_empty())
}

/// What in the managed settings `settings` takes a subscription session
/// over: they come after `visdom`'s own, so it cannot set them aside.
pub fn managed_takeovers(settings: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let mut found = Vec::new();
    if is_set(settings.get("apiKeyHelper")) {
        found.push("`apiKeyHelper` (its key is sent instead of your subscription)".to_string());
    }
    if let Some(method) = settings.get("forceLoginMethod").and_then(|v| v.as_str()) {
        if method != "claudeai" {
            found.push(format!(
                "`forceLoginMethod = {method}` (a claude.ai login is not allowed)"
            ));
        }
    }
    let env = settings.get("env").and_then(|v| v.as_object());
    // An empty key is no key; an empty address or header list still
    // replaces visdom's (no address is Anthropic's own; no headers, no key).
    for (name, what, even_empty) in [
        (
            "ANTHROPIC_API_KEY",
            "its key is sent instead of your subscription",
            false,
        ),
        (
            "ANTHROPIC_AUTH_TOKEN",
            "its token is sent instead of your subscription",
            false,
        ),
        (
            "ANTHROPIC_BASE_URL",
            "the session is sent past the gateway",
            true,
        ),
        (
            "ANTHROPIC_CUSTOM_HEADERS",
            "they replace the headers that carry your subscription key",
            true,
        ),
    ] {
        let value = env.and_then(|env| env.get(name));
        if is_set(value) || (even_empty && value.is_some()) {
            found.push(format!("`{name}` in `env` ({what})"));
        }
    }
    // A switch to another provider's API outranks the gateway's address;
    // visdom turns them off ("0"), but managed settings come after it.
    for switch in crate::agents::via_gateway::CLAUDE_OTHER_PROVIDERS {
        let on = env
            .and_then(|env| env.get(switch))
            .map(|v| match v {
                serde_json::Value::String(s) => {
                    !matches!(s.trim().to_ascii_lowercase().as_str(), "" | "0" | "false")
                }
                serde_json::Value::Bool(b) => *b,
                serde_json::Value::Number(n) => n.as_f64() != Some(0.0),
                _ => false,
            })
            .unwrap_or(false);
        if on {
            found.push(format!(
                "`{switch}` in `env` (the session is sent to another provider, past the gateway)"
            ));
        }
    }
    found
}

/// `gateway.claude_settings`: nothing in Claude Code's settings takes the
/// session over from the claude.ai login. Settings an organisation manages
/// that would are an error; a key helper in the developer's own settings is
/// set aside in sessions `visdom` starts, but not in a Claude Code started
/// without it (`source <(visdom env)` cannot), so it is a warning.
/// `project_dir` is the checkout whose `.claude/` settings apply.
pub fn settings_finding(managed: &Path, home: &Path, project_dir: Option<&Path>) -> Finding {
    const ID: &str = "gateway.claude_settings";
    if let Some(settings) = read_json(managed) {
        let found = managed_takeovers(&settings);
        if !found.is_empty() {
            return Finding::new(
                COMPONENT,
                ID,
                Severity::Error,
                format!(
                    "the Claude Code settings your organisation manages ({}) set {}: they come \
                     after visdom's, so a session on your subscription cannot work. Ask whoever \
                     manages them, or use --billing project",
                    managed.display(),
                    found.join(", ")
                ),
            );
        }
    }
    let mut own = vec![claude_config_dir(home).join("settings.json")];
    if let Some(dir) = project_dir {
        own.push(dir.join(".claude/settings.json"));
        own.push(dir.join(".claude/settings.local.json"));
    }
    let helpers: Vec<String> = own
        .iter()
        .filter(|path| read_json(path).is_some_and(|s| is_set(s.get("apiKeyHelper"))))
        .map(|path| path.display().to_string())
        .collect();
    if !helpers.is_empty() {
        return Finding::new(
            COMPONENT,
            ID,
            Severity::Warn,
            format!(
                "`apiKeyHelper` in {}: set aside in sessions `visdom` starts, but a Claude Code \
                 started without it (after `source <(visdom env)`) sends that key instead of \
                 your subscription",
                helpers.join(", ")
            ),
        );
    }
    Finding::new(
        COMPONENT,
        ID,
        Severity::Ok,
        "nothing in Claude Code's settings takes the session over from your claude.ai login",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::ProjectModel;

    #[test]
    fn the_platforms_message_is_the_product_owners_word_for_word() {
        assert_eq!(
            NOT_SUPPORTED,
            "Your platform configuration doesn't support subscription traffic through visdom \
             gateway. Contact your platform administrator or use Claude API key or agent hooks"
        );
        for traffic in [
            SubscriptionTraffic::Denied,
            SubscriptionTraffic::Unpublished,
        ] {
            let finding = traffic_finding(traffic);
            assert_eq!(finding.severity, Severity::Error);
            assert_eq!(finding.message.lines().next(), Some(NOT_SUPPORTED));
            assert!(finding.message.contains("--billing project"), "{finding:?}");
            assert!(finding.message.contains("--mode hooks"), "{finding:?}");
            assert!(is_not_supported(&finding.message));
        }
        assert_eq!(
            traffic_finding(SubscriptionTraffic::Allowed).severity,
            Severity::Ok
        );
    }

    #[test]
    fn auth_status_tells_a_subscription_from_anything_else() {
        let sub = r#"{"loggedIn":true,"authMethod":"claude.ai","subscriptionType":"team"}"#;
        assert_eq!(
            read_auth_status(sub),
            ClaudeLogin::Subscription {
                plan: Some("team".to_string())
            }
        );
        // A claude.ai account without a paid plan has nothing to pay with.
        let free = r#"{"loggedIn":true,"authMethod":"claude.ai","subscriptionType":"free"}"#;
        let login = read_auth_status(free);
        assert!(matches!(login, ClaudeLogin::Other { .. }), "{login:?}");
        let finding = login_finding(&login);
        assert_eq!(finding.severity, Severity::Error);
        assert!(
            finding.message.contains("without a paid plan"),
            "{finding:?}"
        );
        // A key helper outranks the stored login, which visdom keeps.
        let helper = r#"{"loggedIn":true,"authMethod":"api_key","storedClaudeAiLogin":true}"#;
        assert!(matches!(
            read_auth_status(helper),
            ClaudeLogin::Subscription { .. }
        ));
        let console = r#"{"loggedIn":true,"authMethod":"api_key","apiKeySource":"/login"}"#;
        assert_eq!(
            read_auth_status(console),
            ClaudeLogin::Other {
                method: "api_key".to_string()
            }
        );
        let out = r#"{"loggedIn":false,"authMethod":"none"}"#;
        assert_eq!(read_auth_status(out), ClaudeLogin::SignedOut);
        assert!(matches!(
            read_auth_status("Usage: claude [options]"),
            ClaudeLogin::Unknown(_)
        ));
    }

    #[test]
    fn a_login_that_is_not_a_subscription_names_the_fix() {
        let finding = login_finding(&ClaudeLogin::SignedOut);
        assert_eq!(finding.severity, Severity::Error);
        assert!(finding.message.contains("/login"), "{finding:?}");
        let finding = login_finding(&ClaudeLogin::Other {
            method: "api_key".to_string(),
        });
        assert_eq!(finding.severity, Severity::Error);
        assert!(finding.message.contains("Anthropic Console"), "{finding:?}");
        assert!(finding.message.contains("/login"), "{finding:?}");
        // Nothing shows a subscription: not accepted either.
        let finding = login_finding(&ClaudeLogin::Unknown("x".to_string()));
        assert_eq!(finding.severity, Severity::Error);
        assert!(finding.message.contains("claude update"), "{finding:?}");
    }

    fn project(models: Option<&[(&str, bool)]>) -> OpenProject {
        OpenProject {
            id: "p-1".to_string(),
            slug: "alpha".to_string(),
            name: "Alpha".to_string(),
            models: models.map(|models| {
                models
                    .iter()
                    .map(|(alias, subscription)| ProjectModel {
                        alias: alias.to_string(),
                        subscription: *subscription,
                    })
                    .collect()
            }),
        }
    }

    #[test]
    fn the_alias_must_take_subscription_calls_and_the_ones_that_do_are_named() {
        let alpha = project(Some(&[
            ("gpt-general", false),
            ("claude-sonnet", true),
            ("claude-opus", true),
        ]));
        let ours = |model: Option<&str>| {
            vec![ModelUse {
                model: model.map(String::from),
                source: None,
            }]
        };
        let ok = model_finding(
            &ours(Some("claude-sonnet")),
            None,
            &alpha,
            "claude_code_model",
        );
        assert_eq!(
            (ok.id, ok.severity),
            ("gateway.subscription_model", Severity::Ok)
        );
        let bad = model_finding(
            &ours(Some("gpt-general")),
            Some("VISDOM_GATEWAY_MODEL in the environment"),
            &alpha,
            "claude_code_model",
        );
        assert_eq!(bad.severity, Severity::Error);
        assert!(
            bad.message.contains("claude-sonnet, claude-opus"),
            "{bad:?}"
        );
        assert!(bad
            .message
            .contains("VISDOM_GATEWAY_MODEL in the environment"));
        assert!(bad.message.contains("claude_code_model"), "{bad:?}");
        let none = model_finding(&ours(None), None, &alpha, "claude_code_model");
        assert_eq!(none.severity, Severity::Error);
        assert!(none.message.contains("claude-sonnet"), "{none:?}");

        let nothing = model_finding(
            &ours(Some("gpt-general")),
            None,
            &project(Some(&[("gpt-general", false)])),
            "claude_code_model",
        );
        assert_eq!(nothing.severity, Severity::Error);
        assert!(nothing.message.contains("--billing project"), "{nothing:?}");

        // Without the list nothing can be relied on.
        let silent = model_finding(&ours(Some("x")), None, &project(None), "claude_code_model");
        assert_eq!(silent.severity, Severity::Error);
        assert!(silent.message.contains("--billing project"), "{silent:?}");

        // The small model set in the environment is checked too, and its
        // source is the one to change.
        let env = |name: &str| {
            (name == "ANTHROPIC_DEFAULT_HAIKU_MODEL")
                .then(|| ("gpt-general".to_string(), "the environment".to_string()))
        };
        let uses = models_in_use(Some("claude-sonnet"), &[], &env);
        assert_eq!(uses.len(), 2, "{uses:?}");
        let small = model_finding(&uses, None, &alpha, "claude_code_model");
        assert_eq!(small.severity, Severity::Error);
        assert!(
            small
                .message
                .contains("change ANTHROPIC_DEFAULT_HAIKU_MODEL in the environment"),
            "{small:?}"
        );
    }

    #[test]
    fn the_models_in_use_are_the_agents_options_then_the_environment_then_visdoms() {
        let none = |_: &str| None;
        assert_eq!(
            models_in_use(Some("claude-sonnet"), &[], &none),
            [ModelUse {
                model: Some("claude-sonnet".to_string()),
                source: None
            }]
        );
        let env = |name: &str| {
            (name == "ANTHROPIC_MODEL")
                .then(|| ("claude-opus".to_string(), "the environment".to_string()))
        };
        let uses = models_in_use(Some("claude-sonnet"), &[], &env);
        assert_eq!(uses[0].model.as_deref(), Some("claude-opus"));
        assert_eq!(uses[1].model.as_deref(), Some("claude-sonnet"));
        let args = ["--model=gpt-general".to_string(), "-p".to_string()];
        let uses = models_in_use(Some("claude-sonnet"), &args, &env);
        assert_eq!(uses[0].model.as_deref(), Some("gpt-general"));
        assert_eq!(
            uses[0].source.as_deref(),
            Some("--model given to Claude Code")
        );
        let prompt = ["--".to_string(), "--model".to_string(), "x".to_string()];
        assert_eq!(
            models_in_use(Some("claude-sonnet"), &prompt, &none)[0]
                .model
                .as_deref(),
            Some("claude-sonnet"),
            "after `--` it is the prompt's"
        );
    }

    #[test]
    fn a_model_in_claude_codes_settings_wins_over_the_environment() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path().join("home");
        let repo = dir.path().join("repo");
        let managed = dir.path().join("managed-settings.json");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(repo.join(".claude")).unwrap();
        std::fs::write(
            home.join(".claude/settings.json"),
            r#"{"env":{"ANTHROPIC_MODEL":"from-user","ANTHROPIC_DEFAULT_HAIKU_MODEL":"small-user"}}"#,
        )
        .unwrap();
        std::fs::write(
            repo.join(".claude/settings.json"),
            r#"{"env":{"ANTHROPIC_MODEL":"from-project"}}"#,
        )
        .unwrap();
        let process = |_: &str| Some("from-process".to_string());
        let env = ClaudeEnv::read(&managed, &home, Some(&repo), &[]);
        let (model, at) = env.get("ANTHROPIC_MODEL", &process).unwrap();
        assert_eq!(model, "from-project");
        assert!(at.contains(".claude/settings.json"), "{at}");
        assert_eq!(
            env.get("ANTHROPIC_DEFAULT_HAIKU_MODEL", &process)
                .unwrap()
                .0,
            "small-user"
        );
        assert_eq!(
            env.get("OTHER", &process).unwrap(),
            ("from-process".to_string(), "the environment".to_string())
        );
        let args = [
            "--settings".to_string(),
            r#"{"env":{"ANTHROPIC_MODEL":"first"}}"#.to_string(),
            "--settings".to_string(),
            r#"{"env":{"ANTHROPIC_MODEL":"from-flag"}}"#.to_string(),
        ];
        let env = ClaudeEnv::read(&managed, &home, Some(&repo), &args);
        let (model, at) = env.get("ANTHROPIC_MODEL", &process).unwrap();
        assert_eq!(
            (model.as_str(), at.as_str()),
            ("from-flag", "`env` in --settings given to Claude Code")
        );
        // As the launch merges them, a later `env` replaces an earlier one as
        // a whole: the project's model shows through.
        let replaced = [
            "--settings".to_string(),
            r#"{"env":{"ANTHROPIC_MODEL":"first"}}"#.to_string(),
            "--settings".to_string(),
            r#"{"env":{"OTHER":"1"}}"#.to_string(),
        ];
        let env = ClaudeEnv::read(&managed, &home, Some(&repo), &replaced);
        assert_eq!(
            env.get("ANTHROPIC_MODEL", &process).unwrap().0,
            "from-project"
        );
        // Other model settings are checked when they name one.
        let named = |name: &str| {
            (name == "CLAUDE_CODE_SUBAGENT_MODEL")
                .then(|| ("gpt-general".to_string(), "the environment".to_string()))
        };
        let uses = models_in_use(Some("claude-sonnet"), &[], &named);
        assert_eq!(uses.len(), 2, "{uses:?}");
        assert_eq!(uses[1].model.as_deref(), Some("gpt-general"));
        std::fs::write(&managed, r#"{"env":{"ANTHROPIC_MODEL":"managed"}}"#).unwrap();
        let env = ClaudeEnv::read(&managed, &home, Some(&repo), &args);
        assert_eq!(env.get("ANTHROPIC_MODEL", &process).unwrap().0, "managed");
    }

    #[test]
    fn only_eligible_aliases_are_offered() {
        let available = ["gpt-general".to_string(), "claude-sonnet".to_string()];
        let alpha = project(Some(&[("claude-sonnet", true), ("gpt-general", false)]));
        assert_eq!(eligible_of(&alpha, &available), ["claude-sonnet"]);
        assert!(eligible_of(&project(None), &available).is_empty());
    }

    #[test]
    fn the_key_is_reported_with_its_expiry_date() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-12-27T10:00:00Z")
            .unwrap()
            .timestamp();
        let finding = key_finding(&KeyState::Accepted { expires_at: at });
        assert_eq!(
            (finding.id, finding.severity),
            ("gateway.subscription_key", Severity::Ok)
        );
        assert!(finding.message.contains("2026-12-27"), "{finding:?}");
        assert_eq!(key_finding(&KeyState::NoneKept).severity, Severity::Info);
        assert_eq!(key_finding(&KeyState::KeptRefused).severity, Severity::Warn);
        let failed = key_finding(&KeyState::Failed(NOT_SUPPORTED.to_string()));
        assert_eq!(failed.severity, Severity::Error);
    }

    #[test]
    fn managed_settings_that_take_the_session_over_are_named() {
        let settings = |text: &str| match serde_json::from_str(text).unwrap() {
            serde_json::Value::Object(map) => map,
            _ => unreachable!(),
        };
        assert!(managed_takeovers(&settings(r#"{"forceLoginMethod":"claudeai"}"#)).is_empty());
        assert!(managed_takeovers(&settings(r#"{"apiKeyHelper":""}"#)).is_empty());
        let found = managed_takeovers(&settings(
            r#"{"apiKeyHelper":"get-key","forceLoginMethod":"console",
                "env":{"ANTHROPIC_API_KEY":"sk","ANTHROPIC_BASE_URL":"https://x","OTHER":"1"}}"#,
        ));
        assert_eq!(found.len(), 4, "{found:?}");
        assert!(found[1].contains("forceLoginMethod = console"), "{found:?}");
        // An empty key is none, but empty headers still replace visdom's.
        let found = managed_takeovers(&settings(
            r#"{"env":{"ANTHROPIC_API_KEY":"","ANTHROPIC_CUSTOM_HEADERS":""}}"#,
        ));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("ANTHROPIC_CUSTOM_HEADERS"), "{found:?}");
        // A provider switch that is on; one that is off is fine.
        let found = managed_takeovers(&settings(
            r#"{"env":{"CLAUDE_CODE_USE_BEDROCK":"1","CLAUDE_CODE_USE_VERTEX":"0","CLAUDE_CODE_USE_FOUNDRY":false}}"#,
        ));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("CLAUDE_CODE_USE_BEDROCK"), "{found:?}");
    }

    #[test]
    fn settings_are_an_error_when_managed_and_a_warning_when_the_developers() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path().join("home");
        let repo = dir.path().join("repo");
        let managed = dir.path().join("managed-settings.json");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(repo.join(".claude")).unwrap();
        let clean = settings_finding(&managed, &home, Some(&repo));
        assert_eq!(
            (clean.id, clean.severity),
            ("gateway.claude_settings", Severity::Ok)
        );
        std::fs::write(
            repo.join(".claude/settings.local.json"),
            r#"{"apiKeyHelper":"echo sk"}"#,
        )
        .unwrap();
        let own = settings_finding(&managed, &home, Some(&repo));
        assert_eq!(own.severity, Severity::Warn);
        assert!(own.message.contains("settings.local.json"), "{own:?}");
        std::fs::write(&managed, r#"{"forceLoginMethod":"console"}"#).unwrap();
        let theirs = settings_finding(&managed, &home, Some(&repo));
        assert_eq!(theirs.severity, Severity::Error);
        assert!(theirs.message.contains("--billing project"), "{theirs:?}");
    }
}
