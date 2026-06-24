use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StreamEventType {
    ToolUse,
    Transcript,
    SessionStart,
    SessionEnd,
    /// Client declares the start of a verification phase. Only the most
    /// recent event of this type per session is meaningful — re-opening
    /// is idempotent and just moves the cursor forward.
    VerificationPhaseStart,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEventRequest {
    pub protocol_version: u32,
    #[serde(default)]
    pub tool: Option<String>,
    pub event_type: StreamEventType,
    pub session_id: String,
    pub timestamp: DateTime<Utc>,
    pub hook_event_name: Option<String>,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub tool_input: Option<serde_json::Value>,
    pub tool_response: Option<serde_json::Value>,
    pub tool_is_error: Option<bool>,
    /// Legacy per-session ordering counter (from the CLI's `.event_counter`).
    /// Superseded by `event_uuid`; still accepted from older clients and used as
    /// an ordering fallback. New clients send `None`.
    pub event_index: Option<i32>,
    /// Client-minted UUIDv7 stamped at hook-fire time. Time-ordered, so it is the
    /// ordering key for events; absent (`None`) from older clients, which fall
    /// back to `event_index`.
    #[serde(default)]
    pub event_uuid: Option<uuid::Uuid>,
    pub transcript_lines: Option<Vec<serde_json::Value>>,
    pub transcript_offset: Option<i64>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub final_stats: Option<SessionFinalStats>,
    /// Optional flow identifier grouping this event into a named flow.
    /// `None` when the client does not send this field.
    #[serde(default)]
    pub flow_id: Option<String>,
    /// Ad-hoc string labels for categorisation.
    /// `None` when the client does not send this field; defaults to empty vec.
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    /// Arbitrary key/value metadata as a JSONB object.
    /// `None` when the client does not send this field; defaults to empty object.
    #[serde(default)]
    pub params: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFinalStats {
    pub duration_ms: Option<i64>,
    pub total_tokens: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub user_messages: Option<i32>,
    pub assistant_messages: Option<i32>,
    pub total_tool_calls: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEventResponse {
    pub session_db_id: uuid::Uuid,
    pub event_db_id: Option<uuid::Uuid>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitPushRequest {
    pub commit_sha: String,
    pub branch: Option<String>,
    pub author: String,
    pub message: Option<String>,
    pub diff_data: Option<serde_json::Value>,
    pub committed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitPushResponse {
    pub commit_db_id: uuid::Uuid,
    pub attributions_count: i64,
}

#[derive(Debug, Clone)]
pub struct ExtractedFileChange {
    pub file_path: String,
    pub change_type: String,
    pub diff_text: Option<String>,
    pub content_hash: Option<String>,
}

pub fn is_file_modifying_tool(tool_name: &str) -> bool {
    matches!(tool_name, "Write" | "Edit" | "Bash")
}

impl StreamEventRequest {
    /// Drop optional fields largest-first until the serialized payload is
    /// under 512 KB. Prevents 413 errors on both real-time sends and flush.
    pub fn truncate_large_fields(&mut self) {
        const MAX_BYTES: usize = 512 * 1024;
        if serde_json::to_string(self).map(|s| s.len()).unwrap_or(0) <= MAX_BYTES {
            return;
        }
        self.transcript_lines = None;
        if serde_json::to_string(self).map(|s| s.len()).unwrap_or(0) <= MAX_BYTES {
            return;
        }
        self.tool_response = None;
        if serde_json::to_string(self).map(|s| s.len()).unwrap_or(0) <= MAX_BYTES {
            return;
        }
        self.tool_input = None;
    }
}

/// Scan transcript lines for a tool_result whose tool_use_id matches the given id
/// and return its is_error flag. Returns None if not found or no transcript available.
pub fn extract_is_error_from_transcript(
    tool_use_id: &str,
    transcript_lines: &[serde_json::Value],
) -> Option<bool> {
    for line in transcript_lines {
        // Transcript lines are full message objects; content is an array of blocks.
        // Use explicit matching so a malformed line is skipped rather than aborting the scan.
        let content = match line
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        {
            Some(c) => c,
            None => continue,
        };
        for block in content {
            let block_type = block.get("type").and_then(|v| v.as_str());
            let block_uid = block.get("tool_use_id").and_then(|v| v.as_str());
            if block_type == Some("tool_result") && block_uid == Some(tool_use_id) {
                return block.get("is_error").and_then(|v| v.as_bool());
            }
        }
    }
    None
}

pub fn extract_file_change(
    tool_name: &str,
    tool_input: &serde_json::Value,
) -> Option<ExtractedFileChange> {
    match tool_name {
        "Write" => {
            let file_path = tool_input.get("file_path")?.as_str()?.to_string();
            let content = tool_input.get("content")?.as_str()?;
            let mut hasher = Sha256::new();
            hasher.update(content.as_bytes());
            let hash = hex::encode(hasher.finalize());
            let diff = content
                .lines()
                .map(|l| format!("+{l}"))
                .collect::<Vec<_>>()
                .join("\n");
            Some(ExtractedFileChange {
                file_path,
                change_type: "create".to_string(),
                diff_text: Some(diff),
                content_hash: Some(hash),
            })
        }
        "Edit" => {
            let file_path = tool_input.get("file_path")?.as_str()?.to_string();
            let old_string = tool_input.get("old_string")?.as_str()?;
            let new_string = tool_input.get("new_string")?.as_str()?;
            let diff = format!("--- {old_string}\n+++ {new_string}");
            Some(ExtractedFileChange {
                file_path,
                change_type: "edit".to_string(),
                diff_text: Some(diff),
                content_hash: None,
            })
        }
        _ => None,
    }
}
