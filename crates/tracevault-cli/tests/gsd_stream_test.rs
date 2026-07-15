//! A pi (GSD) session JSONL flows through the incremental reader and the agent
//! stamping to produce a request the server's GsdAdapter recognizes:
//! tool="gsd", protocol_version=2, transcript lines carried.

use std::io::Write;

use tracevault_cli::agent::Agent;
use tracevault_cli::commands::stream::{read_new_transcript_lines, stamp_agent};
use tracevault_protocol::streaming::{StreamEventRequest, StreamEventType};

#[test]
fn gsd_session_produces_gsd_tagged_request() {
    let tmp = tempfile::tempdir().unwrap();
    let transcript = tmp.path().join("session.jsonl");
    let offset = tmp.path().join(".stream_offset");

    // A representative slice of a pi (GSD) native session: a model_change event
    // and an assistant message with a toolCall.
    let mut f = std::fs::File::create(&transcript).unwrap();
    writeln!(
        f,
        r#"{{"type":"model_change","provider":"anthropic","modelId":"claude-opus-4-7"}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"type":"message","message":{{"role":"assistant","content":[{{"type":"toolCall","id":"t1","name":"write","arguments":{{"path":"a.rs","content":"x"}}}}],"usage":{{"input":1,"output":2,"cacheRead":0,"cacheWrite":0}}}}}}"#
    )
    .unwrap();
    drop(f);

    let (lines, start_offset, new_offset) =
        read_new_transcript_lines(&transcript, &offset).unwrap();
    assert_eq!(lines.len(), 2, "both pi session lines read");
    assert_eq!(start_offset, 0, "first read starts at byte 0");
    assert!(new_offset > 0);

    let mut req = StreamEventRequest {
        protocol_version: 1,
        tool: Some("claude-code".to_string()),
        event_type: StreamEventType::ToolUse,
        session_id: "sess-gsd".into(),
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
        transcript_offset: Some(start_offset),
        model: None,
        cwd: Some(tmp.path().to_string_lossy().into_owned()),
        final_stats: None,
        flow_id: None,
        labels: None,
        params: None,
    };
    stamp_agent(&mut req, Agent::Gsd);

    assert_eq!(req.tool.as_deref(), Some("gsd"));
    assert_eq!(req.protocol_version, 2);
    assert_eq!(req.transcript_lines.as_ref().unwrap().len(), 2);
}
