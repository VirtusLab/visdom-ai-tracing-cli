//! Detects how the agent session behind a hook invocation is billed
//! (gateway / API key / subscription), so the server can tell real spend from
//! subscription usage priced at API list rates.
//!
//! Detection is static: it reads the hook's environment (hooks inherit the
//! agent's environment) and the agent's own auth/config files, never runs the
//! agent or touches the network, and answers `Unknown` rather than guess. All
//! inputs come through [`Probe`] so tests drive it with fixture files and a
//! fake environment instead of mutating the process environment.

use std::path::{Path, PathBuf};

use tracevault_protocol::streaming::BillingMode;

use crate::agent::Agent;

/// Everything detection may look at.
pub struct Probe<'a> {
    /// Environment lookup (the hook process's environment in production).
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// The user's home directory.
    pub home: Option<PathBuf>,
    /// Directory the agent was started in (project-level settings live here).
    pub project_dir: PathBuf,
    /// Administrator-managed Claude Code settings files, highest precedence.
    pub managed_claude_settings: Vec<PathBuf>,
    /// The session transcript (Codex: the rollout), if the hook named one.
    pub transcript_path: Option<PathBuf>,
    /// Codex's machine-wide config layers outside `$CODEX_HOME`, in Codex's
    /// precedence: `system` sits below the user config, `managed` above it.
    pub codex_system_config: Vec<PathBuf>,
    pub codex_managed_config: Vec<PathBuf>,
}

impl Probe<'_> {
    /// The env var's value, treating an empty value as unset.
    fn var(&self, name: &str) -> Option<String> {
        (self.env)(name).filter(|v| !v.trim().is_empty())
    }

    /// Whether a boolean-ish env var is switched on (`1`, `true`, ...).
    fn flag(&self, name: &str) -> bool {
        self.var(name).is_some_and(|v| {
            let v = v.trim();
            v != "0" && !v.eq_ignore_ascii_case("false") && !v.eq_ignore_ascii_case("no")
        })
    }
}

/// Billing mode of the current hook invocation, from the real environment.
pub fn detect_current(agent: Agent, hook_cwd: &Path, transcript_path: &str) -> BillingMode {
    let env = |name: &str| std::env::var(name).ok();
    let project_dir = std::env::var_os("CLAUDE_PROJECT_DIR")
        .filter(|_| agent == Agent::ClaudeCode)
        .map(PathBuf::from)
        .unwrap_or_else(|| hook_cwd.to_path_buf());
    let probe = Probe {
        env: &env,
        home: dirs::home_dir(),
        project_dir,
        managed_claude_settings: managed_claude_settings_paths(),
        transcript_path: (!transcript_path.is_empty()).then(|| PathBuf::from(transcript_path)),
        codex_system_config: codex_system_config_paths(),
        codex_managed_config: codex_managed_config_paths(),
    };
    detect(agent, &probe)
}

pub fn detect(agent: Agent, probe: &Probe) -> BillingMode {
    match agent {
        Agent::ClaudeCode => claude_code(probe),
        Agent::Codex => codex(probe),
        // Neither exposes which provider/credential a session runs on in a
        // place the hook can read reliably.
        Agent::Gsd | Agent::OpenCode => BillingMode::Unknown,
    }
}

// ── Claude Code ─────────────────────────────────────────────────────────────
//
// Follows Claude Code's documented credential precedence
// (https://code.claude.com/docs/en/authentication.md#authentication-precedence):
// cloud provider > ANTHROPIC_AUTH_TOKEN > ANTHROPIC_API_KEY > apiKeyHelper >
// CLAUDE_CODE_OAUTH_TOKEN > subscription login. A non-Anthropic
// ANTHROPIC_BASE_URL alone does not make a gateway session: per the docs a
// claude.ai login keeps its subscription billing behind a proxy, so the base
// URL only turns a gateway credential (token / key / helper) into `Gateway`.

fn claude_code(p: &Probe) -> BillingMode {
    for (flag, base_url) in [
        ("CLAUDE_CODE_USE_BEDROCK", "ANTHROPIC_BEDROCK_BASE_URL"),
        ("CLAUDE_CODE_USE_VERTEX", "ANTHROPIC_VERTEX_BASE_URL"),
        ("CLAUDE_CODE_USE_FOUNDRY", "ANTHROPIC_FOUNDRY_BASE_URL"),
    ] {
        if p.flag(flag) {
            let routed = p.var(base_url).is_some_and(|u| !is_direct_provider_api(&u));
            return if routed {
                BillingMode::Gateway
            } else {
                // Pay-per-token on the cloud account.
                BillingMode::ApiKey
            };
        }
    }

    let config_dir = claude_config_dir(p);
    let gateway_or_key = || {
        let routed = p
            .var("ANTHROPIC_BASE_URL")
            .is_some_and(|u| !host_is(&u, "api.anthropic.com"));
        if routed {
            BillingMode::Gateway
        } else {
            BillingMode::ApiKey
        }
    };

    if p.var("ANTHROPIC_AUTH_TOKEN").is_some() {
        return gateway_or_key();
    }
    if let Some(key) = p.var("ANTHROPIC_API_KEY") {
        if claude_api_key_declined(p, config_dir.as_deref(), &key) {
            // Interactive sessions skip a declined key and use the login, but
            // `-p` always uses the key, and the hook cannot tell the two apart.
            return BillingMode::Unknown;
        }
        return gateway_or_key();
    }
    if claude_api_key_helper_configured(p, config_dir.as_deref()) {
        return gateway_or_key();
    }
    if p.var("CLAUDE_CODE_OAUTH_TOKEN").is_some() {
        // Long-lived token from `claude setup-token`: a subscription login.
        return BillingMode::Subscription;
    }
    if claude_subscription_login(p, config_dir.as_deref()) {
        return BillingMode::Subscription;
    }
    BillingMode::Unknown
}

