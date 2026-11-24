//! How a developer's agent sessions reach AI Tracing: the developer's choice,
//! bounded by the workspace's AI traffic policy.

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

/// The environment variable that names the developer's choice of mode.
pub const MODE_ENV: &str = tracevault_cli::commands::stream::TRACING_MODE_ENV;
/// Set on an agent started through the AI Gateway, to that agent's name, and
/// on nothing else: what tells that agent's capture hooks to record nothing.
pub const GATEWAY_SESSION_ENV: &str = tracevault_cli::commands::stream::GATEWAY_SESSION_ENV;

/// The capture path of a session. Exactly one is meant to be active.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum TracingMode {
    /// Agent hooks and git hooks capture the session.
    Hooks,
    /// The agent's LLM traffic goes through the AI Gateway, which records it.
    Gateway,
}

impl TracingMode {
    pub fn name(self) -> &'static str {
        match self {
            TracingMode::Hooks => "hooks",
            TracingMode::Gateway => "gateway",
        }
    }
}

/// Parse a mode as the command line and the config files spell it.
pub fn parse_mode(name: &str) -> Result<TracingMode, String> {
    match name.trim().to_ascii_lowercase().as_str() {
        "hooks" => Ok(TracingMode::Hooks),
        "gateway" => Ok(TracingMode::Gateway),
        _ => Err(format!(
            "unknown tracing mode `{name}` (use hooks or gateway)"
        )),
    }
}

/// The workspace's AI traffic policy, set by an admin in Control Plane
/// (`ai_traffic_policy` of `GET /api/v1/workspace/settings`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrafficPolicy {
    /// Nothing goes through the AI Gateway.
    Direct,
    /// Each developer (and agent) chooses.
    PerAgent,
    /// All LLM traffic goes through the AI Gateway.
    Enforced,
}

impl TrafficPolicy {
    pub fn name(self) -> &'static str {
        match self {
            TrafficPolicy::Direct => "direct",
            TrafficPolicy::PerAgent => "per_agent",
            TrafficPolicy::Enforced => "enforced",
        }
    }
}

/// Why a session runs in the mode it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeReason {
    /// The developer chose it and the policy leaves the choice to them (or
    /// could not be read).
    Chosen,
    /// Nobody chose: hooks, the mode that needs nothing else.
    Default,
    /// The workspace policy decides, whatever was chosen.
    Policy(TrafficPolicy),
}

/// The mode of a session and how it came about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedMode {
    pub mode: TracingMode,
    pub reason: ModeReason,
    /// What the developer chose, when the policy overrules it.
    pub overruled: Option<TracingMode>,
}

/// The mode somebody chose: `--mode`, then `VISDOM_TRACING_MODE`, then the
/// repository config, then the user config. `None` when nobody did.
pub fn configured_mode(
    flag: Option<TracingMode>,
    env: Option<&str>,
    repo: Option<TracingMode>,
    user: Option<TracingMode>,
) -> Result<Option<TracingMode>, String> {
    if flag.is_some() {
        return Ok(flag);
    }
    match env.filter(|v| !v.trim().is_empty()) {
        Some(name) => parse_mode(name)
            .map(Some)
            .map_err(|e| format!("{MODE_ENV}: {e}")),
        None => Ok(repo.or(user)),
    }
}

