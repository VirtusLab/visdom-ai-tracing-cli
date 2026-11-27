//! Who pays for the calls of an agent `visdom` starts through the AI
//! Gateway: the project (its gateway key, today's behaviour) or the
//! developer's own Claude subscription. A choice inside gateway mode, not a
//! mode of its own.

use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use tracevault_cli::agent::Agent;

use crate::mode::{ModeReason, ResolvedMode, TracingMode, TrafficPolicy};

/// The environment variable that names who pays.
pub const BILLING_ENV: &str = "VISDOM_GATEWAY_BILLING";

/// Who pays for the agent's calls through the gateway.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Billing {
    /// The project, through the developer's personal gateway key.
    #[default]
    Project,
    /// The developer's own Claude subscription (Claude Code only).
    Subscription,
}

impl Billing {
    pub fn name(self) -> &'static str {
        match self {
            Billing::Project => "project",
            Billing::Subscription => "subscription",
        }
    }
}

impl std::fmt::Display for Billing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Parse a billing value as the command line and the config files spell it.
pub fn parse_billing(name: &str) -> Result<Billing, String> {
    match name.trim().to_ascii_lowercase().as_str() {
        "project" => Ok(Billing::Project),
        "subscription" => Ok(Billing::Subscription),
        _ => Err(format!(
            "unknown billing `{name}` (use project or subscription)"
        )),
    }
}

/// Where the billing of a session was set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BillingSource {
    Flag,
    Env,
    /// `billing` under `[gateway]` in the repository's (or workspace
    /// folder's) `.visdom/config.toml`.
    Repo,
    /// `billing` under `[gateway]` in the user config at this path.
    User(String),
    /// Nobody set it: the project pays.
    Default,
}

impl BillingSource {
    /// Said as a person would read it.
    pub fn describe(&self) -> String {
        match self {
            BillingSource::Flag => "--billing".to_string(),
            BillingSource::Env => format!("{BILLING_ENV} in the environment"),
            BillingSource::Repo => "billing under [gateway] in .visdom/config.toml".to_string(),
            BillingSource::User(path) => format!("billing under [gateway] in {path}"),
            BillingSource::Default => "the default".to_string(),
        }
    }
}

/// Who pays, and who said so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedBilling {
    pub billing: Billing,
    pub source: BillingSource,
}

impl ResolvedBilling {
    /// `subscription (--billing)`.
    pub fn describe(&self) -> String {
        format!("{} ({})", self.billing.name(), self.source.describe())
    }
}

/// Who pays: `--billing`, then `VISDOM_GATEWAY_BILLING` (a blank one is not
/// set, an unknown value is an error), then the repository config, then the
/// user config (`user_file` names it), then the project.
pub fn configured_billing(
    flag: Option<Billing>,
    env: Option<&str>,
    repo: Option<Billing>,
    user: Option<Billing>,
    user_file: &str,
) -> Result<ResolvedBilling, String> {
    let resolved = |billing, source| Ok(ResolvedBilling { billing, source });
    if let Some(billing) = flag {
        return resolved(billing, BillingSource::Flag);
    }
    if let Some(name) = env.filter(|v| !v.trim().is_empty()) {
        let billing = parse_billing(name).map_err(|e| format!("{BILLING_ENV}: {e}"))?;
        return resolved(billing, BillingSource::Env);
    }
    match (repo, user) {
        (Some(billing), _) => resolved(billing, BillingSource::Repo),
        (None, Some(billing)) => resolved(billing, BillingSource::User(user_file.to_string())),
        (None, None) => resolved(Billing::Project, BillingSource::Default),
    }
}

/// What the billing means for a session in `mode` with `agent`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BillingUse {
    /// The setting applies.
    Applies,
    /// The workspace's policy keeps sessions off the gateway: nothing to pay
    /// through it, so the setting is not used.
    Unused,
    /// A combination that cannot work; the text says why and what to do.
    Rejected(String),
}

