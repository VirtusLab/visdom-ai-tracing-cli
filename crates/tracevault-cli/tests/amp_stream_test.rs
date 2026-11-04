//! The bundled Amp plugin (`assets/amp-plugin/index.ts`), run on real Amp
//! event recordings (`tests/data/amp/`, from the VIS-1196 spike) through
//! `tests/data/amp/replay.mjs`, a stub of Amp's plugin host.
//!
//! The plugin is TypeScript; Node >= 22.18 runs it as is (type stripping).
//! Without such a Node the tests are skipped locally, and fail under CI.
//!
//! What the plugin must do, from the spike: send the prompt and each tool
//! call as they happen (a killed terminal fires no `agent.end`), then only
//! what is new from `agent.end.messages`, so every tool call reaches the
//! server once, including those of a `Task` subagent and the tools that fire
//! no tool events. And it must never break Amp, with or without `tracevault`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tracevault_cli::agent::Agent;
use tracevault_cli::commands::init::AMP_PLUGIN_INDEX;
use tracevault_cli::commands::stream::{inline_tool_is_error, stamp_agent};
use tracevault_cli::transcript::{count_tool_calls, tool_call_names};
use tracevault_protocol::streaming::{StreamEventRequest, StreamEventType};

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/amp")
}

/// A Node that runs `.mts` files with type stripping, or `None` (and the
/// test is skipped) outside CI.
fn node() -> Option<&'static str> {
    let ok = Command::new("node")
        .args(["--no-warnings", "--experimental-strip-types", "-e", "0"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        return Some("node");
    }
    assert!(
        std::env::var_os("CI").is_none(),
        "CI needs Node >= 22.18 on PATH to test the Amp plugin"
    );
    eprintln!("skipping: no Node with type stripping on PATH");
    None
}

/// One `tracevault` run the plugin made: its arguments and the hook event it
/// wrote to stdin.
#[derive(Debug)]
struct Call {
    args: String,
    payload: Value,
}

impl Call {
    fn event(&self) -> &str {
        let mut words = self.args.split_whitespace();
        words.find(|w| *w == "--event");
        words.next().unwrap()
    }
    fn records(&self) -> &[Value] {
        self.payload["transcript_records"].as_array().unwrap()
    }
}

struct Replay {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
    calls: Vec<Call>,
    /// `begin` / `end` lines of a `Slow` fake.
    spans: Vec<String>,
}

impl Replay {
    /// What each `tool.call` handler returned.
    fn tool_call_results(&self) -> Vec<Value> {
        self.stdout
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|v| v["kind"] == "tool.call")
            .map(|v| v["result"].clone())
            .collect()
    }
    fn records(&self) -> Vec<&Value> {
        self.calls.iter().flat_map(|c| c.records()).collect()
    }
}

/// What a fake `tracevault` on `PATH` does.
#[derive(Clone, Copy, PartialEq, Debug)]
enum FakeTracevault {
    /// Log its arguments and stdin.
    Record,
    /// Like `Record`, but slow: logs `begin` / `end` around a 50 ms sleep to
    /// `$TV_LOG.spans`, so overlapping runs show.
    Slow,
    /// Exit at once without reading stdin.
    ExitEarly,
    /// Not on `PATH` at all.
    Missing,
}

fn replay(fixture: &str, fake: FakeTracevault) -> Option<Replay> {
    replay_with(fixture, fake, false)
}

