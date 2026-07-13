use std::path::Path;
use tracevault_protocol::streaming::StreamEventRequest;

// Adapter modules contain stubs for future stream command integration.
#[allow(dead_code)]
pub mod claude_code;
#[allow(dead_code)]
pub mod cursor;

#[allow(dead_code)]
pub trait HookAdapter: Send + Sync {
    fn tool_name(&self) -> &str;
    fn parse_event(&self, raw: &str) -> Result<StreamEventRequest, String>;
    fn parse_transcript(&self, path: &Path) -> Result<Vec<serde_json::Value>, String>;
}

#[derive(Debug, Clone, Copy)]
pub enum DetectedTool {
    ClaudeCode,
    Cursor,
    Codex,
}

impl DetectedTool {
    pub fn name(&self) -> &str {
        match self {
            DetectedTool::ClaudeCode => "claude-code",
            DetectedTool::Cursor => "cursor",
            DetectedTool::Codex => "codex",
        }
    }

    #[allow(dead_code)]
    pub fn adapter(&self) -> Box<dyn HookAdapter> {
        match self {
            DetectedTool::ClaudeCode => Box::new(claude_code::ClaudeCodeAdapter),
            DetectedTool::Cursor => Box::new(cursor::CursorAdapter),
            DetectedTool::Codex => Box::new(claude_code::ClaudeCodeAdapter),
        }
    }
}

pub fn detect_tools(cwd: &Path) -> Vec<DetectedTool> {
    let mut tools = vec![];
    if cwd.join(".claude").exists() {
        tools.push(DetectedTool::ClaudeCode);
    }
    if cwd.join(".cursor").exists() {
        tools.push(DetectedTool::Cursor);
    }
    if cwd.join(".codex").exists() {
        tools.push(DetectedTool::Codex);
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn detect_tools_claude_only() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".claude")).unwrap();
        let tools = detect_tools(dir.path());
        assert_eq!(tools.len(), 1);
        assert!(matches!(tools[0], DetectedTool::ClaudeCode));
    }

    #[test]
    fn detect_tools_neither() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_tools(dir.path()).is_empty());
    }

    #[test]
    fn detect_tools_both() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".claude")).unwrap();
        fs::create_dir(dir.path().join(".cursor")).unwrap();
        assert_eq!(detect_tools(dir.path()).len(), 2);
    }

    #[test]
    fn detect_tools_codex() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".codex")).unwrap();
        let tools = detect_tools(dir.path());
        assert!(tools.iter().any(|t| matches!(t, DetectedTool::Codex)));
    }
}
