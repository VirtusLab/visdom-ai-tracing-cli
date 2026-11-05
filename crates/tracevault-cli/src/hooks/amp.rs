use std::path::Path;
use tracevault_protocol::streaming::StreamEventRequest;

use super::HookAdapter;

/// Detection-only stub for the (currently dormant) `HookAdapter` trait, like
/// `OpenCodeAdapter`: Amp capture runs through the bundled plugin
/// (`assets/amp-plugin/index.ts`) and `commands::stream::stamp_agent`.
pub struct AmpAdapter;

impl HookAdapter for AmpAdapter {
    fn tool_name(&self) -> &str {
        "amp"
    }

    fn parse_event(&self, _raw: &str) -> Result<StreamEventRequest, String> {
        Err("Amp capture uses the bundled plugin + stamp_agent".to_string())
    }

    fn parse_transcript(&self, _path: &Path) -> Result<Vec<serde_json::Value>, String> {
        Err("Amp capture uses the bundled plugin + stamp_agent".to_string())
    }
}
