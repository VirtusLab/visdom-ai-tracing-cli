//! A Codex rollout JSONL flows through the incremental reader and the agent
//! stamping to produce a request the server's CodexAdapter recognizes:
//! tool="codex", protocol_version=2, transcript lines carried.

use std::io::Write;

use tracevault_cli::agent::Agent;
use tracevault_cli::commands::stream::{read_new_transcript_lines, stamp_agent};
use tracevault_protocol::streaming::{StreamEventRequest, StreamEventType};

#[test]
fn codex_rollout_produces_codex_tagged_request() {
    let tmp = tempfile::tempdir().unwrap();
    let transcript = tmp.path().join("rollout.jsonl");
    let offset = tmp.path().join(".stream_offset");

    // A representative slice of a Codex rollout: a user message, an apply_patch
    // custom_tool_call (file change), and a token_count event.
    let mut f = std::fs::File::create(&transcript).unwrap();
    writeln!(f, r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"fix it"}}]}}}}"#).unwrap();
    writeln!(f, r#"{{"type":"response_item","timestamp":"2026-04-29T11:30:00Z","payload":{{"type":"custom_tool_call","name":"apply_patch","input":"*** Begin Patch\n*** Update File: src/main.rs\n@@\n-old\n+new\n*** End Patch\n"}}}}"#).unwrap();
    writeln!(f, r#"{{"type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":100,"output_tokens":10}}}}}}}}"#).unwrap();
    drop(f);

    let (lines, new_offset) = read_new_transcript_lines(&transcript, &offset).unwrap();
    assert_eq!(lines.len(), 3, "all three rollout lines read");
    assert!(new_offset > 0);

    let mut req = StreamEventRequest {
        protocol_version: 1,
        tool: Some("claude-code".to_string()),
        event_type: StreamEventType::ToolUse,
        session_id: "sess-codex".into(),
        timestamp: chrono::Utc::now(),
        hook_event_name: Some("PostToolUse".into()),
        tool_name: None,
        tool_use_id: None,
        tool_input: None,
        tool_response: None,
        tool_is_error: None,
        event_index: None,
        event_uuid: None,
        transcript_lines: Some(lines),
        transcript_offset: Some(new_offset),
        model: None,
        cwd: Some(tmp.path().to_string_lossy().into_owned()),
        final_stats: None,
        flow_id: None,
        labels: None,
        params: None,
    };
    stamp_agent(&mut req, Agent::Codex);

    assert_eq!(req.tool.as_deref(), Some("codex"));
    assert_eq!(req.protocol_version, 2);
    assert_eq!(req.transcript_lines.as_ref().unwrap().len(), 3);
}