/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
fn claude_config_dir(p: &Probe) -> Option<PathBuf> {
    p.var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| p.home.as_ref().map(|h| h.join(".claude")))
}

/// Claude Code's global state file: `$CLAUDE_CONFIG_DIR/.claude.json` when the
/// config dir is overridden, else `~/.claude.json`.
fn claude_global_state(p: &Probe, config_dir: Option<&Path>) -> Option<serde_json::Value> {
    let path = match p.var("CLAUDE_CONFIG_DIR") {
        Some(_) => config_dir?.join(".claude.json"),
        None => p.home.as_ref()?.join(".claude.json"),
    };
    read_json(&path)
}

/// In interactive mode Claude Code asks once whether to use ANTHROPIC_API_KEY
/// and falls back to the subscription when the user declines. The answer is
/// kept in `customApiKeyResponses.rejected` as the key's last 20 characters.
/// That storage is observed, not documented, so it is only used to downgrade
/// an explicitly declined key to "can't tell"; anything else counts the key as
/// in use (which is also what non-interactive `-p` mode always does).
fn claude_api_key_declined(p: &Probe, config_dir: Option<&Path>, key: &str) -> bool {
    let Some(state) = claude_global_state(p, config_dir) else {
        return false;
    };
    let suffix: String = {
        let chars: Vec<char> = key.trim().chars().collect();
        chars[chars.len().saturating_sub(20)..].iter().collect()
    };
    let listed = |list: &str| {
        state
            .pointer(&format!("/customApiKeyResponses/{list}"))
            .and_then(|v| v.as_array())
            .is_some_and(|a| a.iter().any(|e| e.as_str() == Some(suffix.as_str())))
    };
    listed("rejected") && !listed("approved")
}

/// The effective `apiKeyHelper` is non-empty. Settings resolve a scalar from
/// the highest-precedence file that sets it: managed > local > project > user,
/// so an empty value in a higher scope switches off a helper set lower down.
fn claude_api_key_helper_configured(p: &Probe, config_dir: Option<&Path>) -> bool {
    let project = p.project_dir.join(".claude");
    p.managed_claude_settings
        .iter()
        .cloned()
        .chain([
            project.join("settings.local.json"),
            project.join("settings.json"),
        ])
        .chain(config_dir.map(|d| d.join("settings.json")))
        .filter_map(|path| read_json(&path))
        .find_map(|s| s.get("apiKeyHelper").cloned())
        .is_some_and(|v| v.as_str().is_some_and(|v| !v.trim().is_empty()))
}

/// A `/login` subscription session. On Linux/Windows the OAuth credentials sit
/// in `<config dir>/.credentials.json` under `claudeAiOauth` (documented). On
/// macOS they live in the Keychain, so fall back to the account profile Claude
/// Code caches in its global state (`oauthAccount.billingType`, observed as
/// `stripe_subscription`).
fn claude_subscription_login(p: &Probe, config_dir: Option<&Path>) -> bool {
    let has_oauth_credentials = config_dir
        .and_then(|d| read_json(&d.join(".credentials.json")))
        .is_some_and(|c| c.get("claudeAiOauth").is_some_and(|o| o.is_object()));
    if has_oauth_credentials {
        return true;
    }
    claude_global_state(p, config_dir).is_some_and(|s| {
        s.pointer("/oauthAccount/billingType")
            .and_then(|v| v.as_str())
            .is_some_and(|t| t.contains("subscription"))
    })
}

#[cfg(target_os = "macos")]
fn managed_claude_settings_paths() -> Vec<PathBuf> {
    vec![PathBuf::from(
        "/Library/Application Support/ClaudeCode/managed-settings.json",
    )]
}