/// Whether `billing` can be used in `mode` with `agent`. `subscription`
/// needs the gateway (hooks have no gateway key to pay or not pay with) and
/// Claude Code; under a `direct` policy the setting is simply unused.
pub fn billing_use(billing: &ResolvedBilling, mode: &ResolvedMode, agent: Agent) -> BillingUse {
    if billing.billing == Billing::Project {
        return match mode.mode {
            TracingMode::Gateway => BillingUse::Applies,
            TracingMode::Hooks => BillingUse::Unused,
        };
    }
    if mode.mode == TracingMode::Hooks {
        if mode.reason == ModeReason::Policy(TrafficPolicy::Direct) {
            return BillingUse::Unused;
        }
        let why = match mode.reason {
            ModeReason::Default => "nobody chose gateway mode, so sessions are traced by hooks",
            _ => "this session is traced by hooks",
        };
        return BillingUse::Rejected(format!(
            "billing = subscription ({}) is for sessions through the AI Gateway, but {why}: \
             hooks tracing has no gateway key that would pay or not pay. Use --mode gateway, or \
             set billing to project",
            billing.source.describe()
        ));
    }
    if agent != Agent::ClaudeCode {
        return BillingUse::Rejected(format!(
            "billing = subscription ({}) is not supported yet for {}: only Claude Code can use a \
             subscription through the AI Gateway. Use --billing project, or set billing to project",
            billing.source.describe(),
            agent.label()
        ));
    }
    BillingUse::Applies
}

