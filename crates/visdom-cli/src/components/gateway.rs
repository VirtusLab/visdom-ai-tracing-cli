//! Which way this developer's sessions are traced, who decided it, and —
//! through the AI Gateway — whether the gateway is ready for them.

use tracevault_cli::agent::Agent;

use crate::billing::{self, Billing, BillingUse, ResolvedBilling};
use crate::finding::{Finding, Severity};
use crate::gateway::OpenProject;
use crate::mode::{ModeReason, ResolvedMode, TracingMode, TrafficPolicy};
use crate::suite::PolicyState;

const COMPONENT: &str = "gateway";

/// What a session through the gateway will use, worked out before anything
/// is issued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatewayPlan {
    /// The gateway console, which issues the personal key.
    pub console_url: String,
    /// The LLM gateway the agent calls.
    pub llm_url: String,
    pub project: OpenProject,
}

/// The mode, and the policy behind it. Nothing here needs attention unless
/// the policy overruled what the developer chose.
fn mode_findings(mode: &ResolvedMode, policy: &PolicyState) -> Vec<Finding> {
    const ID: &str = "gateway.mode";
    let overruled = |chosen: TracingMode, policy: TrafficPolicy| {
        format!(
            "you chose {} tracing, but the workspace's AI traffic policy is `{}`: sessions {}",
            chosen.name(),
            policy.name(),
            match mode.mode {
                TracingMode::Gateway => "go through the AI Gateway",
                TracingMode::Hooks => "are captured by hooks, not through the AI Gateway",
            }
        )
    };
    let finding = match (mode.mode, mode.reason, mode.overruled) {
        (_, ModeReason::Policy(policy), Some(chosen)) => {
            Finding::new(COMPONENT, ID, Severity::Warn, overruled(chosen, policy))
        }
        (TracingMode::Gateway, ModeReason::Policy(_), None) => Finding::new(
            COMPONENT,
            ID,
            Severity::Ok,
            "sessions go through the AI Gateway (required by the workspace's AI traffic policy)",
        ),
        (TracingMode::Hooks, ModeReason::Policy(_), None) => Finding::new(
            COMPONENT,
            ID,
            Severity::Ok,
            "sessions are captured by hooks (the workspace's AI traffic policy routes nothing \
             through the AI Gateway)",
        ),
        (TracingMode::Gateway, _, _) => Finding::new(
            COMPONENT,
            ID,
            Severity::Ok,
            "sessions go through the AI Gateway (your choice)",
        ),
        (TracingMode::Hooks, ModeReason::Chosen, _) => Finding::new(
            COMPONENT,
            ID,
            Severity::Ok,
            "sessions are captured by hooks (your choice)",
        ),
        (TracingMode::Hooks, _, _) => Finding::new(
            COMPONENT,
            ID,
            Severity::Ok,
            "sessions are captured by hooks (set tracing_mode = \"gateway\" in your visdom \
             config, or pass --mode gateway, to use the AI Gateway)",
        ),
    };
    let mut findings = vec![finding];
    // Not being able to read the policy is worth a line in `doctor`, not one
    // above every launch: most installations simply have none to read.
    match policy {
        PolicyState::Fresh(_) => {}
        PolicyState::Remembered(policy, why) => findings.push(Finding::new(
            COMPONENT,
            "gateway.policy",
            Severity::Info,
            format!(
                "the workspace's AI traffic policy could not be read ({why}); using the last \
                 one known, `{}`",
                policy.name()
            ),
        )),
        PolicyState::Unknown(why) => findings.push(Finding::new(
            COMPONENT,
            "gateway.policy",
            Severity::Info,
            format!("the workspace's AI traffic policy is not known ({why}): your choice applies"),
        )),
    }
    findings
}

/// The findings about the mode and, in gateway mode, about the gateway.
/// `plan` is `None` outside gateway mode.
pub fn check(
    mode: &ResolvedMode,
    policy: &PolicyState,
    plan: Option<&Result<GatewayPlan, String>>,
    agent: Agent,
) -> Vec<Finding> {
    let mut findings = mode_findings(mode, policy);
    let Some(plan) = plan else {
        return findings;
    };
    match plan {
        Ok(plan) => {
            findings.push(Finding::new(
                COMPONENT,
                "gateway.project",
                Severity::Ok,
                format!(
                    "AI Gateway project `{}` at {} (a personal key is issued when the agent starts)",
                    plan.project.slug, plan.llm_url
                ),
            ));
            if agent == Agent::Codex {
                findings.push(Finding::new(
                    COMPONENT,
                    "gateway.codex_key",
                    Severity::Info,
                    "Codex is given its key once: a session running when the key expires (24 \
                     hours at most) or is replaced has to be restarted with `visdom`",
                ));
            }
        }
        Err(why) => {
            let required = matches!(mode.reason, ModeReason::Policy(TrafficPolicy::Enforced));
            findings.push(Finding::new(
                COMPONENT,
                "gateway.project",
                Severity::Error,
                if required {
                    format!("{why} — the workspace requires the AI Gateway, so the agent will not be started")
                } else {
                    format!("{why} — the agent will be started with hooks tracing instead")
                },
            ));
        }
    }
    findings
}

