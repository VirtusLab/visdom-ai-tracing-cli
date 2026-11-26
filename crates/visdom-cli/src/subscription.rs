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
    let mut cmd = Command::new(program);
    cmd.args(["auth", "status"])
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
                    other => format!("`{other}`"),
                }
            ),
        ),
        ClaudeLogin::Unknown(why) => Finding::new(
            COMPONENT,
            ID,
            Severity::Unknown,
            format!(
                "whether Claude Code is signed in with a claude.ai subscription could not be \
                 checked: {why}"
            ),
        ),
    }
}

/// `gateway.subscription_model`: whether `model` is an alias the gateway
/// takes a subscription call on, in `project`. `source` says where the alias
/// was set; `config_key` is the setting that names it.
pub fn model_finding(
    model: Option<&str>,
    source: Option<&str>,
    project: &OpenProject,
    config_key: &str,
) -> Finding {
    const ID: &str = "gateway.subscription_model";
    let Some(eligible) = project.subscription_aliases() else {
        return Finding::new(
            COMPONENT,
            ID,
            Severity::Warn,
            format!(
                "the AI Gateway does not say which model aliases of the project `{}` take calls \
                 paid by a Claude subscription (a version from before it could): one served by \
                 any other provider than Anthropic is refused",
                project.slug
            ),
        );
    };
    let set =
        format!("set {config_key} under [gateway] in your visdom config, or VISDOM_GATEWAY_MODEL");
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
/// `project`; all of them when it does not say.
pub fn eligible_of(project: &OpenProject, available: &[String]) -> Vec<String> {
    match project.subscription_aliases() {
        Some(eligible) => available
            .iter()
            .filter(|alias| eligible.contains(alias))
            .cloned()
            .collect(),
        None => available.to_vec(),
    }
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
    for (name, what) in [
        (
            "ANTHROPIC_API_KEY",
            "its key is sent instead of your subscription",
        ),
        (
            "ANTHROPIC_AUTH_TOKEN",
            "its token is sent instead of your subscription",
        ),
        ("ANTHROPIC_BASE_URL", "the session is sent past the gateway"),
        (
            "ANTHROPIC_CUSTOM_HEADERS",
            "they replace the headers that carry your subscription key",
        ),
    ] {
        if is_set(env.and_then(|env| env.get(name))) {
            found.push(format!("`{name}` in `env` ({what})"));
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
        let finding = login_finding(&ClaudeLogin::Unknown("x".to_string()));
        assert_eq!(finding.severity, Severity::Unknown);
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
        let ok = model_finding(Some("claude-sonnet"), None, &alpha, "claude_code_model");
        assert_eq!(
            (ok.id, ok.severity),
            ("gateway.subscription_model", Severity::Ok)
        );
        let bad = model_finding(
            Some("gpt-general"),
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
        let none = model_finding(None, None, &alpha, "claude_code_model");
        assert_eq!(none.severity, Severity::Error);
        assert!(none.message.contains("claude-sonnet"), "{none:?}");

        let nothing = model_finding(
            Some("gpt-general"),
            None,
            &project(Some(&[("gpt-general", false)])),
            "claude_code_model",
        );
        assert_eq!(nothing.severity, Severity::Error);
        assert!(nothing.message.contains("--billing project"), "{nothing:?}");

        let silent = model_finding(Some("x"), None, &project(None), "claude_code_model");
        assert_eq!(silent.severity, Severity::Warn);
    }

    #[test]
    fn only_eligible_aliases_are_offered() {
        let available = ["gpt-general".to_string(), "claude-sonnet".to_string()];
        let alpha = project(Some(&[("claude-sonnet", true), ("gpt-general", false)]));
        assert_eq!(eligible_of(&alpha, &available), ["claude-sonnet"]);
        assert_eq!(eligible_of(&project(None), &available), available);
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