/// `concurrent`: deliver events without waiting for the previous handler.
fn replay_with(fixture: &str, fake: FakeTracevault, concurrent: bool) -> Option<Replay> {
    let node = node()?;
    let tmp = tempfile::tempdir().unwrap();
    let plugin = tmp.path().join("plugin.mts");
    std::fs::write(&plugin, AMP_PLUGIN_INDEX).unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let log = tmp.path().join("calls.log");
    let script = match fake {
        FakeTracevault::Record => {
            Some("#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$TV_LOG\"\ncat >> \"$TV_LOG\"\necho >> \"$TV_LOG\"\n")
        }
        FakeTracevault::Slow => Some(
            "#!/bin/sh\necho begin >> \"$TV_LOG.spans\"\nsleep 0.05\nprintf '%s\\n' \"$*\" >> \"$TV_LOG\"\ncat >> \"$TV_LOG\"\necho >> \"$TV_LOG\"\necho end >> \"$TV_LOG.spans\"\n",
        ),
        FakeTracevault::ExitEarly => Some("#!/bin/sh\nexit 1\n"),
        FakeTracevault::Missing => None,
    };
    if let Some(script) = script {
        let path = bin.join("tracevault");
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out = Command::new(node)
        .args(["--no-warnings", "--experimental-strip-types"])
        .arg(data_dir().join("replay.mjs"))
        .arg(&plugin)
        .arg(data_dir().join(fixture))
        .current_dir(tmp.path())
        // `sh` for the fake; nothing that could hold a real `tracevault`.
        .env("PATH", format!("{}:/bin:/usr/bin", bin.display()))
        .env("TV_LOG", &log)
        .env("REPLAY_CONCURRENT", if concurrent { "1" } else { "0" })
        .output()
        .unwrap();
    let spans = std::fs::read_to_string(format!("{}.spans", log.display())).unwrap_or_default();
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let calls = lines
        .chunks(2)
        .map(|pair| Call {
            args: pair[0].to_string(),
            payload: serde_json::from_str(pair[1]).unwrap(),
        })
        .collect();
    Some(Replay {
        status: out.status,
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
        calls,
        spans: spans.lines().map(str::to_string).collect(),
    })
}

/// The fixture's raw `{kind, data}` lines.
fn fixture_lines(fixture: &str) -> Vec<Value> {
    std::fs::read_to_string(data_dir().join(fixture))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn agent_end_messages(fixture: &str) -> Vec<Value> {
    fixture_lines(fixture)
        .into_iter()
        .filter(|l| l["kind"] == "agent.end")
        .flat_map(|l| l["data"]["event"]["messages"].as_array().unwrap().clone())
        .collect()
}

fn tool_result_ids(fixture: &str) -> Vec<String> {
    fixture_lines(fixture)
        .iter()
        .filter(|l| l["kind"] == "tool.result")
        .map(|l| {
            l["data"]["event"]["toolUseID"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

fn blocks<'a>(message: &'a Value, block_type: &'a str) -> impl Iterator<Item = &'a Value> {
    message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(move |b| b["type"] == block_type)
}

/// toolUseID → how many `tool_use` blocks the plugin sent for it.
fn sent_tool_uses(replay: &Replay) -> BTreeMap<String, usize> {
    let mut seen = BTreeMap::new();
    for record in replay.records() {
        for b in blocks(&record["message"], "tool_use") {
            *seen
                .entry(b["id"].as_str().unwrap().to_string())
                .or_default() += 1;
        }
    }
    seen
}

fn usage_total(messages: impl IntoIterator<Item = Value>) -> i64 {
    messages
        .into_iter()
        .filter_map(|m| m.get("usage").cloned())
        .map(|u| {
            [
                "inputTokens",
                "outputTokens",
                "cacheCreationInputTokens",
                "cacheReadInputTokens",
            ]
            .iter()
            .map(|k| u[k].as_i64().unwrap_or(0))
            .sum::<i64>()
        })
        .sum()
}

fn events(replay: &Replay) -> Vec<&str> {
    replay.calls.iter().map(Call::event).collect()
}

#[test]
fn headless_turn_sends_every_tool_call_once_and_every_usage() {
    let fixture = "headless-multitool.plugin.jsonl";
    let Some(r) = replay(fixture, FakeTracevault::Record) else {
        return;
    };
    assert!(r.status.success(), "stderr: {}", r.stderr);

    let live = tool_result_ids(fixture).len();
    let mut expected = vec!["notification", "user-prompt-submit"];
    expected.extend(std::iter::repeat_n("post-tool-use", live + 1));
    expected.push("stop");
    assert_eq!(events(&r), expected);

    for call in &r.calls {
        assert_eq!(
            call.payload["session_id"],
            "T-00000000-0000-7000-8000-000000000002"
        );
        assert!(call.args.ends_with("--agent amp"), "{}", call.args);
        assert_eq!(call.payload["transcript_path"], "");
        for record in call.records() {
            assert_eq!(record["type"], "amp_message", "{record}");
            assert_eq!(record["thread_id"], call.payload["session_id"]);
            assert!(record["timestamp"].is_string(), "{record}");
        }
    }

    // Each tool call once: those seen live plus those only in agent.end.
    let in_messages: BTreeSet<String> = agent_end_messages(fixture)
        .iter()
        .flat_map(|m| blocks(m, "tool_use").map(|b| b["id"].as_str().unwrap().to_string()))
        .collect();
    let live_ids: BTreeSet<String> = tool_result_ids(fixture).into_iter().collect();
    let sent = sent_tool_uses(&r);
    assert!(sent.values().all(|n| *n == 1), "{sent:?}");
    let all: BTreeSet<String> = in_messages.union(&live_ids).cloned().collect();
    assert_eq!(sent.keys().cloned().collect::<BTreeSet<_>>(), all);

    // The Task subagent's own shell calls (live, never in the turn's
    // messages) are forwarded, not dropped.
    let subagent: Vec<&String> = live_ids.difference(&in_messages).collect();
    assert!(!subagent.is_empty(), "the fixture has a Task subagent");

    // `Task` fires no tool events: it reaches the server from agent.end.
    let final_records = r.calls[r.calls.len() - 2].records();
    let names: Vec<&str> = final_records.iter().flat_map(tool_call_names).collect();
    assert_eq!(names, vec!["Task"]);

    // The prompt went out on agent.start, and its text not again on
    // agent.end; the context the spike's plugin appended to it on
    // agent.start (a second text block, only in agent.end) did.
    let prompt_text = fixture_lines(fixture)
        .iter()
        .find(|l| l["kind"] == "agent.start")
        .unwrap()["data"]["event"]["message"]
        .clone();
    let texts: Vec<(&Value, &Value)> = r
        .records()
        .into_iter()
        .filter(|rec| rec["message"]["role"] == "user")
        .flat_map(|rec| blocks(&rec["message"], "text").map(move |b| (&rec["event"], &b["text"])))
        .collect();
    let prompt_sends: Vec<&Value> = texts
        .iter()
        .filter(|(_, t)| **t == prompt_text)
        .map(|(e, _)| *e)
        .collect();
    assert_eq!(prompt_sends, vec!["agent.start"], "{texts:?}");
    let appended: Vec<&(&Value, &Value)> = texts
        .iter()
        .filter(|(_, t)| t.as_str().unwrap().contains("PURPLE-OTTER-42"))
        .collect();
    assert_eq!(appended.len(), 1, "{texts:?}");
    assert_eq!(appended[0].0, "agent.end");
    let prompt_records: Vec<&Value> = r
        .records()
        .into_iter()
        .filter(|rec| rec["message"]["id"] == "M-0000000000000000000002")
        .collect();
    assert_eq!(
        prompt_records.len(),
        2,
        "agent.start + the appended context"
    );

    // No usage is lost: every assistant message is sent, with its usage.
    let sent_usage = usage_total(r.records().into_iter().map(|rec| rec["message"].clone()));
    let recorded = usage_total(agent_end_messages(fixture));
    assert!(recorded > 0);
    assert_eq!(sent_usage, recorded);
}

#[test]
fn tool_events_carry_the_tool_and_amp_error_semantics() {
    let Some(r) = replay("headless-multitool.plugin.jsonl", FakeTracevault::Record) else {
        return;
    };
    let tool_calls: Vec<&Call> = r
        .calls
        .iter()
        .filter(|c| {
            c.payload["hook_event_name"] == "PostToolUse" && !c.payload["tool_use_id"].is_null()
        })
        .collect();
    assert!(!tool_calls.is_empty());
    for call in &tool_calls {
        let records = call.records();
        assert_eq!(records.len(), 2);
        let tool_use = blocks(&records[0]["message"], "tool_use").next().unwrap();
        assert_eq!(tool_use["id"], call.payload["tool_use_id"]);
        assert_eq!(tool_use["name"], call.payload["tool_name"]);
        assert_eq!(tool_use["input"], call.payload["tool_input"]);
        let result = blocks(&records[1]["message"], "tool_result")
            .next()
            .unwrap();
        assert_eq!(result["toolUseID"], call.payload["tool_use_id"]);
        // Only status "error" is an error. Here: a permission rejection and
        // `cat` of a missing file (exitCode 1) are both "done".
        assert_eq!(inline_tool_is_error(records), Some(false), "{result}");
    }
    let outputs: Vec<String> = tool_calls
        .iter()
        .map(|c| c.records()[1]["message"]["content"][0]["output"].to_string())
        .collect();
    assert!(
        outputs.iter().any(|o| o.contains("Tool rejected")),
        "{outputs:?}"
    );
    assert!(
        outputs.iter().any(|o| o.contains("\"exitCode\":1")),
        "{outputs:?}"
    );
}

#[test]
fn continued_thread_starts_once_and_sends_side_thread_tools() {
    let fixture = "headless-continue.plugin.jsonl";
    let Some(r) = replay(fixture, FakeTracevault::Record) else {
        return;
    };
    assert!(r.status.success(), "stderr: {}", r.stderr);
    // `threads continue -x` loads the plugin twice; the first is disposed at
    // once and sends nothing. The second starts the session once.
    let ev = events(&r);
    assert_eq!(ev.iter().filter(|e| **e == "notification").count(), 1);
    assert_eq!(ev.first(), Some(&"notification"));
    assert_eq!(ev.last(), Some(&"stop"));

    let names: BTreeSet<&str> = r.records().into_iter().flat_map(tool_call_names).collect();
    for tool in [
        "create_thread",
        "wait_for_threads",
        "read_thread",
        "edit_file",
    ] {
        assert!(names.contains(tool), "{tool} missing from {names:?}");
    }
    assert!(sent_tool_uses(&r).values().all(|n| *n == 1));

    // edit_file failed with status "error" and no output: an error.
    let edit = r
        .calls
        .iter()
        .find(|c| c.payload["tool_name"] == "edit_file")
        .unwrap();
    assert_eq!(inline_tool_is_error(edit.records()), Some(true));
    let result = &edit.records()[1]["message"]["content"][0];
    assert!(result.get("output").is_none(), "{result}");
    assert_eq!(result["error"], "Could not find exact match for old_str");
}

#[test]
fn cancelled_turn_sends_the_running_call_from_agent_end() {
    let Some(r) = replay(
        "interactive-cancel-ctrlc.plugin.jsonl",
        FakeTracevault::Record,
    ) else {
        return;
    };
    assert!(r.status.success(), "stderr: {}", r.stderr);
    assert_eq!(events(&r).iter().filter(|e| **e == "stop").count(), 2);
    // The cancelled shell call fired no tool.result; agent.end has it, with
    // a "cancelled" result.
    let last_turn = r.calls[r.calls.len() - 2].records();
    let names: Vec<&str> = last_turn.iter().flat_map(tool_call_names).collect();
    assert_eq!(names, vec!["shell_command"]);
    let statuses: Vec<&Value> = last_turn
        .iter()
        .flat_map(|rec| blocks(&rec["message"], "tool_result"))
        .map(|b| &b["status"])
        .collect();
    assert_eq!(statuses, vec!["cancelled"]);
    assert!(sent_tool_uses(&r).values().all(|n| *n == 1));
}

#[test]
fn killed_terminal_keeps_what_was_sent_before() {
    let Some(r) = replay(
        "interactive-killed-terminal.plugin.jsonl",
        FakeTracevault::Record,
    ) else {
        return;
    };
    assert!(r.status.success(), "stderr: {}", r.stderr);
    // No agent.end and no onDispose: the prompt went out when it arrived.
    assert_eq!(events(&r), vec!["notification", "user-prompt-submit"]);
}

#[test]
fn tool_call_is_always_allowed() {
    for fixture in [
        "headless-multitool.plugin.jsonl",
        "headless-sigterm.plugin.jsonl",
    ] {
        for fake in [FakeTracevault::Record, FakeTracevault::Missing] {
            let Some(r) = replay(fixture, fake) else {
                return;
            };
            let results = r.tool_call_results();
            assert!(!results.is_empty());
            assert!(
                results
                    .iter()
                    .all(|v| *v == serde_json::json!({"action": "allow"})),
                "{results:?}"
            );
        }
    }
}

/// With `tracevault` missing from PATH, or exiting before it reads its
/// input, every handler still returns and Amp's session goes on.
#[test]
fn amp_keeps_working_without_tracevault() {
    for fake in [FakeTracevault::Missing, FakeTracevault::ExitEarly] {
        for fixture in [
            "headless-multitool.plugin.jsonl",
            "headless-continue.plugin.jsonl",
            "interactive-cancel-ctrlc.plugin.jsonl",
        ] {
            let Some(r) = replay(fixture, fake) else {
                return;
            };
            assert!(r.status.success(), "{fixture}: stderr: {}", r.stderr);
            let handled = r
                .stdout
                .lines()
                .filter(|l| !l.contains(r#""kind":"dispose""#))
                .count();
            let delivered = fixture_lines(fixture)
                .iter()
                .filter(|l| {
                    matches!(
                        l["kind"].as_str(),
                        Some(
                            "session.start"
                                | "agent.start"
                                | "tool.call"
                                | "tool.result"
                                | "agent.end"
                        )
                    )
                })
                .count();
            assert_eq!(handled, delivered, "{fixture}: a handler did not return");
            if fake == FakeTracevault::Missing {
                assert!(r.stderr.contains("spawn"), "{fixture}: {}", r.stderr);
            }
        }
    }
}

/// Events delivered without waiting for the previous handler, with a slow
/// `tracevault`: one thread's runs still never overlap, they keep the event
/// order, and onDispose waits until the queued ones have run.
#[test]
fn overlapping_events_are_sent_one_at_a_time_in_order() {
    let fixture = "headless-multitool.plugin.jsonl";
    let Some(sequential) = replay(fixture, FakeTracevault::Record) else {
        return;
    };
    let Some(r) = replay_with(fixture, FakeTracevault::Slow, true) else {
        return;
    };
    assert!(r.status.success(), "stderr: {}", r.stderr);

    let expected: Vec<&str> = r.calls.iter().map(|_| "begin").collect();
    assert_eq!(r.spans.len(), expected.len() * 2);
    for pair in r.spans.chunks(2) {
        assert_eq!(pair, ["begin", "end"], "runs overlapped: {:?}", r.spans);
    }

    let payloads = |r: &Replay| -> Vec<(String, Value)> {
        r.calls
            .iter()
            .map(|c| {
                let mut p = c.payload.clone();
                for rec in p["transcript_records"].as_array_mut().unwrap() {
                    rec.as_object_mut().unwrap().remove("timestamp");
                }
                // Each replay runs in its own temp dir.
                p.as_object_mut().unwrap().remove("cwd");
                (c.args.clone(), p)
            })
            .collect()
    };
    assert_eq!(payloads(&r), payloads(&sequential));

    let disposed: Vec<Value> = r
        .stdout
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["kind"] == "dispose")
        .collect();
    assert_eq!(disposed.len(), 1);
    assert_eq!(
        disposed[0]["result"]["logged"].as_u64(),
        Some(2 * r.calls.len() as u64),
        "onDispose returned before the queued sends ran"
    );
}

/// The real `tracevault stream` accepts what the plugin sends and keeps each
/// tool call in the session's local log, which `tracevault check` counts
/// (no server needed: the log is written before any request).
#[test]
fn real_cli_keeps_amp_tool_calls_for_check() {
    let Some(node) = node() else {
        return;
    };
    let fixture = "headless-multitool.plugin.jsonl";
    let repo = tempfile::tempdir().unwrap();
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(repo.path())
        .status()
        .unwrap();
    assert!(status.success());
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_tracevault"), bin.join("tracevault")).unwrap();
    let plugin = home.path().join("plugin.mts");
    std::fs::write(&plugin, AMP_PLUGIN_INDEX).unwrap();
    let git_dir = Path::new(
        &String::from_utf8(
            Command::new("sh")
                .args(["-c", "command -v git"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim(),
    )
    .parent()
    .unwrap()
    .to_path_buf();
    let out = Command::new(node)
        .args(["--no-warnings", "--experimental-strip-types"])
        .arg(data_dir().join("replay.mjs"))
        .arg(&plugin)
        .arg(data_dir().join(fixture))
        .current_dir(repo.path())
        .env(
            "PATH",
            format!("{}:{}:/bin:/usr/bin", bin.display(), git_dir.display()),
        )
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env_remove("TRACEVAULT_API_KEY")
        .env_remove("TRACEVAULT_SERVER_URL")
        .env_remove("TRACEVAULT_SESSION_ID")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let log = repo
        .path()
        .join(".tracevault/sessions/T-00000000-0000-7000-8000-000000000002/tool_calls.jsonl");
    let counts = count_tool_calls(&log);
    let live = tool_result_ids(fixture).len() as i32;
    // Every live call plus the Task call that only agent.end carries.
    assert_eq!(counts.values().sum::<i32>(), live + 1, "{counts:?}");
    assert_eq!(counts.get("Task"), Some(&1), "{counts:?}");
    assert_eq!(counts.get("create_file"), Some(&1), "{counts:?}");
    assert_eq!(counts.get("edit_file"), Some(&1), "{counts:?}");
    assert!(
        counts.get("shell_command").copied().unwrap_or(0) >= 4,
        "{counts:?}"
    );
}

#[test]
fn amp_inline_records_produce_amp_tagged_request() {
    let records = vec![serde_json::json!({
        "type": "amp_message", "event": "agent.start", "thread_id": "T-1",
        "timestamp": "2026-11-04T22:54:25.520Z",
        "message": {"role": "user", "id": "M-1", "content": [{"type": "text", "text": "hi"}]},
    })];
    let mut req = StreamEventRequest {
        protocol_version: 1,
        tool: Some("claude-code".to_string()),
        event_type: StreamEventType::ToolUse,
        session_id: "T-1".into(),
        timestamp: chrono::Utc::now(),
        hook_event_name: Some("UserPromptSubmit".into()),
        tool_name: None,
        tool_use_id: None,
        tool_input: None,
        tool_response: None,
        tool_is_error: None,
        event_index: None,
        event_uuid: None,
        transcript_lines: Some(records.clone()),
        transcript_offset: Some(0),
        model: None,
        cwd: None,
        final_stats: None,
        flow_id: None,
        labels: None,
        params: None,
        billing_mode: None,
    };
    stamp_agent(&mut req, Agent::Amp);
    assert_eq!(req.tool.as_deref(), Some("amp"));
    assert_eq!(req.protocol_version, 2);
    assert_eq!(req.transcript_lines.as_deref(), Some(&records[..]));
}

#[test]
fn inline_tool_is_error_reads_amp_status() {
    let result = |status: &str| {
        serde_json::json!({"type": "amp_message", "message": {"role": "user", "content": [
            {"type": "tool_result", "toolUseID": "TU-1", "status": status, "output": "x"}
        ]}})
    };
    let call = serde_json::json!({"type": "amp_message", "message": {"role": "assistant", "content": [
        {"type": "tool_use", "id": "TU-1", "name": "shell_command", "input": {}}
    ]}});
    assert_eq!(
        inline_tool_is_error(&[call.clone(), result("error")]),
        Some(true)
    );
    assert_eq!(
        inline_tool_is_error(&[call.clone(), result("done")]),
        Some(false)
    );
    assert_eq!(
        inline_tool_is_error(&[call.clone(), result("cancelled")]),
        Some(false)
    );
    assert_eq!(inline_tool_is_error(&[call]), None);
}
