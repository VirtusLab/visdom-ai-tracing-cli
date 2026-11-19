//! visdom's own settings: the user file (`<config dir>/visdom/config.toml`)
//! and the optional committed repository file (`.visdom/config.toml`), plus
//! the choice of agent.

use std::path::{Path, PathBuf};

use clap::ValueEnum;
use serde::{Deserialize, Deserializer};
use tracevault_cli::agent::Agent;

use crate::mode::{parse_mode, TracingMode};

/// `<config dir>/visdom/config.toml`.
#[derive(Debug, Default, Deserialize)]
pub struct UserConfig {
    /// The agent `visdom` launches by default.
    #[serde(default, deserialize_with = "de_agent")]
    pub agent: Option<Agent>,
    /// The bazaar MCP endpoint, when the server does not publish it.
    #[serde(default)]
    pub bazaar_url: Option<String>,
    /// How this developer's sessions are traced: `hooks` or `gateway`.
    #[serde(default, deserialize_with = "de_mode")]
    pub tracing_mode: Option<TracingMode>,
    /// Control Plane's address, when the server does not publish it.
    #[serde(default)]
    pub control_plane_url: Option<String>,
    /// Per-agent executable overrides.
    #[serde(default)]
    pub agents: AgentCommands,
    /// The AI Gateway, for `tracing_mode = "gateway"`.
    #[serde(default)]
    pub gateway: GatewayConfig,
}

/// `[gateway]`: how to reach and use the AI Gateway.
#[derive(Debug, Default, Deserialize)]
pub struct GatewayConfig {
    /// The gateway console's address, when the server does not publish it.
    #[serde(default)]
    pub url: Option<String>,
    /// The gateway project (its slug) whose personal key the agent uses.
    #[serde(default)]
    pub project: Option<String>,
    /// The model alias Claude Code asks the gateway for.
    #[serde(default, alias = "claude-code-model")]
    pub claude_code_model: Option<String>,
    /// The model alias Codex asks the gateway for.
    #[serde(default, alias = "codex-model")]
    pub codex_model: Option<String>,
}

impl GatewayConfig {
    /// The configured model alias for `agent`, if any.
    pub fn model_for(&self, agent: Agent) -> Option<String> {
        match agent {
            Agent::ClaudeCode => self.claude_code_model.clone(),
            Agent::Codex => self.codex_model.clone(),
            _ => None,
        }
    }
}

/// `[agents]`: the executable to run for an agent instead of the default.
#[derive(Debug, Default, Deserialize)]
pub struct AgentCommands {
    #[serde(default, alias = "claude-code")]
    pub claude_code: Option<String>,
    #[serde(default)]
    pub codex: Option<String>,
}

impl AgentCommands {
    /// The configured executable for `agent`, if any.
    pub fn program_for(&self, agent: Agent) -> Option<String> {
        match agent {
            Agent::ClaudeCode => self.claude_code.clone(),
            Agent::Codex => self.codex.clone(),
            _ => None,
        }
    }
}

/// `.visdom/config.toml` in a repository: what the team prefers for it.
#[derive(Debug, Default, Deserialize)]
pub struct RepoConfig {
    #[serde(default, deserialize_with = "de_agent")]
    pub agent: Option<Agent>,
    /// The tracing mode the team prefers here; a developer's own choice on
    /// the command line or in the environment still wins.
    #[serde(default, deserialize_with = "de_mode")]
    pub tracing_mode: Option<TracingMode>,
    #[serde(default)]
    pub gateway: RepoGatewayConfig,
}

/// `[gateway]` in a repository: the gateway project this repository belongs to.
#[derive(Debug, Default, Deserialize)]
pub struct RepoGatewayConfig {
    #[serde(default)]
    pub project: Option<String>,
}

/// The `[agents]` key that overrides `agent`'s executable.
pub fn agent_config_key(agent: Agent) -> &'static str {
    match agent {
        Agent::ClaudeCode => "claude_code",
        Agent::Codex => "codex",
        Agent::Gsd => "gsd",
        Agent::OpenCode => "opencode",
        Agent::Amp => "amp",
    }
}

/// Parse an agent name as the command line spells it (`claude-code`, `codex`).
pub fn parse_agent(name: &str) -> Result<Agent, String> {
    Agent::from_str(name.trim(), true)
        .map_err(|_| format!("unknown agent `{name}` (visdom can launch claude-code and codex)"))
}