#[cfg(target_os = "windows")]
fn managed_claude_settings_paths() -> Vec<PathBuf> {
    vec![PathBuf::from(
        r"C:\Program Files\ClaudeCode\managed-settings.json",
    )]
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn managed_claude_settings_paths() -> Vec<PathBuf> {
    vec![PathBuf::from("/etc/claude-code/managed-settings.json")]
}

// ── Codex ───────────────────────────────────────────────────────────────────

//
// Checked against codex-rs at rust-v0.160.0:
// - Routing: the provider a session uses is recorded as `model_provider` on
//   the rollout's first (`session_meta`) line. The built-in `openai` provider
//   can only be redirected by the top-level `openai_base_url` config key
//   (`OPENAI_BASE_URL` stopped doing that around 0.120, so it is ignored here),
//   read from the config layers a hook can see (`codex_config_layers`).
// - Auth: `$CODEX_HOME/auth.json` (default `~/.codex`) carries `auth_mode`.
//   It can be absent while logged in (keyring / ephemeral credential store).
// - `CODEX_ACCESS_TOKEN` overrides auth.json in every mode; `CODEX_API_KEY`
//   overrides it under `codex exec` only.

/// Local model servers Codex ships as built-in providers (`--oss`): neither a
/// gateway nor billed.
const CODEX_LOCAL_PROVIDERS: &[&str] = &["ollama", "lmstudio"];

fn codex(p: &Probe) -> BillingMode {
    let codex_home = p
        .var("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| p.home.as_ref().map(|h| h.join(".codex")));
    let auth_json = codex_home
        .as_ref()
        .and_then(|h| read_json(&h.join("auth.json")));
    let auth = auth_json.as_ref().map(codex_auth_mode);
    let bedrock_auth = auth_json.as_ref().is_some_and(codex_auth_is_bedrock);
    let access_token = p.var("CODEX_ACCESS_TOKEN").is_some();

    let layers = codex_config_layers(p, codex_home.as_deref());
    let config_str = |path: &[&str]| layers.iter().find_map(|l| toml_str(l, path));

    let provider = p
        .transcript_path
        .as_deref()
        .and_then(codex_rollout_provider)
        .or_else(|| config_str(&["model_provider"]))
        .unwrap_or_else(|| "openai".to_string());
    if provider != "openai" {
        if CODEX_LOCAL_PROVIDERS.contains(&provider.as_str()) {
            return BillingMode::Unknown;
        }
        // A configured provider bills through whatever its base URL points at
        // (it carries its own credentials, so stored auth doesn't matter).
        return match config_str(&["model_providers", &provider, "base_url"]) {
            Some(url) if is_direct_provider_api(&url) => BillingMode::ApiKey,
            Some(_) => BillingMode::Gateway,
            // A built-in provider not in the config (Bedrock): stored Bedrock
            // credentials bill per token on the AWS account, unless the
            // access-token override replaces them.
            None if bedrock_auth && !access_token => BillingMode::ApiKey,
            None => BillingMode::Unknown,
        };
    }
    let redirected = config_str(&["openai_base_url"])
        .is_some_and(|u| !host_is(&u, "api.openai.com") && !host_is(&u, "chatgpt.com"));
    if redirected {
        return BillingMode::Gateway;
    }

    if access_token {
        // Overrides auth.json, and is a personal access token or an
        // agent-identity JWT: can't tell which.
        return BillingMode::Unknown;
    }
    match auth {
        // Under `codex exec` CODEX_API_KEY would win over the login.
        Some(BillingMode::Subscription) if p.var("CODEX_API_KEY").is_some() => BillingMode::Unknown,
        Some(mode) => mode,
        None => BillingMode::Unknown,
    }
}

/// The Codex config layers a hook can see, highest precedence first (Codex's
/// `config/src/loader`): legacy `managed_config.toml`, the user config's
/// legacy `[profiles.<profile>]` table, the user config, the system config.
/// Project `.codex/config.toml` files are skipped: Codex refuses routing keys
/// there (`PROJECT_LOCAL_CONFIG_DENYLIST`). Not visible to a hook: cloud and
/// MDM-managed layers, `--profile` files and `-c` overrides.
fn codex_config_layers(p: &Probe, codex_home: Option<&Path>) -> Vec<toml::Table> {
    let read =
        |path: &Path| -> Option<toml::Table> { std::fs::read_to_string(path).ok()?.parse().ok() };
    let user = codex_home.and_then(|h| read(&h.join("config.toml")));
    let profile = user.as_ref().and_then(|u| {
        let name = u.get("profile")?.as_str()?;
        u.get("profiles")?.get(name)?.as_table().cloned()
    });
    p.codex_managed_config
        .iter()
        .filter_map(|path| read(path))
        .chain(profile)
        .chain(user)
        .chain(p.codex_system_config.iter().filter_map(|path| read(path)))
        .collect()
}

/// Non-empty string at a dotted `path` in a TOML table.
fn toml_str(table: &toml::Table, path: &[&str]) -> Option<String> {
    let (last, parents) = path.split_last()?;
    let mut table = table;
    for key in parents {
        table = table.get(*key)?.as_table()?;
    }
    table
        .get(*last)?
        .as_str()
        .filter(|v| !v.trim().is_empty())
        .map(str::to_string)
}

#[cfg(windows)]
fn codex_system_config_paths() -> Vec<PathBuf> {
    std::env::var_os("ProgramData")
        .map(|d| PathBuf::from(d).join(r"OpenAI\Codex\config.toml"))
        .into_iter()
        .collect()
}

#[cfg(not(windows))]
fn codex_system_config_paths() -> Vec<PathBuf> {
    vec![PathBuf::from("/etc/codex/config.toml")]
}

#[cfg(windows)]
fn codex_managed_config_paths() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(not(windows))]
fn codex_managed_config_paths() -> Vec<PathBuf> {
    vec![PathBuf::from("/etc/codex/managed_config.toml")]
}

/// Billing implied by an `auth.json`. Without an explicit `auth_mode`, infer
/// it the way Codex does (`AuthDotJson::resolved_mode`).
fn codex_auth_mode(auth: &serde_json::Value) -> BillingMode {
    let present = |key: &str| auth.get(key).is_some_and(|v| !v.is_null());
    match auth.get("auth_mode").and_then(|v| v.as_str()) {
        Some("apikey" | "bedrockApiKey" | "bedrockAccessKeys") => BillingMode::ApiKey,
        Some("chatgpt" | "chatgptAuthTokens" | "personalAccessToken") => BillingMode::Subscription,
        // `headers`, `agentIdentity`, or a mode newer than this code.
        Some(_) => BillingMode::Unknown,
        None if present("personal_access_token") => BillingMode::Subscription,
        None if codex_auth_is_bedrock(auth) => BillingMode::ApiKey,
        None if present("OPENAI_API_KEY") => BillingMode::ApiKey,
        None if present("tokens") => BillingMode::Subscription,
        None => BillingMode::Unknown,
    }
}

/// Bedrock auth: an explicit `bedrock*` mode, or (without a mode) Bedrock
/// credential fields, which Codex itself resolves to Bedrock.
fn codex_auth_is_bedrock(auth: &serde_json::Value) -> bool {
    match auth.get("auth_mode").and_then(|v| v.as_str()) {
        Some(mode) => mode.starts_with("bedrock"),
        None => ["bedrock_api_key", "bedrock_access_keys"]
            .iter()
            .any(|k| auth.get(k).is_some_and(|v| !v.is_null())),
    }
}

/// Hosts of model vendors' own pay-per-token APIs. A provider endpoint
/// pointing at one of these is billed directly, not through a gateway; any
/// other endpoint (LiteLLM, an internal proxy, localhost) counts as a gateway.
fn is_direct_provider_api(url: &str) -> bool {
    const EXACT: &[&str] = &[
        "api.openai.com",
        "api.anthropic.com",
        "api.mistral.ai",
        "generativelanguage.googleapis.com",
        "api.deepseek.com",
        "api.x.ai",
        "api.groq.com",
        "aiplatform.googleapis.com",
    ];
    const SUFFIXES: &[&str] = &[
        ".openai.azure.com",
        ".services.ai.azure.com",
        ".cognitiveservices.azure.com",
        "-aiplatform.googleapis.com",
    ];
    url_host(url).is_some_and(|h| {
        EXACT.contains(&h.as_str())
            || SUFFIXES.iter().any(|s| h.ends_with(s))
            // Bedrock's own endpoints, including FIPS and VPC (PrivateLink)
            // ones, but not other AWS-hosted hosts such as API Gateway, which
            // can front a customer's gateway.
            || (h.ends_with(".amazonaws.com")
                && h.split('.').any(|label| label.starts_with("bedrock")))
    })
}

/// `model_provider` from the rollout's leading `session_meta` line.
fn codex_rollout_provider(rollout: &Path) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(rollout).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(file).read_line(&mut first).ok()?;
    let meta: serde_json::Value = serde_json::from_str(&first).ok()?;
    if meta.get("type").and_then(|v| v.as_str()) != Some("session_meta") {
        return None;
    }
    meta.pointer("/payload/model_provider")
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .map(str::to_string)
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Host part of a URL, lowercased; `None` if it does not parse as one.
fn url_host(url: &str) -> Option<String> {
    let rest = url.trim().split_once("://").map_or(url.trim(), |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(v6) = host_port.strip_prefix('[') {
        v6.split(']').next()?
    } else {
        host_port.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

fn host_is(url: &str, expected: &str) -> bool {
    url_host(url).is_some_and(|h| h == expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use tempfile::TempDir;

    /// A fake machine: a temp home + project dir and an explicit environment.
    struct Fixture {
        home: TempDir,
        project: TempDir,
        env: HashMap<String, String>,
        managed: Vec<PathBuf>,
        codex_system: Vec<PathBuf>,
        codex_managed: Vec<PathBuf>,
        transcript: Option<PathBuf>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                home: TempDir::new().unwrap(),
                project: TempDir::new().unwrap(),
                env: HashMap::new(),
                managed: Vec::new(),
                codex_system: Vec::new(),
                codex_managed: Vec::new(),
                transcript: None,
            }
        }

        fn env(mut self, key: &str, val: &str) -> Self {
            self.env.insert(key.into(), val.into());
            self
        }

        fn home_file(self, rel: &str, contents: &str) -> Self {
            write(&self.home.path().join(rel), contents);
            self
        }

        fn project_file(self, rel: &str, contents: &str) -> Self {
            write(&self.project.path().join(rel), contents);
            self
        }

        fn managed_settings(mut self, contents: &str) -> Self {
            let path = self.home.path().join("managed/managed-settings.json");
            write(&path, contents);
            self.managed.push(path);
            self
        }

        fn system_config(mut self, contents: &str) -> Self {
            let path = self.home.path().join("etc/codex/config.toml");
            write(&path, contents);
            self.codex_system.push(path);
            self
        }

        fn managed_codex_config(mut self, contents: &str) -> Self {
            let path = self.home.path().join("etc/codex/managed_config.toml");
            write(&path, contents);
            self.codex_managed.push(path);
            self
        }

        fn rollout(mut self, first_line: serde_json::Value) -> Self {
            let path = self.home.path().join("rollout.jsonl");
            write(
                &path,
                &format!("{first_line}\n{{\"type\":\"turn_context\"}}\n"),
            );
            self.transcript = Some(path);
            self
        }

        fn detect(&self, agent: Agent) -> BillingMode {
            let env = |k: &str| self.env.get(k).cloned();
            detect(
                agent,
                &Probe {
                    env: &env,
                    home: Some(self.home.path().to_path_buf()),
                    project_dir: self.project.path().to_path_buf(),
                    managed_claude_settings: self.managed.clone(),
                    transcript_path: self.transcript.clone(),
                    codex_system_config: self.codex_system.clone(),
                    codex_managed_config: self.codex_managed.clone(),
                },
            )
        }
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    const CLAUDE_OAUTH_CREDENTIALS: &str =
        r#"{"claudeAiOauth":{"accessToken":"x","refreshToken":"y","subscriptionType":"max"}}"#;

    // ── Claude Code ─────────────────────────────────────────────────────────

    #[test]
    fn claude_nothing_configured_is_unknown() {
        assert_eq!(
            Fixture::new().detect(Agent::ClaudeCode),
            BillingMode::Unknown
        );
    }

    #[test]
    fn claude_login_credentials_are_a_subscription() {
        let f = Fixture::new().home_file(".claude/.credentials.json", CLAUDE_OAUTH_CREDENTIALS);
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::Subscription);
    }

    #[test]
    fn claude_keychain_login_falls_back_to_the_cached_account_profile() {
        // macOS keeps the OAuth tokens in the Keychain: no .credentials.json.
        let f = Fixture::new().home_file(
            ".claude.json",
            r#"{"oauthAccount":{"billingType":"stripe_subscription"}}"#,
        );
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::Subscription);
    }

    #[test]
    fn claude_account_profile_without_subscription_billing_is_unknown() {
        let f = Fixture::new().home_file(
            ".claude.json",
            r#"{"oauthAccount":{"billingType":"other"}}"#,
        );
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::Unknown);
    }

    #[test]
    fn claude_config_dir_override_is_honoured() {
        let f = Fixture::new();
        let dir = f.home.path().join("alt-claude");
        write(&dir.join(".credentials.json"), CLAUDE_OAUTH_CREDENTIALS);
        let f = f.env("CLAUDE_CONFIG_DIR", dir.to_str().unwrap());
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::Subscription);

        // ...and the default location is no longer consulted.
        let f = Fixture::new()
            .home_file(".claude/.credentials.json", CLAUDE_OAUTH_CREDENTIALS)
            .env("CLAUDE_CONFIG_DIR", "/nonexistent/claude-config");
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::Unknown);
    }

    #[test]
    fn claude_setup_token_is_a_subscription() {
        let f = Fixture::new().env("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat01-x");
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::Subscription);
    }

    #[test]
    fn claude_api_key_beats_a_subscription_login() {
        let f = Fixture::new()
            .home_file(".claude/.credentials.json", CLAUDE_OAUTH_CREDENTIALS)
            .env(
                "ANTHROPIC_API_KEY",
                "sk-ant-api03-abcdefghijklmnopqrstuvwxyz",
            );
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::ApiKey);
    }

    #[test]
    fn claude_empty_api_key_counts_as_unset() {
        let f = Fixture::new()
            .home_file(".claude/.credentials.json", CLAUDE_OAUTH_CREDENTIALS)
            .env("ANTHROPIC_API_KEY", "");
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::Subscription);
    }

    #[test]
    fn claude_declined_api_key_is_unknown() {
        // Interactive sessions would fall back to the login, `-p` would use
        // the key; the hook can't tell which, so it must not claim either.
        let key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz";
        let last20 = &key[key.len() - 20..];
        let f = Fixture::new()
            .home_file(".claude/.credentials.json", CLAUDE_OAUTH_CREDENTIALS)
            .home_file(
                ".claude.json",
                &json!({"customApiKeyResponses": {"approved": [], "rejected": [last20]}})
                    .to_string(),
            )
            .env("ANTHROPIC_API_KEY", key);
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::Unknown);
    }

    #[test]
    fn claude_approved_api_key_is_used() {
        let key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz";
        let last20 = &key[key.len() - 20..];
        let f = Fixture::new()
            .home_file(".claude/.credentials.json", CLAUDE_OAUTH_CREDENTIALS)
            .home_file(
                ".claude.json",
                &json!({"customApiKeyResponses": {"approved": [last20], "rejected": []}})
                    .to_string(),
            )
            .env("ANTHROPIC_API_KEY", key);
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::ApiKey);
    }

    #[test]
    fn claude_api_key_through_a_gateway_base_url_is_gateway() {
        let f = Fixture::new()
            .env("ANTHROPIC_API_KEY", "sk-litellm-virtual-key")
            .env("ANTHROPIC_BASE_URL", "https://litellm.internal.example.com");
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::Gateway);
    }

    #[test]
    fn claude_auth_token_through_a_gateway_is_gateway() {
        let f = Fixture::new()
            .env("ANTHROPIC_AUTH_TOKEN", "sk-litellm")
            .env("ANTHROPIC_BASE_URL", "http://localhost:4000");
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::Gateway);
    }

    #[test]
    fn claude_explicit_anthropic_base_url_is_not_a_gateway() {
        let f = Fixture::new()
            .env("ANTHROPIC_API_KEY", "sk-ant-api03-x")
            .env("ANTHROPIC_BASE_URL", "https://API.anthropic.com/");
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::ApiKey);
    }

    #[test]
    fn claude_base_url_alone_keeps_subscription_billing() {
        // Documented: a claude.ai login keeps its subscription behind a proxy
        // unless a gateway credential is also set.
        let f = Fixture::new()
            .home_file(".claude/.credentials.json", CLAUDE_OAUTH_CREDENTIALS)
            .env("ANTHROPIC_BASE_URL", "https://proxy.example.com");
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::Subscription);
    }

    #[test]
    fn claude_api_key_helper_in_any_settings_scope_is_detected() {
        let helper = r#"{"apiKeyHelper":"~/bin/get-key.sh"}"#;
        for f in [
            Fixture::new().home_file(".claude/settings.json", helper),
            Fixture::new().project_file(".claude/settings.json", helper),
            Fixture::new().project_file(".claude/settings.local.json", helper),
            Fixture::new().managed_settings(helper),
        ] {
            let f = f.home_file(".claude/.credentials.json", CLAUDE_OAUTH_CREDENTIALS);
            assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::ApiKey);
        }
        let f = Fixture::new()
            .project_file(".claude/settings.json", helper)
            .env("ANTHROPIC_BASE_URL", "https://litellm.example.com");
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::Gateway);
    }

    #[test]
    fn claude_settings_without_a_helper_do_not_count() {
        let f = Fixture::new()
            .project_file(".claude/settings.json", r#"{"apiKeyHelper":"","hooks":{}}"#)
            .project_file(".claude/settings.local.json", "not json");
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::Unknown);
    }

    #[test]
    fn claude_cloud_providers_bill_per_token() {
        for flag in [
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ] {
            let f = Fixture::new()
                .home_file(".claude/.credentials.json", CLAUDE_OAUTH_CREDENTIALS)
                .env(flag, "1");
            assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::ApiKey, "{flag}");
        }
    }

    #[test]
    fn claude_cloud_provider_flag_set_to_false_is_off() {
        let f = Fixture::new()
            .home_file(".claude/.credentials.json", CLAUDE_OAUTH_CREDENTIALS)
            .env("CLAUDE_CODE_USE_BEDROCK", "0")
            .env("CLAUDE_CODE_USE_VERTEX", "false");
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::Subscription);
    }

    #[test]
    fn claude_cloud_provider_routed_through_a_gateway_is_gateway() {
        let f = Fixture::new().env("CLAUDE_CODE_USE_BEDROCK", "1").env(
            "ANTHROPIC_BEDROCK_BASE_URL",
            "https://litellm.example.com/bedrock",
        );
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::Gateway);

        // A regional / VPC endpoint of the provider itself is not a gateway.
        let f = Fixture::new().env("CLAUDE_CODE_USE_BEDROCK", "1").env(
            "ANTHROPIC_BEDROCK_BASE_URL",
            "https://bedrock-runtime.eu-west-1.amazonaws.com",
        );
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::ApiKey);
    }

    // ── Codex ───────────────────────────────────────────────────────────────

    fn session_meta(provider: &str) -> serde_json::Value {
        json!({"type": "session_meta", "payload": {"id": "s", "model_provider": provider}})
    }

    #[test]
    fn codex_nothing_configured_is_unknown() {
        assert_eq!(Fixture::new().detect(Agent::Codex), BillingMode::Unknown);
    }

    #[test]
    fn codex_auth_modes_map_to_billing() {
        for (mode, expected) in [
            ("chatgpt", BillingMode::Subscription),
            ("chatgptAuthTokens", BillingMode::Subscription),
            ("personalAccessToken", BillingMode::Subscription),
            ("apikey", BillingMode::ApiKey),
            ("bedrockApiKey", BillingMode::ApiKey),
            ("bedrockAccessKeys", BillingMode::ApiKey),
            ("headers", BillingMode::Unknown),
            ("agentIdentity", BillingMode::Unknown),
            ("someFutureMode", BillingMode::Unknown),
        ] {
            let f = Fixture::new().home_file(
                ".codex/auth.json",
                &json!({"auth_mode": mode, "OPENAI_API_KEY": null}).to_string(),
            );
            assert_eq!(f.detect(Agent::Codex), expected, "auth_mode {mode}");
        }
    }

    #[test]
    fn codex_auth_json_without_a_mode_is_inferred_like_codex_does() {
        for (auth, expected) in [
            (json!({"OPENAI_API_KEY": "sk-x"}), BillingMode::ApiKey),
            (
                json!({"OPENAI_API_KEY": null, "tokens": {"access_token": "a"}}),
                BillingMode::Subscription,
            ),
            (
                json!({"OPENAI_API_KEY": null, "personal_access_token": "p"}),
                BillingMode::Subscription,
            ),
            (
                json!({"OPENAI_API_KEY": null, "bedrock_api_key": {"api_key": "b"}}),
                BillingMode::ApiKey,
            ),
            (json!({"OPENAI_API_KEY": null}), BillingMode::Unknown),
        ] {
            let f = Fixture::new().home_file(".codex/auth.json", &auth.to_string());
            assert_eq!(f.detect(Agent::Codex), expected, "{auth}");
        }
    }

    #[test]
    fn codex_home_override_is_honoured() {
        let f = Fixture::new();
        let dir = f.home.path().join("alt-codex");
        write(
            &dir.join("auth.json"),
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk"}"#,
        );
        let f = f
            .home_file(".codex/auth.json", r#"{"auth_mode":"chatgpt"}"#)
            .env("CODEX_HOME", dir.to_str().unwrap());
        assert_eq!(f.detect(Agent::Codex), BillingMode::ApiKey);
    }

    #[test]
    fn codex_custom_provider_in_the_rollout_is_gateway() {
        let f = Fixture::new()
            .home_file(".codex/auth.json", r#"{"auth_mode":"chatgpt"}"#)
            .home_file(
                ".codex/config.toml",
                "[model_providers.litellm]\nbase_url = \"http://localhost:4000/v1\"\n",
            )
            .rollout(session_meta("litellm"));
        assert_eq!(f.detect(Agent::Codex), BillingMode::Gateway);
    }

    #[test]
    fn codex_custom_provider_at_a_vendor_api_is_billed_directly() {
        for url in [
            "https://my-res.openai.azure.com/openai/v1",
            "https://api.mistral.ai/v1",
            "https://us-central1-aiplatform.googleapis.com/v1",
        ] {
            let f = Fixture::new()
                .home_file(
                    ".codex/config.toml",
                    &format!("[model_providers.direct]\nbase_url = \"{url}\"\n"),
                )
                .rollout(session_meta("direct"));
            assert_eq!(f.detect(Agent::Codex), BillingMode::ApiKey, "{url}");
        }
    }

    #[test]
    fn codex_custom_provider_without_a_resolvable_endpoint_is_unknown() {
        let f = Fixture::new()
            .home_file(".codex/auth.json", r#"{"auth_mode":"chatgpt"}"#)
            .rollout(session_meta("defined-elsewhere"));
        assert_eq!(f.detect(Agent::Codex), BillingMode::Unknown);
    }

    #[test]
    fn codex_openai_provider_in_the_rollout_defers_to_auth() {
        let f = Fixture::new()
            .home_file(".codex/auth.json", r#"{"auth_mode":"chatgpt"}"#)
            // The rollout is authoritative over a config that changed since.
            .home_file(".codex/config.toml", "model_provider = \"litellm\"\n")
            .rollout(session_meta("openai"));
        assert_eq!(f.detect(Agent::Codex), BillingMode::Subscription);
    }

    #[test]
    fn codex_local_oss_provider_is_unknown() {
        let f = Fixture::new()
            .home_file(".codex/auth.json", r#"{"auth_mode":"chatgpt"}"#)
            .rollout(session_meta("ollama"));
        assert_eq!(f.detect(Agent::Codex), BillingMode::Unknown);
    }

    #[test]
    fn codex_custom_provider_in_config_is_gateway_without_a_rollout() {
        let f = Fixture::new()
            .home_file(".codex/auth.json", r#"{"auth_mode":"chatgpt"}"#)
            .home_file(
                ".codex/config.toml",
                "model_provider = \"litellm\"\n[model_providers.litellm]\nbase_url = \"https://litellm.example.com\"\n",
            );
        assert_eq!(f.detect(Agent::Codex), BillingMode::Gateway);
    }

    #[test]
    fn codex_openai_base_url_redirect_is_gateway() {
        let f = Fixture::new()
            .home_file(
                ".codex/auth.json",
                r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk"}"#,
            )
            .home_file(
                ".codex/config.toml",
                "openai_base_url = \"https://gw.example.com/v1\"\n",
            )
            .rollout(session_meta("openai"));
        assert_eq!(f.detect(Agent::Codex), BillingMode::Gateway);

        // Pointing it at OpenAI itself is not a redirect.
        let f = Fixture::new()
            .home_file(
                ".codex/auth.json",
                r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk"}"#,
            )
            .home_file(
                ".codex/config.toml",
                "openai_base_url = \"https://api.openai.com/v1\"\n",
            );
        assert_eq!(f.detect(Agent::Codex), BillingMode::ApiKey);
    }

    #[test]
    fn codex_ignores_project_routing_config() {
        let f = Fixture::new()
            .home_file(".codex/auth.json", r#"{"auth_mode":"chatgpt"}"#)
            .project_file(
                ".codex/config.toml",
                "model_provider = \"litellm\"\nopenai_base_url = \"https://gw.example.com/v1\"\n",
            )
            .rollout(session_meta("openai"));
        assert_eq!(f.detect(Agent::Codex), BillingMode::Subscription);
    }

    #[test]
    fn codex_ignores_openai_base_url_env() {
        // Codex 0.160 no longer reads OPENAI_BASE_URL for the openai provider.
        let f = Fixture::new()
            .home_file(".codex/auth.json", r#"{"auth_mode":"chatgpt"}"#)
            .env("OPENAI_BASE_URL", "https://gw.example.com/v1");
        assert_eq!(f.detect(Agent::Codex), BillingMode::Subscription);
    }

    #[test]
    fn codex_bedrock_auth_is_api_key_whatever_the_provider() {
        let f = Fixture::new()
            .home_file(".codex/auth.json", r#"{"auth_mode":"bedrockApiKey"}"#)
            .rollout(session_meta("amazon-bedrock"));
        assert_eq!(f.detect(Agent::Codex), BillingMode::ApiKey);
    }

    #[test]
    fn codex_access_token_overrides_stored_bedrock_credentials() {
        for provider in ["amazon-bedrock", "openai"] {
            let f = Fixture::new()
                .home_file(".codex/auth.json", r#"{"auth_mode":"bedrockApiKey"}"#)
                .env("CODEX_ACCESS_TOKEN", "t")
                .rollout(session_meta(provider));
            assert_eq!(f.detect(Agent::Codex), BillingMode::Unknown, "{provider}");
        }
    }

    /// The production entry point reads the real process environment and the
    /// hook's transcript path (what `run_stream` stamps on every event).
    #[test]
    fn detect_current_reads_the_process_environment_and_transcript() {
        use crate::test_helpers::{lock_env_mutation_sync, EnvVarGuard};
        let _lock = lock_env_mutation_sync();
        let f = Fixture::new()
            .home_file(".codex/auth.json", r#"{"auth_mode":"chatgpt"}"#)
            .home_file(
                ".codex/config.toml",
                "[model_providers.litellm]\nbase_url = \"https://litellm.example.com\"\n",
            )
            .rollout(session_meta("litellm"));
        let mut env = EnvVarGuard::new();
        env.set("CODEX_HOME", f.home.path().join(".codex"));
        env.remove("CODEX_ACCESS_TOKEN");
        env.remove("CODEX_API_KEY");
        let transcript = f.transcript.clone().unwrap();
        let transcript = transcript.to_str().unwrap();

        assert_eq!(
            detect_current(Agent::Codex, f.project.path(), transcript),
            BillingMode::Gateway
        );
        // No transcript: falls back to auth.json.
        assert_eq!(
            detect_current(Agent::Codex, f.project.path(), ""),
            BillingMode::Subscription
        );
    }

    #[test]
    fn codex_env_overrides_make_a_login_ambiguous() {
        let f = Fixture::new()
            .home_file(".codex/auth.json", r#"{"auth_mode":"chatgpt"}"#)
            .env("CODEX_ACCESS_TOKEN", "t");
        assert_eq!(f.detect(Agent::Codex), BillingMode::Unknown);

        // CODEX_API_KEY only wins under `codex exec`, which the hook can't see.
        let f = Fixture::new()
            .home_file(".codex/auth.json", r#"{"auth_mode":"chatgpt"}"#)
            .env("CODEX_API_KEY", "sk");
        assert_eq!(f.detect(Agent::Codex), BillingMode::Unknown);
        let f = Fixture::new()
            .home_file(
                ".codex/auth.json",
                r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk"}"#,
            )
            .env("CODEX_API_KEY", "sk");
        assert_eq!(f.detect(Agent::Codex), BillingMode::ApiKey);
    }

    #[test]
    fn codex_malformed_files_are_unknown_not_an_error() {
        let f = Fixture::new()
            .home_file(".codex/auth.json", "{not json")
            .home_file(".codex/config.toml", "= broken")
            .rollout(json!({"type": "something_else"}));
        assert_eq!(f.detect(Agent::Codex), BillingMode::Unknown);
    }

    // ── other agents / helpers ──────────────────────────────────────────────

    #[test]
    fn agents_without_reliable_signals_are_unknown() {
        let f = Fixture::new()
            .home_file(".claude/.credentials.json", CLAUDE_OAUTH_CREDENTIALS)
            .home_file(".codex/auth.json", r#"{"auth_mode":"chatgpt"}"#)
            .env("ANTHROPIC_API_KEY", "sk");
        assert_eq!(f.detect(Agent::OpenCode), BillingMode::Unknown);
        assert_eq!(f.detect(Agent::Gsd), BillingMode::Unknown);
    }

    #[test]
    fn url_host_parsing() {
        assert_eq!(
            url_host("https://API.Anthropic.com/v1").as_deref(),
            Some("api.anthropic.com")
        );
        assert_eq!(
            url_host("http://user:pw@localhost:4000").as_deref(),
            Some("localhost")
        );
        assert_eq!(url_host("http://[::1]:4000/x").as_deref(), Some("::1"));
        assert_eq!(
            url_host("gw.example.com:8080").as_deref(),
            Some("gw.example.com")
        );
        assert_eq!(url_host("https://"), None);
    }

    #[test]
    fn direct_provider_hosts() {
        for direct in [
            "https://api.openai.com/v1",
            "https://my-res.openai.azure.com/openai",
            "https://my-res.services.ai.azure.com",
            "https://us-east5-aiplatform.googleapis.com/v1",
            "https://bedrock-runtime.eu-west-1.amazonaws.com",
            "https://bedrock-runtime-fips.us-east-1.amazonaws.com",
            "https://vpce-0abc.bedrock-runtime.us-east-1.vpce.amazonaws.com",
        ] {
            assert!(is_direct_provider_api(direct), "{direct}");
        }
        for gateway in [
            "http://localhost:4000",
            "https://litellm.example.com",
            "https://abc123.execute-api.eu-west-1.amazonaws.com/prod",
            "https://my-apim.azure-api.net/openai",
            "https://evilamazonaws.com/bedrock",
            "https://api.openai.com.evil.example",
        ] {
            assert!(!is_direct_provider_api(gateway), "{gateway}");
        }
    }

    // ── review round 2: precedence and routing ──────────────────────────────

    #[test]
    fn claude_helper_overridden_by_an_empty_higher_scope_value_is_off() {
        let f = Fixture::new()
            .home_file(".claude/.credentials.json", CLAUDE_OAUTH_CREDENTIALS)
            .home_file(
                ".claude/settings.json",
                r#"{"apiKeyHelper":"~/bin/key.sh"}"#,
            )
            .project_file(".claude/settings.local.json", r#"{"apiKeyHelper":""}"#)
            .env("ANTHROPIC_BASE_URL", "https://litellm.example.com");
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::Subscription);

        // A higher scope that doesn't mention the key leaves the lower one on.
        let f = Fixture::new()
            .home_file(
                ".claude/settings.json",
                r#"{"apiKeyHelper":"~/bin/key.sh"}"#,
            )
            .project_file(".claude/settings.local.json", r#"{"model":"opus"}"#);
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::ApiKey);
    }

    #[test]
    fn claude_bedrock_behind_aws_api_gateway_is_gateway() {
        let f = Fixture::new().env("CLAUDE_CODE_USE_BEDROCK", "1").env(
            "ANTHROPIC_BEDROCK_BASE_URL",
            "https://abc123.execute-api.eu-west-1.amazonaws.com/prod",
        );
        assert_eq!(f.detect(Agent::ClaudeCode), BillingMode::Gateway);
    }

    #[test]
    fn codex_system_config_redirect_is_gateway() {
        let f = Fixture::new()
            .home_file(".codex/auth.json", r#"{"auth_mode":"chatgpt"}"#)
            .system_config("openai_base_url = \"https://gw.example.com/v1\"\n")
            .rollout(session_meta("openai"));
        assert_eq!(f.detect(Agent::Codex), BillingMode::Gateway);
    }

    #[test]
    fn codex_config_layers_follow_codex_precedence() {
        // The user config overrides the system config...
        let f = Fixture::new()
            .home_file(".codex/auth.json", r#"{"auth_mode":"chatgpt"}"#)
            .system_config("openai_base_url = \"https://gw.example.com/v1\"\n")
            .home_file(
                ".codex/config.toml",
                "openai_base_url = \"https://api.openai.com/v1\"\n",
            );
        assert_eq!(f.detect(Agent::Codex), BillingMode::Subscription);

        // ...and the legacy managed config overrides the user config.
        let f = Fixture::new()
            .home_file(".codex/auth.json", r#"{"auth_mode":"chatgpt"}"#)
            .home_file(
                ".codex/config.toml",
                "openai_base_url = \"https://api.openai.com/v1\"\n",
            )
            .managed_codex_config("openai_base_url = \"https://gw.example.com/v1\"\n");
        assert_eq!(f.detect(Agent::Codex), BillingMode::Gateway);
    }

    #[test]
    fn codex_legacy_profile_selects_the_provider() {
        let f = Fixture::new()
            .home_file(".codex/auth.json", r#"{"auth_mode":"chatgpt"}"#)
            .home_file(
                ".codex/config.toml",
                "profile = \"work\"\nmodel_provider = \"openai\"\n\
                 [profiles.work]\nmodel_provider = \"litellm\"\n\
                 [model_providers.litellm]\nbase_url = \"https://litellm.example.com\"\n",
            );
        assert_eq!(f.detect(Agent::Codex), BillingMode::Gateway);
    }

    #[test]
    fn codex_provider_routing_wins_over_stored_bedrock_auth() {
        let f = Fixture::new()
            .home_file(".codex/auth.json", r#"{"auth_mode":"bedrockApiKey"}"#)
            .home_file(
                ".codex/config.toml",
                "[model_providers.litellm]\nbase_url = \"https://litellm.example.com\"\n",
            )
            .rollout(session_meta("litellm"));
        assert_eq!(f.detect(Agent::Codex), BillingMode::Gateway);
    }

    #[test]
    fn codex_custom_provider_behind_aws_api_gateway_is_gateway() {
        let f = Fixture::new()
            .home_file(
                ".codex/config.toml",
                "[model_providers.aws]\nbase_url = \"https://abc.execute-api.us-east-1.amazonaws.com/v1\"\n",
            )
            .rollout(session_meta("aws"));
        assert_eq!(f.detect(Agent::Codex), BillingMode::Gateway);
    }
}
