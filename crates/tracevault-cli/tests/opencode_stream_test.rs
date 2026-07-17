//! OpenCode's plugin sends inline `transcript_records` on the hook event (no
//! single tailable JSONL transcript exists, unlike Claude Code/Codex/GSD). The
//! fileless path in `run_stream` must use those records as `transcript_lines`
//! and treat `.stream_offset` as a per-session RECORD counter, mirroring the
//! byte-offset semantics of `read_new_transcript_lines`.

use tracevault_cli::agent::Agent;
use tracevault_cli::commands::stream::{
    inline_offset_bump, inline_tool_is_error, resolve_transcript_source, stamp_agent,
};
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

// ── resolve_transcript_source: regression for the empty-inline-records bug ──
//
// The OpenCode plugin's `stop` (session.idle) event sends
// `transcript_records: Some(vec![])` — an empty but present inline-records
// vec. Before the fix, `run_stream` filtered out empty vecs with
// `.filter(|r| !r.is_empty())`, so this case fell through to the file-read
// result `(vec![], 0, 0)` (OpenCode's `transcript_path` is always ""), which
// clobbered `.stream_offset` back to 0 and collided with earlier turns'
// `transcript_offset`/`chunk_index` on the server. `resolve_transcript_source`
// must take the inline branch for ANY `Some(..)`, including empty, so the
// prior record-count offset is preserved instead of reset.

#[test]
fn resolve_transcript_source_empty_inline_preserves_prior_offset() {
    // This is the regression assertion for the bug: an empty `Some(vec![])`
    // must NOT fall through to the (zeroed) file-read result.
    let got = resolve_transcript_source(Some(vec![]), 5, (vec![], 0, 0));
    assert_eq!(got, (vec![], 5, 5));
}

#[test]
fn resolve_transcript_source_nonempty_inline_bumps_offset() {
    let record =
        serde_json::json!({"type": "message", "message": {"role": "user", "content": "hi"}});
    let got = resolve_transcript_source(Some(vec![record.clone()]), 5, (vec![], 0, 0));
    assert_eq!(got, (vec![record], 5, 6));
}

#[test]
fn resolve_transcript_source_none_passes_through_file_result_unchanged() {
    let record =
        serde_json::json!({"type": "message", "message": {"role": "assistant", "content": "x"}});
    let file_result = (vec![record.clone()], 3, 4);
    let got = resolve_transcript_source(None, 5, file_result.clone());
    assert_eq!(got, file_result);
}

#[test]
fn inline_tool_is_error_reads_toolresult_flag() {
    let ok = serde_json::json!({"type": "message", "message": {"role": "toolResult", "isError": false, "content": []}});
    let err = serde_json::json!({"type": "message", "message": {"role": "toolResult", "isError": true, "content": []}});
    let call =
        serde_json::json!({"type": "message", "message": {"role": "assistant", "content": []}});
    // A failed tool result surfaces as Some(true), a successful one as Some(false).
    assert_eq!(inline_tool_is_error(&[call.clone(), err]), Some(true));
    assert_eq!(inline_tool_is_error(&[call.clone(), ok]), Some(false));
    // No toolResult record → None (matches file-agent behavior for non-tool events).
    assert_eq!(inline_tool_is_error(&[call]), None);
}
