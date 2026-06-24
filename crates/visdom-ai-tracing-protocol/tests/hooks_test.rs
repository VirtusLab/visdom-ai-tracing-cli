use tracevault_protocol::hooks::parse_hook_event;

#[test]
fn parses_pre_tool_use_write() {
    let json = r#"{
        "session_id": "abc123",
        "transcript_path": "/home/user/.claude/projects/test/abc123.jsonl",
        "cwd": "/home/user/project",
        "hook_event_name": "PreToolUse",
        "tool_name": "Write",
        "tool_input": {
            "file_path": "/home/user/project/src/main.rs",
            "content": "fn main() {}"
        }
    }"#;

    let event = parse_hook_event(json).unwrap();
    assert_eq!(event.session_id, "abc123");
    assert_eq!(event.hook_event_name, "PreToolUse");
    assert_eq!(event.tool_name.as_deref(), Some("Write"));
}

#[test]
fn parses_post_tool_use_edit() {
    let json = r#"{
        "session_id": "abc123",
        "transcript_path": "/home/user/.claude/projects/test/abc123.jsonl",
        "cwd": "/home/user/project",
        "hook_event_name": "PostToolUse",
        "tool_name": "Edit",
        "tool_input": {
            "file_path": "/home/user/project/src/lib.rs",
            "old_string": "old",
            "new_string": "new"
        },
        "tool_response": {
            "success": true
        }
    }"#;

    let event = parse_hook_event(json).unwrap();
    assert_eq!(event.hook_event_name, "PostToolUse");
    assert_eq!(event.tool_name.as_deref(), Some("Edit"));
    assert!(event.tool_input.is_some());
}