/// Why a launch through the gateway with `billing` cannot start yet, if it
/// cannot. Starting Claude Code on its subscription login is not in this
/// version: a launch that silently let the project pay instead would be
/// wrong, so it stops.
pub fn launch_unavailable(billing: &ResolvedBilling) -> Option<String> {
    (billing.billing == Billing::Subscription).then(|| {
        format!(
            "billing = subscription ({}): starting the agent on your Claude subscription through \
             the AI Gateway is not available in this version of visdom yet. Use --billing \
             project for now, or set billing to project",
            billing.source.describe()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::effective_mode;
    use Billing::{Project, Subscription};

    fn pick(
        flag: Option<Billing>,
        env: Option<&str>,
        repo: Option<Billing>,
        user: Option<Billing>,
    ) -> Result<ResolvedBilling, String> {
        configured_billing(flag, env, repo, user, "/home/u/.config/visdom/config.toml")
    }

    fn resolved(billing: Billing, source: BillingSource) -> ResolvedBilling {
        ResolvedBilling { billing, source }
    }

    #[test]
    fn billing_parses_as_it_is_written() {
        assert_eq!(parse_billing("project"), Ok(Project));
        assert_eq!(parse_billing(" Subscription "), Ok(Subscription));
        assert!(parse_billing("free")
            .unwrap_err()
            .contains("project or subscription"));
        assert_eq!(Subscription.name(), "subscription");
        assert_eq!(Billing::default(), Project);
    }

    #[test]
    fn who_pays_is_flag_then_env_then_repo_then_user_then_the_project() {
        assert_eq!(
            pick(None, None, None, None),
            Ok(resolved(Project, BillingSource::Default))
        );
        assert_eq!(
            pick(None, None, None, Some(Subscription)),
            Ok(resolved(
                Subscription,
                BillingSource::User("/home/u/.config/visdom/config.toml".to_string())
            ))
        );
        assert_eq!(
            pick(None, None, Some(Project), Some(Subscription)),
            Ok(resolved(Project, BillingSource::Repo))
        );
        assert_eq!(
            pick(None, Some("subscription"), Some(Project), Some(Project)),
            Ok(resolved(Subscription, BillingSource::Env))
        );
        assert_eq!(
            pick(
                Some(Project),
                Some("subscription"),
                Some(Subscription),
                Some(Subscription)
            ),
            Ok(resolved(Project, BillingSource::Flag))
        );
    }

    #[test]
    fn a_blank_variable_is_not_set_and_an_unknown_one_is_an_error() {
        assert_eq!(
            pick(None, Some("  "), None, Some(Subscription))
                .unwrap()
                .billing,
            Subscription
        );
        let err = pick(None, Some("free"), None, None).unwrap_err();
        assert!(err.starts_with("VISDOM_GATEWAY_BILLING:"), "{err}");
        assert!(err.contains("project or subscription"), "{err}");
    }

    #[test]
    fn sources_are_said_as_a_person_reads_them() {
        let user = pick(None, None, None, Some(Project)).unwrap();
        assert_eq!(
            user.describe(),
            "project (billing under [gateway] in /home/u/.config/visdom/config.toml)"
        );
        assert_eq!(
            pick(None, Some("project"), None, None)
                .unwrap()
                .source
                .describe(),
            "VISDOM_GATEWAY_BILLING in the environment"
        );
        assert_eq!(BillingSource::Default.describe(), "the default");
    }

    fn sub(source: BillingSource) -> ResolvedBilling {
        resolved(Subscription, source)
    }

    #[test]
    fn a_subscription_through_the_gateway_applies_to_claude_code() {
        for policy in [None, Some(TrafficPolicy::PerAgent)] {
            let mode = effective_mode(Some(TracingMode::Gateway), policy);
            assert_eq!(
                billing_use(&sub(BillingSource::Flag), &mode, Agent::ClaudeCode),
                BillingUse::Applies
            );
        }
        // `enforced` means through the gateway, not paid by the project.
        let enforced = effective_mode(None, Some(TrafficPolicy::Enforced));
        assert_eq!(
            billing_use(&sub(BillingSource::Repo), &enforced, Agent::ClaudeCode),
            BillingUse::Applies
        );
        assert_eq!(
            billing_use(
                &resolved(Project, BillingSource::Default),
                &enforced,
                Agent::ClaudeCode
            ),
            BillingUse::Applies
        );
    }

    #[test]
    fn a_subscription_outside_the_gateway_is_rejected_and_says_why() {
        for mode in [
            effective_mode(Some(TracingMode::Hooks), None),
            effective_mode(None, Some(TrafficPolicy::PerAgent)),
        ] {
            match billing_use(&sub(BillingSource::Flag), &mode, Agent::ClaudeCode) {
                BillingUse::Rejected(why) => {
                    assert!(why.contains("--billing"), "{why}");
                    assert!(why.contains("hooks"), "{why}");
                    assert!(why.contains("--mode gateway"), "{why}");
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn a_direct_policy_leaves_the_setting_unused() {
        let direct = effective_mode(Some(TracingMode::Gateway), Some(TrafficPolicy::Direct));
        assert_eq!(
            billing_use(&sub(BillingSource::Env), &direct, Agent::ClaudeCode),
            BillingUse::Unused
        );
        assert_eq!(
            billing_use(
                &resolved(Project, BillingSource::Default),
                &direct,
                Agent::Codex
            ),
            BillingUse::Unused
        );
    }

    #[test]
    fn codex_cannot_use_a_subscription_yet() {
        let mode = effective_mode(Some(TracingMode::Gateway), None);
        match billing_use(&sub(BillingSource::Env), &mode, Agent::Codex) {
            BillingUse::Rejected(why) => {
                assert!(why.contains("not supported yet for Codex"), "{why}");
                assert!(why.contains("VISDOM_GATEWAY_BILLING"), "{why}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            billing_use(
                &resolved(Project, BillingSource::Default),
                &mode,
                Agent::Codex
            ),
            BillingUse::Applies
        );
    }

    #[test]
    fn only_a_subscription_launch_is_unavailable() {
        assert!(launch_unavailable(&resolved(Project, BillingSource::Flag)).is_none());
        let why = launch_unavailable(&sub(BillingSource::Flag)).unwrap();
        assert!(why.contains("not available in this version"), "{why}");
    }
}
