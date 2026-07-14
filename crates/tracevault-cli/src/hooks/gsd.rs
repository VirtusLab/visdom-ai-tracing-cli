use std::path::Path;
use tracevault_protocol::streaming::StreamEventRequest;

use super::HookAdapter;

/// Placeholder GSD (pi) adapter for the (currently dormant) `HookAdapter` trait.
///
/// GSD capture does NOT go through this trait — the live path is the
/// embedded pi/GSD extension (installed by `commands::init::install_gsd_extension`
/// under the user-global `~/.gsd/extensions/tracevault/`), which shells out to
/// `tracevault stream --agent gsd` itself. This type exists so
/// `DetectedTool::Gsd::adapter()` reports the correct `tool_name` ("gsd")
/// rather than borrowing another adapter's label, mirroring the `CodexAdapter`
/// and `CursorAdapter` stubs. The parse methods are intentionally
/// unimplemented here.
pub struct GsdAdapter;

impl HookAdapter for GsdAdapter {
    fn tool_name(&self) -> &str {
        "gsd"
    }

    fn parse_event(&self, _raw: &str) -> Result<StreamEventRequest, String> {
        Err("GSD hook adapter not implemented (capture uses the pi/GSD extension)".to_string())
    }

    fn parse_transcript(&self, _path: &Path) -> Result<Vec<serde_json::Value>, String> {
        Err(
            "GSD transcript parsing not implemented (capture uses the pi/GSD extension)"
                .to_string(),
        )
    }
}