fn de_agent<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Agent>, D::Error> {
    match Option::<String>::deserialize(d)? {
        None => Ok(None),
        Some(name) => parse_agent(&name)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

fn de_mode<'de, D: Deserializer<'de>>(d: D) -> Result<Option<TracingMode>, D::Error> {
    match Option::<String>::deserialize(d)? {
        None => Ok(None),
        Some(name) => parse_mode(&name)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

pub fn user_config_path_in(config_root: &Path) -> PathBuf {
    config_root.join("visdom").join("config.toml")
}

pub fn repo_config_path_in(project_root: &Path) -> PathBuf {
    project_root.join(".visdom").join("config.toml")
}

fn load<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    toml::from_str(&text).map_err(|e| format!("cannot parse {}: {e}", path.display()))
}

/// The user config under `config_root` (the platform config directory). A
/// missing file is the default config.
pub fn load_user_config_in(config_root: &Path) -> Result<UserConfig, String> {
    load(&user_config_path_in(config_root))
}

/// The repository config in `project_root`. A missing file is the default.
pub fn load_repo_config_in(project_root: &Path) -> Result<RepoConfig, String> {
    load(&repo_config_path_in(project_root))
}

/// The agent somebody chose outside the command line: `VISDOM_AGENT`, then
/// the repository config, then the user config. `None` when nobody did.
pub fn configured_agent(
    env: Option<&str>,
    repo: &RepoConfig,
    user: &UserConfig,
) -> Result<Option<Agent>, String> {
    match env.filter(|v| !v.trim().is_empty()) {
        Some(name) => parse_agent(name)
            .map(Some)
            .map_err(|e| format!("VISDOM_AGENT: {e}")),
        None => Ok(repo.agent.or(user.agent)),
    }
}

/// Which agent to use: `--agent`, then `VISDOM_AGENT`, then the repository
/// config, then the user config, then Claude Code. Only Claude Code and Codex
/// can be launched.
pub fn resolve_agent(
    flag: Option<Agent>,
    env: Option<&str>,
    repo: &RepoConfig,
    user: &UserConfig,
) -> Result<Agent, String> {
    let agent = match flag {
        Some(agent) => agent,
        None => configured_agent(env, repo, user)?.unwrap_or(Agent::ClaudeCode),
    };
    match agent {
        Agent::ClaudeCode | Agent::Codex => Ok(agent),
        other => Err(format!(
            "visdom can launch claude-code and codex; {} is not supported yet",
            other.tool_name()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(agent: Option<Agent>) -> UserConfig {
        UserConfig {
            agent,
            ..Default::default()
        }
    }

    #[test]
    fn agent_precedence_flag_env_repo_user_default() {
        let repo = RepoConfig {
            agent: Some(Agent::Codex),
            ..Default::default()
        };
        let none = RepoConfig::default();
        let cases = [
            // (flag, env, repo, user) => expected
            (
                Some(Agent::ClaudeCode),
                Some("codex"),
                &repo,
                Some(Agent::Codex),
                Agent::ClaudeCode,
            ),
            (
                None,
                Some("codex"),
                &none,
                Some(Agent::ClaudeCode),
                Agent::Codex,
            ),
            (None, None, &repo, Some(Agent::ClaudeCode), Agent::Codex),
            (None, None, &none, Some(Agent::Codex), Agent::Codex),
            (None, None, &none, None, Agent::ClaudeCode),
        ];
        for (flag, env, repo, user_agent, expected) in cases {
            assert_eq!(
                resolve_agent(flag, env, repo, &user(user_agent)),
                Ok(expected),
                "flag={flag:?} env={env:?} repo={:?} user={user_agent:?}",
                repo.agent
            );
        }
        // An empty variable is the same as an unset one.
        assert_eq!(
            resolve_agent(None, Some(""), &none, &user(None)),
            Ok(Agent::ClaudeCode)
        );
    }

    #[test]
    fn configured_agent_is_none_when_nobody_chose() {
        let none = RepoConfig::default();
        assert_eq!(configured_agent(None, &none, &user(None)), Ok(None));
        assert_eq!(configured_agent(Some(" "), &none, &user(None)), Ok(None));
        assert_eq!(
            configured_agent(None, &none, &user(Some(Agent::Codex))),
            Ok(Some(Agent::Codex))
        );
        assert_eq!(
            configured_agent(Some("codex"), &none, &user(None)),
            Ok(Some(Agent::Codex))
        );
        assert!(configured_agent(Some("emacs"), &none, &user(None)).is_err());
    }

    #[test]
    fn unsupported_agent_is_error() {
        let err = resolve_agent(
            Some(Agent::Gsd),
            None,
            &RepoConfig::default(),
            &UserConfig::default(),
        )
        .unwrap_err();
        assert!(err.contains("gsd is not supported yet"), "{err}");
        let err = resolve_agent(
            None,
            Some("emacs"),
            &RepoConfig::default(),
            &UserConfig::default(),
        )
        .unwrap_err();
        assert!(
            err.contains("VISDOM_AGENT") && err.contains("emacs"),
            "{err}"
        );
    }

    #[test]
    fn missing_config_files_are_default() {
        let tmp = tempfile::tempdir().unwrap();
        let user = load_user_config_in(tmp.path()).unwrap();
        assert!(user.agent.is_none() && user.bazaar_url.is_none());
        assert!(load_repo_config_in(tmp.path()).unwrap().agent.is_none());
    }

    #[test]
    fn user_config_reads_every_field() {
        let tmp = tempfile::tempdir().unwrap();
        let path = user_config_path_in(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "agent = \"codex\"\nbazaar_url = \"https://ctx.example.com/mcp/remote\"\n\
             [agents]\nclaude_code = \"/opt/claude\"\ncodex = \"/opt/codex\"\n",
        )
        .unwrap();
        let user = load_user_config_in(tmp.path()).unwrap();
        assert_eq!(user.agent, Some(Agent::Codex));
        assert_eq!(
            user.bazaar_url.as_deref(),
            Some("https://ctx.example.com/mcp/remote")
        );
        assert_eq!(
            user.agents.program_for(Agent::ClaudeCode).as_deref(),
            Some("/opt/claude")
        );
        assert_eq!(
            user.agents.program_for(Agent::Codex).as_deref(),
            Some("/opt/codex")
        );
    }

    #[test]
    fn malformed_user_config_is_error_naming_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = user_config_path_in(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "agent = [not toml").unwrap();
        let err = load_user_config_in(tmp.path()).unwrap_err();
        assert!(err.contains(&path.display().to_string()), "{err}");

        std::fs::write(&path, "agent = \"emacs\"\n").unwrap();
        let err = load_user_config_in(tmp.path()).unwrap_err();
        assert!(err.contains("emacs"), "{err}");
    }

    #[test]
    fn the_tracing_mode_and_gateway_settings_are_read() {
        let tmp = tempfile::tempdir().unwrap();
        let path = user_config_path_in(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "tracing_mode = \"gateway\"\ncontrol_plane_url = \"https://cp.example.com\"\n\n\
             [gateway]\nurl = \"https://gw.example.com\"\nproject = \"alpha\"\n\
             claude_code_model = \"claude-sonnet\"\ncodex-model = \"gpt-general\"\n",
        )
        .unwrap();
        let user = load_user_config_in(tmp.path()).unwrap();
        assert_eq!(user.tracing_mode, Some(TracingMode::Gateway));
        assert_eq!(
            user.control_plane_url.as_deref(),
            Some("https://cp.example.com")
        );
        assert_eq!(user.gateway.url.as_deref(), Some("https://gw.example.com"));
        assert_eq!(user.gateway.project.as_deref(), Some("alpha"));
        assert_eq!(
            user.gateway.model_for(Agent::ClaudeCode).as_deref(),
            Some("claude-sonnet")
        );
        assert_eq!(
            user.gateway.model_for(Agent::Codex).as_deref(),
            Some("gpt-general")
        );
        assert_eq!(user.gateway.model_for(Agent::Amp), None);
    }

    #[test]
    fn a_repository_names_its_mode_and_gateway_project() {
        let tmp = tempfile::tempdir().unwrap();
        let path = repo_config_path_in(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "tracing_mode = \"hooks\"\n\n[gateway]\nproject = \"beta\"\n",
        )
        .unwrap();
        let repo = load_repo_config_in(tmp.path()).unwrap();
        assert_eq!(repo.tracing_mode, Some(TracingMode::Hooks));
        assert_eq!(repo.gateway.project.as_deref(), Some("beta"));
        assert_eq!(repo.agent, None);
    }

    #[test]
    fn an_unknown_tracing_mode_is_a_config_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = user_config_path_in(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "tracing_mode = \"proxy\"\n").unwrap();
        let err = load_user_config_in(tmp.path()).unwrap_err();
        assert!(err.contains("hooks or gateway"), "{err}");
    }
}
