use std::path::Path;
use tracevault_protocol::streaming::StreamEventRequest;

use super::HookAdapter;

pub struct ClaudeCodeAdapter;

impl HookAdapter for ClaudeCodeAdapter {
    fn tool_name(&self) -> &str {
        "claude-code"
    }

    fn parse_event(&self, raw: &str) -> Result<StreamEventRequest, String> {
        let mut req: StreamEventRequest =
            serde_json::from_str(raw).map_err(|e| format!("Failed to parse event JSON: {e}"))?;
        req.tool = Some("claude-code".to_string());
        if req.protocol_version == 0 {
            req.protocol_version = 2;
        }
        Ok(req)
    }

    fn parse_transcript(&self, path: &Path) -> Result<Vec<serde_json::Value>, String> {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("Failed to read transcript: {e}"))?;
        let mut lines = Vec::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(line) {
                Ok(v) => lines.push(v),
                Err(_) => continue,
            }
        }
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_event_sets_tool_name() {
        let adapter = ClaudeCodeAdapter;
        let json = serde_json::json!({
            "protocol_version": 2,
            "event_type": "tool_use",
            "session_id": "sess-1",
            "timestamp": "2026-01-01T00:00:00Z"
        });
        let result = adapter
            .parse_event(&serde_json::to_string(&json).unwrap())
            .unwrap();
        assert_eq!(result.tool.as_deref(), Some("claude-code"));
    }

    #[test]
    fn parse_event_upgrades_version_0() {
        let adapter = ClaudeCodeAdapter;
        let json = serde_json::json!({
            "protocol_version": 0,
            "event_type": "tool_use",
            "session_id": "sess-1",
            "timestamp": "2026-01-01T00:00:00Z"
        });
        let result = adapter
            .parse_event(&serde_json::to_string(&json).unwrap())
            .unwrap();
        assert_eq!(result.protocol_version, 2);
    }
}