/// The mode a session runs in. An admin may enforce one (`enforced`: gateway,
/// `direct`: hooks); under `per_agent`, or when the policy is not known, the
/// developer's choice stands, and hooks when they made none.
pub fn effective_mode(chosen: Option<TracingMode>, policy: Option<TrafficPolicy>) -> ResolvedMode {
    let forced = match policy {
        Some(TrafficPolicy::Enforced) => Some(TracingMode::Gateway),
        Some(TrafficPolicy::Direct) => Some(TracingMode::Hooks),
        Some(TrafficPolicy::PerAgent) | None => None,
    };
    match (forced, chosen, policy) {
        (Some(mode), chosen, Some(policy)) => ResolvedMode {
            mode,
            reason: ModeReason::Policy(policy),
            overruled: chosen.filter(|c| *c != mode),
        },
        (_, Some(mode), _) => ResolvedMode {
            mode,
            reason: ModeReason::Chosen,
            overruled: None,
        },
        _ => ResolvedMode {
            mode: TracingMode::Hooks,
            reason: ModeReason::Default,
            overruled: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TracingMode::{Gateway, Hooks};

    #[test]
    fn modes_parse_as_they_are_written() {
        assert_eq!(parse_mode("hooks"), Ok(Hooks));
        assert_eq!(parse_mode(" Gateway "), Ok(Gateway));
        assert!(parse_mode("proxy")
            .unwrap_err()
            .contains("hooks or gateway"));
        assert_eq!(Gateway.name(), "gateway");
    }

    #[test]
    fn the_choice_is_flag_then_env_then_repo_then_user() {
        let pick = |flag, env, repo, user| configured_mode(flag, env, repo, user);
        assert_eq!(pick(None, None, None, None), Ok(None));
        assert_eq!(pick(None, None, None, Some(Gateway)), Ok(Some(Gateway)));
        assert_eq!(
            pick(None, None, Some(Hooks), Some(Gateway)),
            Ok(Some(Hooks))
        );
        assert_eq!(
            pick(None, Some("gateway"), Some(Hooks), Some(Hooks)),
            Ok(Some(Gateway))
        );
        assert_eq!(
            pick(Some(Hooks), Some("gateway"), Some(Gateway), Some(Gateway)),
            Ok(Some(Hooks))
        );
        assert_eq!(
            pick(None, Some("  "), None, Some(Gateway)),
            Ok(Some(Gateway))
        );
        let err = pick(None, Some("nope"), None, None).unwrap_err();
        assert!(err.starts_with("VISDOM_TRACING_MODE:"), "{err}");
    }

    #[test]
    fn without_an_enforcing_policy_the_choice_stands() {
        for policy in [None, Some(TrafficPolicy::PerAgent)] {
            assert_eq!(
                effective_mode(Some(Gateway), policy),
                ResolvedMode {
                    mode: Gateway,
                    reason: ModeReason::Chosen,
                    overruled: None
                }
            );
            assert_eq!(
                effective_mode(None, policy),
                ResolvedMode {
                    mode: Hooks,
                    reason: ModeReason::Default,
                    overruled: None
                }
            );
        }
    }

    #[test]
    fn an_enforcing_policy_decides_and_says_what_it_overruled() {
        let enforced = Some(TrafficPolicy::Enforced);
        assert_eq!(
            effective_mode(Some(Hooks), enforced),
            ResolvedMode {
                mode: Gateway,
                reason: ModeReason::Policy(TrafficPolicy::Enforced),
                overruled: Some(Hooks)
            }
        );
        assert_eq!(effective_mode(None, enforced).mode, Gateway);
        assert_eq!(effective_mode(Some(Gateway), enforced).overruled, None);

        let direct = Some(TrafficPolicy::Direct);
        assert_eq!(
            effective_mode(Some(Gateway), direct),
            ResolvedMode {
                mode: Hooks,
                reason: ModeReason::Policy(TrafficPolicy::Direct),
                overruled: Some(Gateway)
            }
        );
        assert_eq!(effective_mode(None, direct).overruled, None);
    }

    #[test]
    fn policies_read_control_planes_spelling() {
        let parse = |s: &str| serde_json::from_str::<TrafficPolicy>(s);
        assert_eq!(parse("\"per_agent\"").unwrap(), TrafficPolicy::PerAgent);
        assert_eq!(parse("\"enforced\"").unwrap(), TrafficPolicy::Enforced);
        assert_eq!(parse("\"direct\"").unwrap(), TrafficPolicy::Direct);
        assert!(parse("\"other\"").is_err());
    }
}