/// Who pays for the agent's calls through the gateway, and who said so
/// (`gateway.billing`). Nothing outside the gateway unless a subscription is
/// set where it cannot be used.
pub fn billing_finding(billing: &ResolvedBilling, usage: &BillingUse) -> Option<Finding> {
    const ID: &str = "gateway.billing";
    let finding = match (usage, billing.billing) {
        (BillingUse::Rejected(why), _) => Finding::new(COMPONENT, ID, Severity::Error, why.clone()),
        (BillingUse::Unused, Billing::Project) => return None,
        (BillingUse::Unused, Billing::Subscription) => Finding::new(
            COMPONENT,
            ID,
            Severity::Info,
            format!(
                "billing = {}: not used, the workspace's AI traffic policy keeps sessions off the \
                 AI Gateway",
                billing.describe()
            ),
        ),
        (BillingUse::Applies, Billing::Project) => Finding::new(
            COMPONENT,
            ID,
            Severity::Ok,
            format!(
                "billing = {}: the project pays for the agent's calls, through your personal key",
                billing.describe()
            ),
        ),
        (BillingUse::Applies, Billing::Subscription) => Finding::new(
            COMPONENT,
            ID,
            Severity::Warn,
            billing::launch_unavailable(billing).unwrap_or_default(),
        ),
    };
    Some(finding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::effective_mode;

    fn fresh(policy: TrafficPolicy) -> PolicyState {
        PolicyState::Fresh(policy)
    }

    fn plan() -> GatewayPlan {
        GatewayPlan {
            console_url: "https://gw.example.com".to_string(),
            llm_url: "https://llm.example.com".to_string(),
            project: OpenProject {
                id: "p-1".to_string(),
                slug: "alpha".to_string(),
                name: "Alpha".to_string(),
            },
        }
    }

    fn only(findings: Vec<Finding>) -> Finding {
        assert_eq!(findings.len(), 1, "{findings:?}");
        findings.into_iter().next().unwrap()
    }

    #[test]
    fn the_default_is_hooks_and_says_how_to_choose_the_gateway() {
        let mode = effective_mode(None, Some(TrafficPolicy::PerAgent));
        let finding = only(check(
            &mode,
            &fresh(TrafficPolicy::PerAgent),
            None,
            Agent::ClaudeCode,
        ));
        assert_eq!(
            (finding.id, finding.severity),
            ("gateway.mode", Severity::Ok)
        );
        assert!(finding.message.contains("--mode gateway"), "{finding:?}");
    }

    #[test]
    fn a_choice_the_policy_allows_is_reported_as_the_developers() {
        let mode = effective_mode(Some(TracingMode::Gateway), Some(TrafficPolicy::PerAgent));
        let findings = check(
            &mode,
            &fresh(TrafficPolicy::PerAgent),
            Some(&Ok(plan())),
            Agent::ClaudeCode,
        );
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(findings[0].message.contains("your choice"));
        assert_eq!(findings[1].id, "gateway.project");
        assert_eq!(findings[1].severity, Severity::Ok);
        assert!(findings[1].message.contains("`alpha`"), "{findings:?}");
    }

    #[test]
    fn an_overruled_choice_needs_attention() {
        let mode = effective_mode(Some(TracingMode::Hooks), Some(TrafficPolicy::Enforced));
        let findings = check(
            &mode,
            &fresh(TrafficPolicy::Enforced),
            Some(&Ok(plan())),
            Agent::ClaudeCode,
        );
        assert_eq!(findings[0].severity, Severity::Warn);
        assert!(findings[0].message.contains("`enforced`"), "{findings:?}");
        assert!(findings[0].message.contains("go through the AI Gateway"));

        let mode = effective_mode(Some(TracingMode::Gateway), Some(TrafficPolicy::Direct));
        let finding = only(check(
            &mode,
            &fresh(TrafficPolicy::Direct),
            None,
            Agent::Codex,
        ));
        assert_eq!(finding.severity, Severity::Warn);
        assert!(finding.message.contains("captured by hooks"), "{finding:?}");
    }

    #[test]
    fn a_policy_that_agrees_with_the_choice_is_fine() {
        let mode = effective_mode(None, Some(TrafficPolicy::Enforced));
        let findings = check(
            &mode,
            &fresh(TrafficPolicy::Enforced),
            Some(&Ok(plan())),
            Agent::ClaudeCode,
        );
        assert_eq!(findings[0].severity, Severity::Ok);
        assert!(findings[0].message.contains("required by the workspace"));
    }

    #[test]
    fn an_unreadable_policy_is_information_only() {
        let mode = effective_mode(None, None);
        let unknown = PolicyState::Unknown("this installation names no Control Plane".to_string());
        let findings = check(&mode, &unknown, None, Agent::ClaudeCode);
        assert_eq!(findings[1].id, "gateway.policy");
        assert_eq!(findings[1].severity, Severity::Info);

        let remembered = PolicyState::Remembered(TrafficPolicy::Enforced, "HTTP 503".to_string());
        let mode = effective_mode(None, remembered.policy());
        let findings = check(&mode, &remembered, Some(&Ok(plan())), Agent::ClaudeCode);
        assert_eq!(findings[1].severity, Severity::Info);
        assert!(findings[1].message.contains("HTTP 503"), "{findings:?}");
        assert!(findings[1].message.contains("`enforced`"), "{findings:?}");
    }

    #[test]
    fn a_gateway_that_is_not_ready_says_what_happens_next() {
        let failed: Result<GatewayPlan, String> = Err("no AI Gateway project is open".to_string());
        let chosen = effective_mode(Some(TracingMode::Gateway), Some(TrafficPolicy::PerAgent));
        let findings = check(
            &chosen,
            &fresh(TrafficPolicy::PerAgent),
            Some(&failed),
            Agent::ClaudeCode,
        );
        assert_eq!(findings[1].severity, Severity::Error);
        assert!(findings[1].message.contains("hooks tracing instead"));

        let enforced = effective_mode(None, Some(TrafficPolicy::Enforced));
        let findings = check(
            &enforced,
            &fresh(TrafficPolicy::Enforced),
            Some(&failed),
            Agent::ClaudeCode,
        );
        assert!(findings[1].message.contains("will not be started"));
    }

    #[test]
    fn who_pays_is_reported_with_its_source() {
        use crate::billing::BillingSource;
        let project = ResolvedBilling {
            billing: Billing::Project,
            source: BillingSource::Default,
        };
        let finding = billing_finding(&project, &BillingUse::Applies).unwrap();
        assert_eq!(
            (finding.id, finding.severity),
            ("gateway.billing", Severity::Ok)
        );
        assert!(
            finding.message.contains("project (the default)"),
            "{finding:?}"
        );
        assert!(billing_finding(&project, &BillingUse::Unused).is_none());

        let sub = ResolvedBilling {
            billing: Billing::Subscription,
            source: BillingSource::Flag,
        };
        let finding = billing_finding(&sub, &BillingUse::Applies).unwrap();
        assert_eq!(finding.severity, Severity::Warn);
        assert!(finding.message.contains("not available"), "{finding:?}");
        let finding = billing_finding(&sub, &BillingUse::Unused).unwrap();
        assert_eq!(finding.severity, Severity::Info);
        let finding = billing_finding(&sub, &BillingUse::Rejected("why".to_string())).unwrap();
        assert_eq!(
            (finding.severity, finding.message.as_str()),
            (Severity::Error, "why")
        );
    }

    #[test]
    fn codex_is_told_its_key_does_not_renew() {
        let mode = effective_mode(Some(TracingMode::Gateway), None);
        let unknown = PolicyState::Fresh(TrafficPolicy::PerAgent);
        let findings = check(&mode, &unknown, Some(&Ok(plan())), Agent::Codex);
        let note = findings
            .iter()
            .find(|f| f.id == "gateway.codex_key")
            .unwrap();
        assert_eq!(note.severity, Severity::Info);
        let findings = check(&mode, &unknown, Some(&Ok(plan())), Agent::ClaudeCode);
        assert!(!findings.iter().any(|f| f.id == "gateway.codex_key"));
    }
}
