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
///
/// The path is absolute: the replays run with a `PATH` of their own, which
/// need not hold Node (on CI it lives in the tool cache).
fn node() -> Option<PathBuf> {
    let found = Command::new("node")
        .args([
            "--no-warnings",
            "--experimental-strip-types",
            "-p",
            "process.execPath",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
    if let Some(path) = found {
        return Some(path);
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
    /// The `tracevault stream` runs.
    calls: Vec<Call>,
    /// The `tracevault session-start` / `user-prompt` runs (policy lookups).
    policy_calls: Vec<Call>,
    /// `begin` / `end` lines of a `Slow` fake.
    spans: Vec<String>,
}

impl Replay {
    /// What each `tool.call` handler returned.
    fn tool_call_results(&self) -> Vec<Value> {
        self.results("tool.call")
    }
    /// What each `agent.start` handler returned.
    fn agent_start_results(&self) -> Vec<Value> {
        self.results("agent.start")
    }
    fn results(&self, kind: &str) -> Vec<Value> {
        self.stdout
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|v| v["kind"] == kind)
            .map(|v| v["result"].clone())
            .collect()
    }
    /// How long each `kind` handler took, in ms.
    fn handler_ms(&self, kind: &str) -> Vec<u64> {
        self.stdout
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|v| v["kind"] == kind)
            .map(|v| v["ms"].as_u64().unwrap())
            .collect()
    }
    fn records(&self) -> Vec<&Value> {
        self.calls.iter().flat_map(|c| c.records()).collect()
    }
    /// The subcommand of each policy lookup, in order.
    fn policy_commands(&self) -> Vec<&str> {
        self.policy_calls
            .iter()
            .map(|c| c.args.split_whitespace().next().unwrap())
            .collect()
    }
}

/// What a fake `tracevault` on `PATH` does.
#[derive(Clone, Copy, PartialEq, Debug)]
enum FakeTracevault {
    /// Log its arguments and stdin; a policy lookup prints the answer the
    /// test gave it (see [`replay_answering`]).
    Record,
    /// Like `Record`, but `stream` runs are slow: they log `begin` / `end`
    /// around a 50 ms sleep to `$TV_LOG.spans`, so overlapping runs show.
    Slow,
    /// Exit at once without reading stdin.
    ExitEarly,
    /// Read stdin, print an error on stderr and exit 3.
    Fail,
    /// Like `Record`, but never exit when the arguments contain this.
    HangOn(&'static str),
    /// Not on `PATH` at all.
    Missing,
}

fn replay(fixture: &str, fake: FakeTracevault) -> Option<Replay> {
    replay_with(fixture, fake, false, &[])
}

/// The plugin's timeouts in these tests (its defaults are 30 s, 10 s, 10 s).
const SPAWN_TIMEOUT_MS: &str = "1000";
const DISPOSE_WAIT_MS: &str = "1000";
const INJECT_TIMEOUT_MS: &str = "500";

/// `concurrent`: deliver events without waiting for the previous handler.
/// `env`: extra environment for the plugin.
fn replay_with(
    fixture: &str,
    fake: FakeTracevault,
    concurrent: bool,
    env: &[(&str, &str)],
) -> Option<Replay> {
    replay_answering(fixture, fake, concurrent, env, &[])
}

/// Policy lookups' answers: `(command, n, stdout)`.
type Answers<'a> = &'a [(&'a str, usize, &'a str)];

/// [`replay_with`], where the fake's policy lookups answer: `(command, n,
/// stdout)` makes the `n`th (from 1) `tracevault <command>` run print
/// `stdout`. Other runs print nothing.
fn replay_answering(
    fixture: &str,
    fake: FakeTracevault,
    concurrent: bool,
    env: &[(&str, &str)],
    answers: Answers,
) -> Option<Replay> {
    let node = node()?;
    let tmp = tempfile::tempdir().unwrap();
    let plugin = tmp.path().join("plugin.mts");
    std::fs::write(&plugin, AMP_PLUGIN_INDEX).unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let log = tmp.path().join("calls.log");
    for (command, n, stdout) in answers {
        std::fs::write(format!("{}.{command}.{n}", log.display()), stdout).unwrap();
    }
    // `stream` runs are logged to `$TV_LOG`, policy lookups to
    // `$TV_LOG.policy`: the two run side by side, and their lines must not
    // interleave.
    const LOG_TO: &str = "case \"$1\" in stream) L=\"$TV_LOG\";; *) L=\"$TV_LOG.policy\";; esac\n";
    const RECORD: &str = "printf '%s\\n' \"$*\" >> \"$L\"\ncat >> \"$L\"\necho >> \"$L\"\n";
    // A policy lookup prints its `n`th answer, if the test gave one. With
    // `$TV_LINGER` set it then keeps running that long, as `session-start`
    // does while it sends earlier sessions' cost reports. The command named
    // in `$TV_DELAY_CMD` waits `$TV_DELAY` before it answers.
    const ANSWER: &str = "case \"$1\" in session-start|user-prompt)\n\
        n=$(( $(cat \"$TV_LOG.$1.n\" 2>/dev/null || echo 0) + 1 ))\n\
        echo \"$n\" > \"$TV_LOG.$1.n\"\n\
        if [ \"$1\" = \"$TV_DELAY_CMD\" ]; then sleep \"$TV_DELAY\"; fi\n\
        if [ -f \"$TV_LOG.$1.$n\" ]; then cat \"$TV_LOG.$1.$n\"; fi\n\
        if [ -n \"$TV_LINGER\" ]; then sleep \"$TV_LINGER\"; fi\n\
        ;; esac\n";
    let script = match fake {
        FakeTracevault::Record => Some(format!("#!/bin/sh\n{LOG_TO}{RECORD}{ANSWER}")),
        FakeTracevault::HangOn(word) => Some(format!(
            "#!/bin/sh\n{LOG_TO}case \"$*\" in *'{word}'*) exec sleep 30;; esac\n{RECORD}{ANSWER}"
        )),
        FakeTracevault::Fail => Some(
            "#!/bin/sh\ncat > /dev/null\necho 'boom: corrupt .stream_offset' >&2\nexit 3\n"
                .to_string(),
        ),
        FakeTracevault::Slow => Some(format!(
            "#!/bin/sh\n{LOG_TO}\
             if [ \"$1\" = stream ]; then echo begin >> \"$TV_LOG.spans\"; sleep 0.05; fi\n\
             {RECORD}\
             if [ \"$1\" = stream ]; then echo end >> \"$TV_LOG.spans\"; fi\n\
             {ANSWER}"
        )),
        FakeTracevault::ExitEarly => Some("#!/bin/sh\nexit 1\n".to_string()),
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
        .env("TRACEVAULT_AMP_SPAWN_TIMEOUT_MS", SPAWN_TIMEOUT_MS)
        .env("TRACEVAULT_AMP_DISPOSE_WAIT_MS", DISPOSE_WAIT_MS)
        .env("TRACEVAULT_AMP_INJECT_TIMEOUT_MS", INJECT_TIMEOUT_MS)
        .env_remove("TV_LINGER")
        .env_remove("TV_DELAY_CMD")
        .envs(env.iter().copied())
        .output()
        .unwrap();
    let spans = std::fs::read_to_string(format!("{}.spans", log.display())).unwrap_or_default();
    let read_calls = |path: &Path| -> Vec<Call> {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        lines
            .chunks(2)
            .map(|pair| Call {
                args: pair[0].to_string(),
                payload: serde_json::from_str(pair[1]).unwrap(),
            })
            .collect()
    };
    Some(Replay {
        status: out.status,
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
        calls: read_calls(&log),
        policy_calls: read_calls(Path::new(&format!("{}.policy", log.display()))),
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
    let Some(r) = replay_with(fixture, FakeTracevault::Slow, true, &[]) else {
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
        .env_remove("AMP_CURRENT_THREAD_ID")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The real `tracevault session-start` reads the plugin's hook event and,
    // with no server to ask, answers with nothing to inject.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("unreadable answer"), "{stderr}");
    assert!(!stderr.contains("no answer after"), "{stderr}");
    let agent_starts: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["kind"] == "agent.start")
        .map(|v| v["result"].clone())
        .collect();
    assert_eq!(agent_starts, vec![serde_json::json!({})]);

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

fn dispose_result(r: &Replay) -> Value {
    r.stdout
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|v| v["kind"] == "dispose")
        .expect("onDispose ran")["result"]
        .clone()
}

/// `tracevault stream` reports its errors on stderr and exits 0, so a run's
/// stderr and a non-zero exit go to the plugin log.
#[test]
fn failing_tracevault_is_logged() {
    let Some(r) = replay("headless-multitool.plugin.jsonl", FakeTracevault::Fail) else {
        return;
    };
    assert!(r.status.success(), "stderr: {}", r.stderr);
    assert!(
        r.stderr
            .contains("tracevault stream --event notification --agent amp: exit 3: boom: corrupt .stream_offset"),
        "{}",
        r.stderr
    );
}

/// A run that never exits is killed after the spawn timeout, and the
/// thread's later sends still go out, in order.
#[test]
fn hung_run_is_killed_and_later_sends_go_on() {
    let fixture = "headless-multitool.plugin.jsonl";
    let Some(all) = replay(fixture, FakeTracevault::Record) else {
        return;
    };
    let Some(r) = replay(fixture, FakeTracevault::HangOn("--event notification")) else {
        return;
    };
    assert!(r.status.success(), "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains(&format!(
            "tracevault stream --event notification --agent amp: no exit after {SPAWN_TIMEOUT_MS} ms, killed"
        )),
        "{}",
        r.stderr
    );
    let expected: Vec<&str> = events(&all).into_iter().skip(1).collect();
    assert_eq!(events(&r), expected);
}

/// onDispose gives up on a send still running after its deadline, so a hung
/// `tracevault` cannot hold up Amp's exit; what ran before is sent.
#[test]
fn dispose_returns_at_its_deadline() {
    let Some(r) = replay_with(
        "headless-multitool.plugin.jsonl",
        FakeTracevault::HangOn("--event stop"),
        true,
        &[
            ("TRACEVAULT_AMP_DISPOSE_WAIT_MS", "200"),
            ("TRACEVAULT_AMP_SPAWN_TIMEOUT_MS", "3000"),
        ],
    ) else {
        return;
    };
    assert!(r.status.success(), "stderr: {}", r.stderr);
    let disposed = dispose_result(&r);
    let ms = disposed["ms"].as_u64().unwrap();
    // Well short of the 3 s it would take without the deadline.
    assert!((150..2000).contains(&ms), "onDispose took {ms} ms");
    assert!(!events(&r).contains(&"stop"));
    assert_eq!(disposed["logged"].as_u64(), Some(2 * r.calls.len() as u64));
}

// --- Agent policies (VIS-1199) ---

/// A `session-start` / `user-prompt` answer that injects `context`.
fn injecting(event: &str, context: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {"hookEventName": event, "additionalContext": context},
        "suppressOutput": true,
    })
    .to_string()
        + "\n"
}

/// What `session-start` / `user-prompt` print when there is nothing to inject.
const ALLOW: &str = "{\"suppressOutput\":true}\n";

/// `agent.start`'s return value that injects `context` hidden from the UI.
fn hidden(context: &str) -> Value {
    serde_json::json!({"message": {"content": context, "display": false}})
}

/// Two turns of one thread, in one Amp process.
const TWO_TURNS: &str = "interactive-cancel-ctrlc.plugin.jsonl";

fn agent_start_events(fixture: &str) -> Vec<Value> {
    fixture_lines(fixture)
        .into_iter()
        .filter(|l| l["kind"] == "agent.start")
        .map(|l| l["data"]["event"].clone())
        .collect()
}

fn prompts_sent(r: &Replay) -> usize {
    r.calls
        .iter()
        .filter(|c| c.event() == "user-prompt-submit")
        .count()
}

/// The first turn of a thread runs `tracevault session-start` with an
/// Amp-shaped hook event and returns its context hidden; a later turn runs
/// `tracevault user-prompt`, which injects nothing while the repo stays the
/// same.
#[test]
fn first_turn_injects_policies_and_same_repo_does_not_again() {
    let first = injecting("SessionStart", "POLICY: no rm -rf");
    let Some(r) = replay_answering(
        TWO_TURNS,
        FakeTracevault::Record,
        false,
        &[],
        &[("session-start", 1, &first), ("user-prompt", 1, ALLOW)],
    ) else {
        return;
    };
    assert!(r.status.success(), "stderr: {}", r.stderr);
    assert_eq!(
        r.agent_start_results(),
        vec![hidden("POLICY: no rm -rf"), serde_json::json!({})]
    );
    assert_eq!(r.policy_commands(), ["session-start", "user-prompt"]);

    // The CLI reads both as hook events, with the thread id as the session id.
    let starts = agent_start_events(TWO_TURNS);
    let thread = starts[0]["thread"]["id"].as_str().unwrap();
    for (call, hook) in r
        .policy_calls
        .iter()
        .zip(["SessionStart", "UserPromptSubmit"])
    {
        let event =
            tracevault_protocol::hooks::parse_hook_event(&call.payload.to_string()).unwrap();
        assert_eq!(event.session_id, thread);
        assert_eq!(event.hook_event_name, hook);
        assert_eq!(event.transcript_path, "");
        assert!(!event.cwd.is_empty());
    }
    assert_eq!(r.policy_calls[1].payload["prompt"], starts[1]["message"]);
    // The prompts are still sent, once each.
    assert_eq!(prompts_sent(&r), 2);
}

/// After the session's repo changed (`tracevault repo switch`), the next
/// turn's `user-prompt` answers with the new repo's policies, and the plugin
/// injects them.
#[test]
fn repo_change_injects_the_new_policies_on_the_next_turn() {
    let first = injecting("SessionStart", "POLICY A");
    let second = injecting("UserPromptSubmit", "POLICY B");
    let Some(r) = replay_answering(
        TWO_TURNS,
        FakeTracevault::Record,
        false,
        &[],
        &[("session-start", 1, &first), ("user-prompt", 1, &second)],
    ) else {
        return;
    };
    assert!(r.status.success(), "stderr: {}", r.stderr);
    assert_eq!(
        r.agent_start_results(),
        vec![hidden("POLICY A"), hidden("POLICY B")]
    );
}

/// No repo bound, or no policies: `session-start` prints no context and the
/// turn gets nothing. A `systemMessage` meant for the user goes to the log.
#[test]
fn nothing_to_inject_returns_nothing() {
    let notice = "{\"suppressOutput\":true,\"systemMessage\":\"repo is not registered\"}\n";
    let Some(r) = replay_answering(
        TWO_TURNS,
        FakeTracevault::Record,
        false,
        &[],
        &[("session-start", 1, notice)],
    ) else {
        return;
    };
    assert!(r.status.success(), "stderr: {}", r.stderr);
    assert_eq!(
        r.agent_start_results(),
        vec![serde_json::json!({}), serde_json::json!({})]
    );
    assert!(
        r.stderr
            .contains("tracevault session-start: repo is not registered"),
        "{}",
        r.stderr
    );
}

/// Only a thread's own turns get policies: a `Task` subagent runs inside its
/// parent's turn and fires no `agent.start` of its own.
#[test]
fn subagents_get_no_injection_of_their_own() {
    let fixture = "headless-multitool.plugin.jsonl";
    let first = injecting("SessionStart", "POLICY");
    let Some(r) = replay_answering(
        fixture,
        FakeTracevault::Record,
        false,
        &[],
        &[("session-start", 1, &first)],
    ) else {
        return;
    };
    assert!(r.status.success(), "stderr: {}", r.stderr);
    assert_eq!(agent_start_events(fixture).len(), 1);
    assert_eq!(r.policy_commands(), ["session-start"]);
    assert_eq!(r.agent_start_results(), vec![hidden("POLICY")]);
}

/// A missing, failing or babbling `tracevault` leaves the turn alone.
#[test]
fn failed_policy_lookup_injects_nothing() {
    let cases: [(FakeTracevault, Answers); 4] = [
        (FakeTracevault::Missing, &[]),
        (FakeTracevault::ExitEarly, &[]),
        (FakeTracevault::Fail, &[]),
        (
            FakeTracevault::Record,
            &[
                ("session-start", 1, "not json\n"),
                ("user-prompt", 1, "{\"hookSpecificOutput\": 42}\n"),
            ],
        ),
    ];
    for (fake, answers) in cases {
        let Some(r) = replay_answering(TWO_TURNS, fake, false, &[], answers) else {
            return;
        };
        assert!(r.status.success(), "{fake:?}: stderr: {}", r.stderr);
        assert_eq!(
            r.agent_start_results(),
            vec![serde_json::json!({}), serde_json::json!({})],
            "{fake:?}"
        );
        let expected = match fake {
            FakeTracevault::Fail => "tracevault session-start: exit 3: boom",
            FakeTracevault::Record => "tracevault session-start: unreadable answer: not json",
            _ => "spawn",
        };
        if fake != FakeTracevault::ExitEarly {
            assert!(r.stderr.contains(expected), "{fake:?}: {}", r.stderr);
        }
    }
}

/// A `session-start` that does not answer in time: the turn goes on without
/// policies after the inject timeout, its prompt is still sent, and the next
/// turn's lookup works.
#[test]
fn hung_policy_lookup_times_out_and_the_turn_goes_on() {
    let second = injecting("UserPromptSubmit", "POLICY B");
    let Some(r) = replay_answering(
        TWO_TURNS,
        FakeTracevault::HangOn("session-start"),
        false,
        &[],
        &[("user-prompt", 1, &second)],
    ) else {
        return;
    };
    assert!(r.status.success(), "stderr: {}", r.stderr);
    assert_eq!(
        r.agent_start_results(),
        vec![serde_json::json!({}), hidden("POLICY B")]
    );
    assert!(
        r.stderr.contains(&format!(
            "tracevault session-start: no answer after {INJECT_TIMEOUT_MS} ms"
        )),
        "{}",
        r.stderr
    );
    assert_eq!(prompts_sent(&r), 2);
}

/// `session-start` prints its answer and then sends earlier sessions' cost
/// reports: the turn takes the answer without waiting for the exit.
#[test]
fn answer_is_used_before_the_lookup_exits() {
    let first = injecting("SessionStart", "POLICY");
    let Some(r) = replay_answering(
        TWO_TURNS,
        FakeTracevault::Record,
        false,
        // Longer than the inject timeout, shorter than the spawn timeout.
        &[("TV_LINGER", "0.8")],
        &[("session-start", 1, &first), ("user-prompt", 1, ALLOW)],
    ) else {
        return;
    };
    assert!(r.status.success(), "stderr: {}", r.stderr);
    assert_eq!(r.agent_start_results()[0], hidden("POLICY"));
    assert!(!r.stderr.contains("no answer after"), "{}", r.stderr);
    assert!(!r.stderr.contains("killed"), "{}", r.stderr);
}

/// `session-start` answers after the turn gave up on it. The CLI has
/// recorded those policies as injected, so `user-prompt` will not offer them
/// again: the plugin injects them into the next turn instead.
#[test]
fn late_answer_goes_into_the_next_turn() {
    let first = injecting("SessionStart", "POLICY LATE");
    let Some(r) = replay_answering(
        TWO_TURNS,
        FakeTracevault::Record,
        false,
        // After the inject timeout, before the spawn timeout.
        &[
            ("TV_DELAY_CMD", "session-start"),
            ("TV_DELAY", "0.7"),
            // The next prompt comes after the late answer.
            ("REPLAY_TURN_GAP_MS", "500"),
        ],
        &[("session-start", 1, &first), ("user-prompt", 1, ALLOW)],
    ) else {
        return;
    };
    assert!(r.status.success(), "stderr: {}", r.stderr);
    assert_eq!(
        r.agent_start_results(),
        vec![serde_json::json!({}), hidden("POLICY LATE")]
    );
    assert!(r.stderr.contains("answered late"), "{}", r.stderr);
}

/// When `user-prompt` answers too, its policies (for the repo now in use)
/// win over a late answer.
#[test]
fn fresh_policies_win_over_a_late_answer() {
    let first = injecting("SessionStart", "POLICY LATE");
    let second = injecting("UserPromptSubmit", "POLICY NEW");
    let Some(r) = replay_answering(
        TWO_TURNS,
        FakeTracevault::Record,
        false,
        &[
            ("TV_DELAY_CMD", "session-start"),
            ("TV_DELAY", "0.7"),
            ("REPLAY_TURN_GAP_MS", "500"),
        ],
        &[("session-start", 1, &first), ("user-prompt", 1, &second)],
    ) else {
        return;
    };
    assert!(r.status.success(), "stderr: {}", r.stderr);
    assert_eq!(
        r.agent_start_results(),
        vec![serde_json::json!({}), hidden("POLICY NEW")]
    );
}

/// A hung `tracevault stream` holds up the turn no longer than the inject
/// timeout, though its own kill comes much later; the policies still arrive.
#[test]
fn slow_prompt_send_does_not_hold_up_the_turn() {
    let first = injecting("SessionStart", "POLICY");
    let Some(r) = replay_answering(
        TWO_TURNS,
        FakeTracevault::HangOn("--event user-prompt-submit"),
        false,
        &[("TRACEVAULT_AMP_SPAWN_TIMEOUT_MS", "3000")],
        &[("session-start", 1, &first), ("user-prompt", 1, ALLOW)],
    ) else {
        return;
    };
    assert!(r.status.success(), "stderr: {}", r.stderr);
    assert_eq!(r.agent_start_results()[0], hidden("POLICY"));
    let ms = r.handler_ms("agent.start");
    // The inject timeout is 500 ms; the hung send is killed after 3 s.
    assert!(ms[0] < 1500, "agent.start took {ms:?} ms");
}
