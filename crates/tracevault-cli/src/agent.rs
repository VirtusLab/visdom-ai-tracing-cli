//! The AI coding agent a hook invocation belongs to. Carries the two
//! agent-specific values stamped on an outgoing `StreamEventRequest`:
//! the `tool` name (which the server keys its adapter off) and the
//! `protocol_version`. Everything else in the capture path is agent-agnostic.

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    ClaudeCode,
    Codex,
    /// GSD (Get Shit Done) — runs on the pi coding-agent framework.
    #[value(alias = "gsd2")]
    Gsd,
}

impl Agent {
    /// The `tool` value stamped on the stream request. The server selects its
    /// per-agent adapter from this string.
    pub fn tool_name(&self) -> &'static str {
        match self {
            Agent::ClaudeCode => "claude-code",
            Agent::Codex => "codex",
            Agent::Gsd => "gsd",
        }
    }

    /// Wire protocol version. Claude Code stays on v1 (behaviour-preserving);
    /// Codex uses v2, where the server honours the explicit `tool`.
    pub fn protocol_version(&self) -> u8 {
        match self {
            Agent::ClaudeCode => 1,
            Agent::Codex => 2,
            Agent::Gsd => 2,
        }
    }

    /// Human-readable label for CLI output. Preserves the original wording so
    /// the default Claude Code path prints identically to before.
    pub fn label(&self) -> &'static str {
        match self {
            Agent::ClaudeCode => "Claude Code",
            Agent::Codex => "Codex",
            Agent::Gsd => "GSD",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    #[test]
    fn tool_name_and_protocol_version() {
        assert_eq!(Agent::ClaudeCode.tool_name(), "claude-code");
        assert_eq!(Agent::ClaudeCode.protocol_version(), 1);
        assert_eq!(Agent::Codex.tool_name(), "codex");
        assert_eq!(Agent::Codex.protocol_version(), 2);
    }

    #[test]
    fn value_enum_parses_kebab_case() {
        assert_eq!(
            Agent::from_str("claude-code", true).unwrap(),
            Agent::ClaudeCode
        );
        assert_eq!(Agent::from_str("codex", true).unwrap(), Agent::Codex);
        assert!(Agent::from_str("cursor", true).is_err());
    }

    #[test]
    fn label_wording() {
        assert_eq!(Agent::ClaudeCode.label(), "Claude Code");
        assert_eq!(Agent::Codex.label(), "Codex");
    }

    #[test]
    fn gsd_agent_mapping() {
        assert_eq!(Agent::Gsd.tool_name(), "gsd");
        assert_eq!(Agent::Gsd.protocol_version(), 2);
        assert_eq!(Agent::Gsd.label(), "GSD");
    }

    #[test]
    fn gsd_parses_from_str_and_alias() {
        use clap::ValueEnum;
        assert_eq!(Agent::from_str("gsd", true).unwrap(), Agent::Gsd);
        assert_eq!(Agent::from_str("gsd2", true).unwrap(), Agent::Gsd);
    }
}
