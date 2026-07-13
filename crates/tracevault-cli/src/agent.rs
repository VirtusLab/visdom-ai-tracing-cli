//! The AI coding agent a hook invocation belongs to. Carries the two
//! agent-specific values stamped on an outgoing `StreamEventRequest`:
//! the `tool` name (which the server keys its adapter off) and the
//! `protocol_version`. Everything else in the capture path is agent-agnostic.

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    ClaudeCode,
    Codex,
}

impl Agent {
    /// The `tool` value stamped on the stream request. The server selects its
    /// per-agent adapter from this string.
    pub fn tool_name(&self) -> &'static str {
        match self {
            Agent::ClaudeCode => "claude-code",
            Agent::Codex => "codex",
        }
    }

    /// Wire protocol version. Claude Code stays on v1 (behaviour-preserving);
    /// Codex uses v2, where the server honours the explicit `tool`.
    pub fn protocol_version(&self) -> u8 {
        match self {
            Agent::ClaudeCode => 1,
            Agent::Codex => 2,
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
}
