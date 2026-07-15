use std::path::Path;
use tracevault_protocol::streaming::StreamEventRequest;

use super::HookAdapter;

/// Detection-only stub for the (currently dormant) `HookAdapter` trait.
///
/// OpenCode capture does NOT go through this trait — the live path stamps
/// the agent via the bundled plugin + `commands::stream::stamp_agent`. This
/// type exists so `DetectedTool::OpenCode::adapter()` reports the correct
/// `tool_name` ("opencode"), mirroring the `CodexAdapter`/`GsdAdapter`
/// stubs. The parse methods are intentionally unimplemented here.
pub struct OpenCodeAdapter;

impl HookAdapter for OpenCodeAdapter {
    fn tool_name(&self) -> &str {
        "opencode"
    }

    fn parse_event(&self, _raw: &str) -> Result<StreamEventRequest, String> {
        Err("OpenCode capture uses the bundled plugin + stamp_agent".to_string())
    }

    fn parse_transcript(&self, _path: &Path) -> Result<Vec<serde_json::Value>, String> {
        Err("OpenCode capture uses the bundled plugin + stamp_agent".to_string())
    }
}
