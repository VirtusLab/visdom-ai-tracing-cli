//! Tool calls in an agent's transcript, for `tracevault check` (which counts
//! them) and the stream hook (which keeps a fileless agent's tool calls in a
//! local log `check` can read).
//!
//! The server parses the same schemas in its per-agent adapters
//! (`tracevault-core/src/agent_adapter/` in visdom-ai-tracing). A schema change
//! there likely needs one here too, or `check` stops seeing that agent's calls.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Names of the tools called in one transcript record, in every transcript
/// schema a supported agent writes:
/// - Claude Code: `type: "assistant"`, `message.content[]` blocks of `type: "tool_use"`
/// - pi (GSD) and OpenCode: `type: "message"`, an assistant `message.content[]`
///   with blocks of `type: "toolCall"`
/// - Codex: `type: "response_item"` whose `payload` is a `custom_tool_call` or
///   `function_call` (named) or a `local_shell_call` (counted as `Bash`, the
///   name the server gives it)
/// - Amp: `type: "amp_message"` (the envelope the bundled plugin wraps Amp's
///   own thread messages in), an assistant `message.content[]` with blocks of
///   `type: "tool_use"`. Names are Amp's own (`create_file`, `edit_file`,
///   `shell_command`, `Task`, `create_thread`, ...).
pub fn tool_call_names(entry: &serde_json::Value) -> Vec<&str> {
    fn str_at<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
        v.get(key).and_then(|v| v.as_str())
    }
    let blocks_of_type = |block_type: &'static str| {
        entry
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten()
            .filter(move |b| str_at(b, "type") == Some(block_type))
            .filter_map(|b| str_at(b, "name"))
    };
    match str_at(entry, "type") {
        Some("assistant") => blocks_of_type("tool_use").collect(),
        Some("amp_message")
            if entry.get("message").and_then(|m| str_at(m, "role")) == Some("assistant") =>
        {
            blocks_of_type("tool_use").collect()
        }
        Some("message")
            if entry.get("message").and_then(|m| str_at(m, "role")) == Some("assistant") =>
        {
            blocks_of_type("toolCall").collect()
        }
        Some("response_item") => {
            let Some(payload) = entry.get("payload") else {
                return Vec::new();
            };
            match str_at(payload, "type") {
                Some("custom_tool_call" | "function_call") => {
                    str_at(payload, "name").into_iter().collect()
                }
                Some("local_shell_call") => vec!["Bash"],
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// Count the tool calls in the transcript at `path`, by tool name. The file is
/// read line by line, so a long session's transcript is never held in memory
/// whole. A missing or unreadable file counts nothing, and lines that are not
/// JSON (including invalid UTF-8) are skipped.
pub fn count_tool_calls(path: &Path) -> HashMap<String, i32> {
    let mut counts = HashMap::new();
    let Ok(file) = fs::File::open(path) else {
        return counts;
    };
    for line in BufReader::new(file).split(b'\n').map_while(Result::ok) {
        let Ok(entry) = serde_json::from_slice::<serde_json::Value>(&line) else {
            continue;
        };
        for name in tool_call_names(&entry) {
            *counts.entry(name.to_string()).or_insert(0) += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASH_CALL: &[u8] =
        br#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#;

    #[test]
    fn count_tool_calls_skips_lines_that_are_not_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let bytes = [BASH_CALL, b"\nnot json\n\xff\xfe\n", BASH_CALL].concat();
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            count_tool_calls(&path),
            HashMap::from([("Bash".to_string(), 2)])
        );
    }

    #[test]
    fn count_tool_calls_of_a_missing_file_is_empty() {
        assert!(count_tool_calls(Path::new("/nonexistent/t.jsonl")).is_empty());
    }

    #[test]
    fn amp_tool_use_blocks_are_tool_calls() {
        let assistant = serde_json::json!({"type":"amp_message","event":"agent.end","message":{"role":"assistant","content":[
            {"type":"thinking","thinking":"..."},
            {"type":"tool_use","id":"TU-1","name":"shell_command","input":{"command":"ls"}},
            {"type":"tool_use","id":"TU-2","name":"Task","input":{}}
        ]}});
        assert_eq!(tool_call_names(&assistant), vec!["shell_command", "Task"]);
        let result = serde_json::json!({"type":"amp_message","message":{"role":"user","content":[
            {"type":"tool_result","toolUseID":"TU-1","status":"done","output":"x"}
        ]}});
        assert!(tool_call_names(&result).is_empty());
    }

    #[test]
    fn tool_results_are_not_tool_calls() {
        let result = serde_json::json!({"type":"message","message":{"role":"toolResult","toolName":"bash","content":[{"type":"toolCall","name":"bash"}]}});
        assert!(tool_call_names(&result).is_empty());
    }
}
