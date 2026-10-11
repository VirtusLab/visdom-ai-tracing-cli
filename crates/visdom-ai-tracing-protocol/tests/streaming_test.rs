use serde_json::json;
use std::collections::HashMap;
use tracevault_protocol::streaming::*;

#[test]
fn test_stream_event_request_serialization() {
    let eid = uuid::Uuid::now_v7();
    let req = StreamEventRequest {
        protocol_version: 1,
        tool: None,
        event_type: StreamEventType::ToolUse,
        session_id: "sess-123".to_string(),
        timestamp: chrono::Utc::now(),
        hook_event_name: Some("PostToolUse".to_string()),
        tool_name: Some("Edit".to_string()),
        tool_use_id: None,
        tool_input: Some(
            json!({"file_path": "src/main.rs", "old_string": "old", "new_string": "new"}),
        ),
        tool_response: Some(json!({"success": true})),
        tool_is_error: None,
        event_index: Some(42),
        event_uuid: Some(eid),
        transcript_lines: None,
        transcript_offset: None,
        model: None,
        cwd: None,
        final_stats: None,
        flow_id: None,
        labels: None,
        params: None,
        billing_mode: None,
    };
    let json_str = serde_json::to_string(&req).unwrap();
    let parsed: StreamEventRequest = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed.session_id, "sess-123");
    assert_eq!(parsed.event_index, Some(42));
    assert_eq!(parsed.event_uuid, Some(eid));
}

#[test]
fn test_event_uuid_defaults_to_none_for_legacy_payloads() {
    // A payload from an older client omits event_uuid entirely; it must
    // deserialize as None rather than failing.
    let legacy = json!({
        "protocol_version": 1,
        "event_type": "tool_use",
        "session_id": "sess-legacy",
        "timestamp": chrono::Utc::now(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Edit",
        "tool_use_id": "toolu_legacy",
        "tool_input": null,
        "tool_response": null,
        "tool_is_error": null,
        "event_index": 7,
        "transcript_lines": null,
        "transcript_offset": null,
        "model": null,
        "cwd": null,
        "final_stats": null
    });
    let parsed: StreamEventRequest = serde_json::from_value(legacy).unwrap();
    assert_eq!(parsed.event_uuid, None);
    assert_eq!(parsed.billing_mode, None);
    assert_eq!(parsed.event_index, Some(7));
}

#[test]
fn test_extract_file_change_from_edit() {
    let tool_input = json!({
        "file_path": "/repo/src/lib.rs",
        "old_string": "fn old() {}",
        "new_string": "fn new_func() {}"
    });
    let change = extract_file_change("Edit", &tool_input);
    assert!(change.is_some());
    let c = change.unwrap();
    assert_eq!(c.file_path, "/repo/src/lib.rs");
    assert_eq!(c.change_type, "edit");
    assert!(c.diff_text.is_some());
}

#[test]
fn test_extract_file_change_from_write() {
    let tool_input = json!({
        "file_path": "/repo/src/new_file.rs",
        "content": "fn main() {}"
    });
    let change = extract_file_change("Write", &tool_input);
    assert!(change.is_some());
    let c = change.unwrap();
    assert_eq!(c.file_path, "/repo/src/new_file.rs");
    assert_eq!(c.change_type, "create");
    assert!(c.content_hash.is_some());
}

#[test]
fn test_extract_file_change_from_read_returns_none() {
    let tool_input = json!({"file_path": "/repo/src/lib.rs"});
    assert!(extract_file_change("Read", &tool_input).is_none());
}

#[test]
fn test_is_file_modifying_tool() {
    assert!(is_file_modifying_tool("Write"));
    assert!(is_file_modifying_tool("Edit"));
    assert!(is_file_modifying_tool("Bash"));
    assert!(!is_file_modifying_tool("Read"));
    assert!(!is_file_modifying_tool("Grep"));
}

#[test]
fn test_commit_push_request_serialization() {
    let req = CommitPushRequest {
        commit_sha: "abc123".to_string(),
        branch: Some("main".to_string()),
        author: "dev@example.com".to_string(),
        message: Some("feat: add new feature".to_string()),
        diff_data: Some(json!({"files": []})),
        committed_at: Some(chrono::Utc::now()),
    };
    let json_str = serde_json::to_string(&req).unwrap();
    let parsed: CommitPushRequest = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed.commit_sha, "abc123");
}

