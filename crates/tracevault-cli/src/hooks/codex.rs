use std::path::Path;
use tracevault_protocol::streaming::StreamEventRequest;

use super::HookAdapter;

/// Placeholder Codex adapter for the (currently dormant) `HookAdapter` trait.
///
/// Codex capture does NOT go through this trait — the live path stamps the
/// agent via `commands::stream::stamp_agent` (tool="codex", protocol v2) and
/// reads the rollout with the shared `read_new_transcript_lines`. This type
/// exists so `DetectedTool::Codex::adapter()` reports the correct `tool_name`
/// ("codex") rather than borrowing the Claude adapter's "claude-code" label,
/// mirroring the `CursorAdapter` stub. The parse methods are intentionally
/// unimplemented here.
pub struct CodexAdapter;

impl HookAdapter for CodexAdapter {
    fn tool_name(&self) -> &str {
        "codex"
    }

    fn parse_event(&self, _raw: &str) -> Result<StreamEventRequest, String> {
        Err("Codex hook adapter not implemented (capture uses stamp_agent)".to_string())
    }

    fn parse_transcript(&self, _path: &Path) -> Result<Vec<serde_json::Value>, String> {
        Err("Codex transcript parsing not implemented (capture uses stamp_agent)".to_string())
    }
}
