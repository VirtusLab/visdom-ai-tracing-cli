//! Deciding which fixes to apply, and applying them.

use crate::finding::{Finding, Fix, FixAction};

/// Whether offered fixes are applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixPolicy {
    /// Nothing is changed and nobody is asked.
    Never,
    /// Each fix is shown and applied on a yes.
    Ask,
    /// Every fix is applied.
    All,
}

/// The policy for one run. `doctor` changes nothing unless `--fix` is given;
/// `--yes` applies everything; without a terminal on stdin there is nobody to
/// ask, so nothing is applied.
pub fn policy_for(fix_flag: bool, yes: bool, is_doctor: bool, stdin_is_tty: bool) -> FixPolicy {
    if is_doctor && !fix_flag {
        FixPolicy::Never
    } else if yes {
        FixPolicy::All
    } else if !stdin_is_tty {
        FixPolicy::Never
    } else {
        FixPolicy::Ask
    }
}

/// Carries out a [`FixAction`].
pub trait FixExecutor {
    fn execute(&mut self, action: &FixAction) -> Result<(), String>;
}

/// What happened to one finding's fix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixOutcome {
    pub id: &'static str,
    pub applied: bool,
    pub error: Option<String>,
}

/// Offer every finding's fix under `policy`. An action identical to one
/// already handled in this run (several findings can share one fix) is neither
/// asked about nor executed again: the later finding gets the same outcome.
pub fn apply_fixes(
    findings: &[Finding],
    policy: FixPolicy,
    ask: &mut dyn FnMut(&Fix) -> bool,
    exec: &mut dyn FixExecutor,
) -> Vec<FixOutcome> {
    let mut outcomes = Vec::new();
    if policy == FixPolicy::Never {
        return outcomes;
    }
    // Every distinct action handled so far, with how it went.
    let mut handled: Vec<(&FixAction, bool, Option<String>)> = Vec::new();
    for finding in findings {
        let Some(fix) = &finding.fix else { continue };
        let (applied, error) = match handled.iter().find(|(action, ..)| *action == &fix.action) {
            Some((_, applied, error)) => (*applied, error.clone()),
            None => {
                let result = if policy == FixPolicy::Ask && !ask(fix) {
                    (false, None)
                } else {
                    match exec.execute(&fix.action) {
                        Ok(()) => (true, None),
                        Err(e) => (false, Some(e)),
                    }
                };
                handled.push((&fix.action, result.0, result.1.clone()));
                result
            }
        };
        outcomes.push(FixOutcome {
            id: finding.id,
            applied,
            error,
        });
    }
    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::Severity;

    #[derive(Default)]
    struct Recorder {
        calls: Vec<FixAction>,
        fail: bool,
    }

    impl FixExecutor for Recorder {
        fn execute(&mut self, action: &FixAction) -> Result<(), String> {
            self.calls.push(action.clone());
            if self.fail {
                Err("boom".to_string())
            } else {
                Ok(())
            }
        }
    }

    fn action(name: &str) -> FixAction {
        FixAction::RunCommand {
            program: name.to_string(),
            args: vec![],
            cwd: None,
        }
    }

    fn finding(id: &'static str, action: FixAction) -> Finding {
        Finding::new("tracing", id, Severity::Error, "broken").with_fix(Fix {
            description: "do it".to_string(),
            action,
        })
    }

    #[test]
    fn policy_table() {
        use FixPolicy::*;
        for fix in [false, true] {
            for yes in [false, true] {
                for doctor in [false, true] {
                    for tty in [false, true] {
                        let expected = if doctor && !fix {
                            Never
                        } else if yes {
                            All
                        } else if tty {
                            Ask
                        } else {
                            Never
                        };
                        assert_eq!(
                            policy_for(fix, yes, doctor, tty),
                            expected,
                            "fix={fix} yes={yes} doctor={doctor} tty={tty}"
                        );
                    }
                }
            }
        }
        // The cases that matter most, spelled out.
        assert_eq!(policy_for(false, true, true, true), Never); // doctor --yes
        assert_eq!(policy_for(true, false, true, false), Never); // doctor --fix, piped
        assert_eq!(policy_for(false, false, false, true), Ask); // visdom, terminal
        assert_eq!(policy_for(false, true, false, false), All); // visdom --yes, piped
    }

    #[test]
    fn same_action_applied_once() {
        let findings = [
            finding("a", action("init")),
            finding("b", action("init")),
            finding("c", action("other")),
        ];
        let mut exec = Recorder::default();
        let mut asked = 0;
        let outcomes = apply_fixes(
            &findings,
            FixPolicy::Ask,
            &mut |_| {
                asked += 1;
                true
            },
            &mut exec,
        );
        assert_eq!(exec.calls, [action("init"), action("other")]);
        assert_eq!(asked, 2);
        assert!(outcomes.iter().all(|o| o.applied && o.error.is_none()));
        assert_eq!(outcomes.len(), 3);
    }

    #[test]
    fn non_tty_without_yes_never_applies() {
        let policy = policy_for(false, false, false, false);
        assert_eq!(policy, FixPolicy::Never);
        let mut exec = Recorder::default();
        let outcomes = apply_fixes(
            &[finding("a", action("init"))],
            policy,
            &mut |_| panic!("nobody to ask"),
            &mut exec,
        );
        assert!(exec.calls.is_empty());
        assert!(outcomes.is_empty());
    }

    #[test]
    fn ask_declined_is_not_applied() {
        let findings = [finding("a", action("init")), finding("b", action("init"))];
        let mut exec = Recorder::default();
        let mut asked = 0;
        let outcomes = apply_fixes(
            &findings,
            FixPolicy::Ask,
            &mut |_| {
                asked += 1;
                false
            },
            &mut exec,
        );
        assert!(exec.calls.is_empty());
        assert_eq!(asked, 1, "a declined fix is not offered twice");
        assert!(outcomes.iter().all(|o| !o.applied && o.error.is_none()));
    }

    #[test]
    fn all_applies_without_asking() {
        let mut exec = Recorder::default();
        let outcomes = apply_fixes(
            &[finding("a", action("init"))],
            FixPolicy::All,
            &mut |_| panic!("--yes must not ask"),
            &mut exec,
        );
        assert_eq!(exec.calls.len(), 1);
        assert!(outcomes[0].applied);
    }

    #[test]
    fn executor_error_is_reported_not_panicked() {
        let mut exec = Recorder {
            fail: true,
            ..Default::default()
        };
        let outcomes = apply_fixes(
            &[finding("a", action("init")), finding("b", action("init"))],
            FixPolicy::All,
            &mut |_| true,
            &mut exec,
        );
        assert_eq!(exec.calls.len(), 1, "a failed fix is not retried");
        for outcome in &outcomes {
            assert!(!outcome.applied);
            assert_eq!(outcome.error.as_deref(), Some("boom"));
        }
    }

    #[test]
    fn findings_without_a_fix_are_skipped() {
        let mut exec = Recorder::default();
        let outcomes = apply_fixes(
            &[Finding::new("tracing", "a", Severity::Warn, "m")],
            FixPolicy::All,
            &mut |_| true,
            &mut exec,
        );
        assert!(outcomes.is_empty() && exec.calls.is_empty());
    }
}
