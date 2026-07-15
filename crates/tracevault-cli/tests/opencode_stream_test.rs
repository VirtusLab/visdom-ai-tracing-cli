//! OpenCode's plugin sends inline `transcript_records` on the hook event (no
//! single tailable JSONL transcript exists, unlike Claude Code/Codex/GSD). The
//! fileless path in `run_stream` must use those records as `transcript_lines`
//! and treat `.stream_offset` as a per-session RECORD counter, mirroring the
//! byte-offset semantics of `read_new_transcript_lines`.

use tracevault_cli::agent::Agent;
use tracevault_cli::commands::stream::{inline_offset_bump, stamp_agent};
use tracevault_protocol::streaming::{StreamEventRequest, StreamEventType};

#[test]
fn inline_offset_bump_counts_records() {
    // (start_offset, records_len) -> (send_offset, next_offset)
    assert_eq!(inline_offset_bump(0, 2), (0, 2));
    assert_eq!(inline_offset_bump(2, 3), (2, 5));
    assert_eq!(inline_offset_bump(5, 0), (5, 5));
}

#[test]
fn opencode_inline_records_produce_opencode_tagged_request() {
    // A representative slice of inline OpenCode records, as the plugin would
    // send them on `HookEvent.transcript_records` — no transcript file
    // involved at all.
    let records = vec![
        serde_json::json!({"type": "message", "message": {"role": "user", "content": "hi"}}),
        serde_json::json!({"type": "message", "message": {"role": "assistant", "content": "hello"}}),
    ];

    // Simulate the fileless override run_stream performs: no prior
    // `.stream_offset` (start at 0), so `start_offset` == 0 and `new_offset`
    // == records.len().
    let (send_offset, next_offset) = inline_offset_bump(0, records.len() as i64);
    assert_eq!(send_offset, 0);
    assert_eq!(next_offset, 2);

    let mut req = StreamEventRequest {
        protocol_version: 1,
        tool: Some("claude-code".to_string()),
        event_type: StreamEventType::ToolUse,
        session_id: "sess-opencode".into(),
        timestamp: chrono::Utc::now(),
        hook_event_name: Some("PostToolUse".into()),
        tool_name: None,
        tool_use_id: None,
        tool_input: None,
        tool_response: None,
        tool_is_error: None,
        event_index: None,
        event_uuid: None,
        transcript_lines: Some(records.clone()),
        transcript_offset: Some(send_offset),
        model: None,
        cwd: Some("/tmp/opencode-project".to_string()),
        final_stats: None,
        flow_id: None,
        labels: None,
        params: None,
    };
    stamp_agent(&mut req, Agent::OpenCode);

    assert_eq!(req.tool.as_deref(), Some("opencode"));
    assert_eq!(req.protocol_version, 2);
    assert_eq!(req.transcript_lines.as_ref().unwrap().len(), 2);
    assert_eq!(req.transcript_lines.as_ref().unwrap(), &records);
    assert_eq!(req.transcript_offset, Some(0));
}