#[test]
fn extract_file_change_write_missing_content() {
    let input = json!({"file_path": "/tmp/test.rs"});
    assert!(extract_file_change("Write", &input).is_none());
}

#[test]
fn extract_file_change_edit_missing_old_string() {
    let input = json!({"file_path": "/tmp/test.rs", "new_string": "new"});
    assert!(extract_file_change("Edit", &input).is_none());
}

#[test]
fn extract_file_change_write_missing_file_path() {
    let input = json!({"content": "hello"});
    assert!(extract_file_change("Write", &input).is_none());
}

/// Regression: pending→flush round-trip must preserve context stamp fields.
///
/// The stream hook serializes a `StreamEventRequest` to JSON before appending
/// it to `pending.jsonl`; `flush` deserializes each line back to resend it.
/// If a new field were ever added without `#[serde(default)]` the
/// deserialization of a line written by an older client would fail silently
/// (or panic). This test serializes a fully-populated request and checks that
/// `flow_id`, `labels`, and `params` survive the round-trip unchanged.
#[test]
fn pending_flush_round_trip_preserves_context_stamp() {
    let mut params = HashMap::new();
    params.insert("env".to_string(), "staging".to_string());
    params.insert("region".to_string(), "us-east-1".to_string());

    let req = StreamEventRequest {
        protocol_version: 1,
        tool: Some("claude-code".to_string()),
        event_type: StreamEventType::ToolUse,
        session_id: "sess-rt-test".to_string(),
        timestamp: chrono::Utc::now(),
        hook_event_name: Some("PostToolUse".to_string()),
        tool_name: Some("Edit".to_string()),
        tool_use_id: None,
        tool_input: None,
        tool_response: None,
        tool_is_error: None,
        event_index: None,
        event_uuid: Some(uuid::Uuid::now_v7()),
        transcript_lines: None,
        transcript_offset: None,
        model: None,
        cwd: Some("/repo".to_string()),
        final_stats: None,
        flow_id: Some("flow-round-trip".to_string()),
        labels: Some(vec!["backend".to_string(), "payments".to_string()]),
        params: Some(params.clone()),
        billing_mode: None,
    };

    let json_str = serde_json::to_string(&req).expect("serialization must succeed");
    let parsed: StreamEventRequest =
        serde_json::from_str(&json_str).expect("deserialization must succeed");

    assert_eq!(
        parsed.flow_id,
        Some("flow-round-trip".to_string()),
        "flow_id must survive the JSON round-trip"
    );
    assert_eq!(
        parsed.labels,
        Some(vec!["backend".to_string(), "payments".to_string()]),
        "labels must survive the JSON round-trip"
    );
    let round_tripped_params = parsed.params.expect("params must be Some after round-trip");
    assert_eq!(
        round_tripped_params.get("env").map(String::as_str),
        Some("staging"),
        "params[env] must survive the JSON round-trip"
    );
    assert_eq!(
        round_tripped_params.get("region").map(String::as_str),
        Some("us-east-1"),
        "params[region] must survive the JSON round-trip"
    );
}

// ── billing_mode: wire compatibility in both directions ─────────────────────

fn request_with_billing_mode(billing_mode: Option<BillingMode>) -> StreamEventRequest {
    StreamEventRequest {
        protocol_version: 2,
        tool: Some("codex".to_string()),
        event_type: StreamEventType::SessionStart,
        session_id: "sess-billing".to_string(),
        timestamp: chrono::Utc::now(),
        hook_event_name: Some("SessionStart".to_string()),
        tool_name: None,
        tool_use_id: None,
        tool_input: None,
        tool_response: None,
        tool_is_error: None,
        event_index: None,
        event_uuid: Some(uuid::Uuid::now_v7()),
        transcript_lines: None,
        transcript_offset: None,
        model: None,
        cwd: Some("/repo".to_string()),
        final_stats: None,
        flow_id: None,
        labels: None,
        params: None,
        billing_mode,
    }
}

