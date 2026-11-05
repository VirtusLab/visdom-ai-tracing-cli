# Amp plugin event fixtures

Real, sanitised Amp plugin event recordings, copied unchanged from the server repo
(`visdom-ai-tracing`, `crates/tracevault-core/tests/data/amp/`, VIS-1196). Its README there
explains how they were recorded (Amp CLI `0.0.1793822441-g8e158a`) and what they show.

Each line is `{seq, ts, pid, kind, data}`; `data.event` is Amp's own event payload.

| File | Covers |
| --- | --- |
| `headless-multitool.plugin.jsonl` | `amp -x`: `create_file`, a rejected shell command, `edit_file`, a failing command, a `Task` subagent, `git commit` |
| `headless-continue.plugin.jsonl` | `amp threads continue -x`: the plugin loaded twice, `edit_file` with `status: "error"`, `create_thread` / `wait_for_threads` / `read_thread` |
| `interactive-cancel-ctrlc.plugin.jsonl` | TUI: a completed turn, a turn cancelled during a shell call, Ctrl+C exit |
| `interactive-killed-terminal.plugin.jsonl` | TUI: terminal killed mid-turn, no `agent.end`, no `onDispose` |
| `headless-sigterm.plugin.jsonl` | `amp -x` stuck on an approval dialog, then SIGTERM |

`replay.mjs` feeds a fixture through the bundled plugin (`assets/amp-plugin/index.ts`) with a
stub Amp API; `tests/amp_stream_test.rs` runs it with a fake `tracevault` on `PATH`.
