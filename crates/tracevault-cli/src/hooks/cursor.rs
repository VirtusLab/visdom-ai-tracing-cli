use std::path::Path;
use tracevault_protocol::streaming::StreamEventRequest;

use super::HookAdapter;

pub struct CursorAdapter;

impl HookAdapter for CursorAdapter {
    fn tool_name(&self) -> &str {
        "cursor"
    }

    fn parse_event(&self, _raw: &str) -> Result<StreamEventRequest, String> {
        Err("Cursor hook adapter not yet implemented".to_string())
    }

    fn parse_transcript(&self, _path: &Path) -> Result<Vec<serde_json::Value>, String> {
        Err("Cursor transcript parsing not yet implemented".to_string())
    }
}