#[test]
fn billing_mode_wire_values_are_snake_case() {
    for (mode, wire) in [
        (BillingMode::Gateway, "gateway"),
        (BillingMode::ApiKey, "api_key"),
        (BillingMode::Subscription, "subscription"),
        (BillingMode::Unknown, "unknown"),
    ] {
        assert_eq!(serde_json::to_value(mode).unwrap(), json!(wire));
        let back: BillingMode = serde_json::from_value(json!(wire)).unwrap();
        assert_eq!(back, mode);
    }
}

#[test]
fn billing_mode_round_trips_on_the_request() {
    let req = request_with_billing_mode(Some(BillingMode::Subscription));
    let value = serde_json::to_value(&req).unwrap();
    assert_eq!(value["billing_mode"], json!("subscription"));
    let parsed: StreamEventRequest = serde_json::from_value(value).unwrap();
    assert_eq!(parsed.billing_mode, Some(BillingMode::Subscription));
}

/// A client that does not know the mode leaves the field off the wire
/// entirely, so the payload is byte-for-byte what older clients sent.
#[test]
fn absent_billing_mode_is_omitted_from_the_payload() {
    let value = serde_json::to_value(request_with_billing_mode(None)).unwrap();
    assert!(
        value.get("billing_mode").is_none(),
        "billing_mode must be omitted when None, got {value}"
    );
}

/// A mode added by a later protocol version must not make this version
/// reject the whole event: it degrades to `Unknown`.
#[test]
fn unrecognised_billing_mode_degrades_to_unknown() {
    let mut value = serde_json::to_value(request_with_billing_mode(None)).unwrap();
    value["billing_mode"] = json!("enterprise_commit");
    let parsed: StreamEventRequest = serde_json::from_value(value).unwrap();
    assert_eq!(parsed.billing_mode, Some(BillingMode::Unknown));
}

/// The `StreamEventRequest` shape as published in protocol 0.28.x, before
/// `billing_mode` existed — what a server still built on that release
/// deserializes into. Same derive (no `deny_unknown_fields`), same fields.
#[allow(dead_code)]
#[derive(serde::Deserialize)]
struct StreamEventRequestV028 {
    protocol_version: u32,
    #[serde(default)]
    tool: Option<String>,
    event_type: StreamEventType,
    session_id: String,
    timestamp: chrono::DateTime<chrono::Utc>,
    hook_event_name: Option<String>,
    tool_name: Option<String>,
    tool_use_id: Option<String>,
    tool_input: Option<serde_json::Value>,
    tool_response: Option<serde_json::Value>,
    tool_is_error: Option<bool>,
    event_index: Option<i32>,
    #[serde(default)]
    event_uuid: Option<uuid::Uuid>,
    transcript_lines: Option<Vec<serde_json::Value>>,
    transcript_offset: Option<i64>,
    model: Option<String>,
    cwd: Option<String>,
    final_stats: Option<SessionFinalStats>,
    #[serde(default)]
    flow_id: Option<String>,
    #[serde(default)]
    labels: Option<Vec<String>>,
    #[serde(default)]
    params: Option<HashMap<String, String>>,
}

/// New CLI → old server: an event carrying `billing_mode` still parses on a
/// server built on protocol 0.28, which just ignores the field.
#[test]
fn new_client_payload_parses_on_a_pre_billing_mode_server() {
    for mode in [
        BillingMode::Gateway,
        BillingMode::ApiKey,
        BillingMode::Subscription,
        BillingMode::Unknown,
    ] {
        let payload = serde_json::to_string(&request_with_billing_mode(Some(mode))).unwrap();
        let old: StreamEventRequestV028 = serde_json::from_str(&payload)
            .unwrap_or_else(|e| panic!("0.28 server rejected {mode:?}: {e}"));
        assert_eq!(old.session_id, "sess-billing");
    }
}

/// Old CLI → new server: an event from a client that predates the field
/// parses, with `billing_mode` = None ("not reported", distinct from a client
/// that reported `Unknown`).
#[test]
fn old_client_payload_parses_with_billing_mode_none() {
    let mut value = serde_json::to_value(request_with_billing_mode(None)).unwrap();
    value.as_object_mut().unwrap().remove("billing_mode");
    let parsed: StreamEventRequest = serde_json::from_value(value).unwrap();
    assert_eq!(parsed.billing_mode, None);
}
