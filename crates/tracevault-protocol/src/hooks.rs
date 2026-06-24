use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEvent {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    #[serde(default)]
    pub permission_mode: Option<String>,
    pub hook_event_name: String,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    #[serde(default)]
    pub tool_response: Option<serde_json::Value>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#continue: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suppress_output: Option<bool>,
}

impl HookResponse {
    pub fn allow() -> Self {
        Self {
            r#continue: None,
            suppress_output: Some(true),
        }
    }
}

#[derive(Debug, Error)]
pub enum HookError {
    #[error("Failed to parse hook event: {0}")]
    ParseError(#[from] serde_json::Error),
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

pub fn parse_hook_event(json: &str) -> Result<HookEvent, HookError> {
    Ok(serde_json::from_str(json)?)
}

impl HookEvent {
    /// Extract file path from tool_input if this is a Write or Edit event
    pub fn file_path(&self) -> Option<String> {
        self.tool_input
            .as_ref()?
            .get("file_path")?
            .as_str()
            .map(String::from)
    }

    pub fn is_file_modification(&self) -> bool {
        matches!(self.tool_name.as_deref(), Some("Write") | Some("Edit"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(tool_name: Option<&str>) -> HookEvent {
        HookEvent {
            session_id: "s".into(),
            transcript_path: "t".into(),
            cwd: ".".into(),
            permission_mode: None,
            hook_event_name: "PostToolUse".into(),
            tool_name: tool_name.map(String::from),
            tool_input: None,
            tool_response: None,
            tool_use_id: None,
        }
    }

    #[test]
    fn is_file_modification_write() {
        assert!(make_event(Some("Write")).is_file_modification());
    }

    #[test]
    fn is_file_modification_edit() {
        assert!(make_event(Some("Edit")).is_file_modification());
    }

    #[test]
    fn is_file_modification_bash_false() {
        assert!(!make_event(Some("Bash")).is_file_modification());
    }

    #[test]
    fn file_path_returns_none_when_no_input() {
        assert!(make_event(Some("Write")).file_path().is_none());
    }

    #[test]
    fn file_path_returns_none_when_no_file_path_key() {
        let mut e = make_event(Some("Write"));
        e.tool_input = Some(serde_json::json!({"content": "hello"}));
        assert!(e.file_path().is_none());
    }

    #[test]
    fn hook_response_allow_serializes() {
        let resp = HookResponse::allow();
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json.get("suppress_output").unwrap(), true);
        assert!(json.get("continue").is_none());
    }
}
