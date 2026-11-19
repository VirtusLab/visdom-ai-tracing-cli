//! What a check found, and the change that would fix it.

use std::path::PathBuf;

use serde::{Serialize, Serializer};
use tracevault_cli::agent::Agent;

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Ok,
    /// Worth knowing, nothing to do.
    Info,
    Warn,
    /// The component does not work for this project as it is.
    Error,
    /// Could not be verified (unreadable file, unreachable service).
    Unknown,
}

#[derive(Serialize, Clone, Debug)]
pub struct Finding {
    pub component: &'static str,
    pub id: &'static str,
    pub severity: Severity,
    pub message: String,
    pub fix: Option<Fix>,
}

impl Finding {
    pub fn new(
        component: &'static str,
        id: &'static str,
        severity: Severity,
        message: impl Into<String>,
    ) -> Self {
        Finding {
            component,
            id,
            severity,
            message: message.into(),
            fix: None,
        }
    }

    pub fn with_fix(mut self, fix: Fix) -> Self {
        self.fix = Some(fix);
        self
    }
}

/// A change `visdom` can make. `description` is what the user is shown before
/// agreeing to it: the command that will run and where.
#[derive(Serialize, Clone, Debug)]
pub struct Fix {
    pub description: String,
    pub action: FixAction,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FixAction {
    /// `visdom trace init --agent <agent>` in `dir`.
    TracingInit {
        #[serde(serialize_with = "ser_agent")]
        agent: Agent,
        dir: PathBuf,
    },
    /// `visdom login --server-url <server_url>`.
    Login { server_url: String },
    /// Any other command, e.g. the agent's own `mcp add`.
    RunCommand {
        program: String,
        args: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
    },
    /// Steps run in order; the first failure stops the rest.
    Sequence { steps: Vec<FixAction> },
}

fn ser_agent<S: Serializer>(agent: &Agent, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(agent.tool_name())
}

/// The command line of a `RunCommand`, for display.
pub fn command_line(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(|word| {
            if word.is_empty() || word.contains(char::is_whitespace) {
                format!("'{word}'")
            } else {
                word.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn marker(severity: Severity) -> char {
    match severity {
        Severity::Ok => '✓',
        Severity::Info => '-',
        Severity::Warn => '!',
        Severity::Error => '✗',
        Severity::Unknown => '?',
    }
}

fn title(component: &str) -> String {
    let mut chars = component.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The findings as text, grouped by component in first-seen order.
pub fn render_text(findings: &[Finding]) -> String {
    let mut components: Vec<&str> = Vec::new();
    for finding in findings {
        if !components.contains(&finding.component) {
            components.push(finding.component);
        }
    }
    let mut out = String::new();
    for component in components {
        out.push_str(&title(component));
        out.push('\n');
        for finding in findings.iter().filter(|f| f.component == component) {
            out.push_str(&format!(
                "  {} {}\n",
                marker(finding.severity),
                finding.message
            ));
            if let Some(fix) = &finding.fix {
                out.push_str(&format!("      fix: {}\n", fix.description));
            }
        }
    }
    out
}

/// `{"findings":[…]}`, pretty-printed.
pub fn render_json(findings: &[Finding]) -> String {
    serde_json::to_string_pretty(&serde_json::json!({ "findings": findings }))
        .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}

/// 1 when any finding is an error, else 0.
pub fn exit_code(findings: &[Finding]) -> i32 {
    i32::from(findings.iter().any(|f| f.severity == Severity::Error))
}

/// The findings a developer starting an agent should see: everything that is
/// neither fine nor merely informational.
pub fn needs_attention(findings: &[Finding]) -> Vec<Finding> {
    findings
        .iter()
        .filter(|f| !matches!(f.severity, Severity::Ok | Severity::Info))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_fix() -> Fix {
        Fix {
            description: "run `claude mcp add …`".to_string(),
            action: FixAction::RunCommand {
                program: "claude".to_string(),
                args: vec!["mcp".to_string(), "add".to_string()],
                cwd: None,
            },
        }
    }

    #[test]
    fn json_shape_is_stable() {
        let findings = [
            Finding::new("bazaar", "bazaar.mcp", Severity::Error, "not registered")
                .with_fix(run_fix()),
            Finding::new("tracing", "tracing.repo", Severity::Error, "no config").with_fix(Fix {
                description: "init".to_string(),
                action: FixAction::TracingInit {
                    agent: Agent::ClaudeCode,
                    dir: PathBuf::from("/repo"),
                },
            }),
        ];
        let v: serde_json::Value = serde_json::from_str(&render_json(&findings)).unwrap();
        let first = &v["findings"][0];
        let keys: Vec<&str> = first
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["component", "id", "severity", "message", "fix"]);
        assert_eq!(first["severity"], "error");
        assert_eq!(first["fix"]["action"]["kind"], "run_command");
        assert_eq!(first["fix"]["action"]["program"], "claude");
        assert!(first["fix"]["action"].get("cwd").is_none());
        let init = &v["findings"][1]["fix"]["action"];
        assert_eq!(init["kind"], "tracing_init");
        assert_eq!(init["agent"], "claude-code");
    }

    #[test]
    fn exit_code_only_on_error() {
        let mk = |s| Finding::new("tracing", "x", s, "m");
        assert_eq!(exit_code(&[]), 0);
        assert_eq!(
            exit_code(&[
                mk(Severity::Ok),
                mk(Severity::Info),
                mk(Severity::Warn),
                mk(Severity::Unknown)
            ]),
            0
        );
        assert_eq!(exit_code(&[mk(Severity::Ok), mk(Severity::Error)]), 1);
    }

    #[test]
    fn text_groups_by_component_and_shows_fixes() {
        let text = render_text(&[
            Finding::new("tracing", "tracing.auth", Severity::Ok, "logged in"),
            Finding::new("bazaar", "bazaar.mcp", Severity::Error, "not registered")
                .with_fix(run_fix()),
            Finding::new(
                "tracing",
                "tracing.git_hooks",
                Severity::Warn,
                "no git hooks",
            ),
        ]);
        assert_eq!(
            text,
            "Tracing\n  ✓ logged in\n  ! no git hooks\nBazaar\n  ✗ not registered\n      fix: run \
             `claude mcp add …`\n"
        );
    }

    #[test]
    fn needs_attention_drops_ok_and_info() {
        let mk = |s| Finding::new("tracing", "x", s, "m");
        let kept = needs_attention(&[
            mk(Severity::Ok),
            mk(Severity::Info),
            mk(Severity::Warn),
            mk(Severity::Error),
            mk(Severity::Unknown),
        ]);
        let severities: Vec<Severity> = kept.iter().map(|f| f.severity).collect();
        assert_eq!(
            severities,
            [Severity::Warn, Severity::Error, Severity::Unknown]
        );
    }

    #[test]
    fn command_line_quotes_words_with_spaces() {
        assert_eq!(
            command_line("claude", &["mcp".to_string(), "my server".to_string()]),
            "claude mcp 'my server'"
        );
    }
}
