/**
 * TraceVault pi/GSD extension.
 *
 * Streams pi session events to TraceVault by shelling out to the `tracevault`
 * CLI (which owns credentials, repo binding, retry, and pre-push `check`). This
 * extension carries NO credentials or config of its own.
 *
 * Transport model mirrors the Codex shell hooks: on each event we hand the CLI
 * a hook-event JSON on stdin whose `transcript_path` points at pi's live session
 * JSONL; the CLI forwards new lines by byte offset and the server's PiAdapter
 * parses them.
 */
import type { ExtensionAPI } from "@gsd/pi-coding-agent";
import { spawn } from "child_process";

/**
 * Per-session promise chain so at most one `tracevault` invocation runs at a
 * time for a given session. GSD has genuinely concurrent tools (async_bash,
 * bg_shell, await_job), so two `tool_execution_end` handlers can otherwise
 * spawn overlapping `tracevault stream` processes that race the same
 * `.stream_offset` file, causing duplicate or dropped forwarding. Enqueuing
 * every invocation onto this chain preserves per-session ordering without
 * blocking other sessions.
 */
const chains = new Map<string, Promise<void>>();

/** Append `task` to `sessionId`'s chain and await it, so calls for the same
 * session never overlap. Runs `task` even if a prior queued task rejected,
 * so one failure cannot wedge the chain for the rest of the session. */
function enqueue(sessionId: string, task: () => Promise<void>): Promise<void> {
  const prev = chains.get(sessionId) ?? Promise.resolve();
  const next = prev.then(task, task);
  chains.set(sessionId, next);
  return next;
}

/** Spawn `tracevault <args...>`, feeding `stdinJson` on stdin. Never throws. */
function runTracevault(args: string[], stdinJson: unknown, cwd: string): Promise<void> {
  return new Promise((resolve) => {
    try {
      const child = spawn("tracevault", args, { cwd, stdio: ["pipe", "ignore", "ignore"] });
      child.on("error", (e) => {
        console.error(`[tracevault] spawn failed: ${e instanceof Error ? e.message : String(e)}`);
        resolve();
      });
      child.on("close", () => resolve());
      child.stdin.write(JSON.stringify(stdinJson));
      child.stdin.end();
    } catch (e) {
      console.error(`[tracevault] ${e instanceof Error ? e.message : String(e)}`);
      resolve();
    }
  });
}

function hookEvent(
  sessionId: string,
  transcriptPath: string,
  cwd: string,
  name: string,
  toolName?: string,
  toolInput?: unknown,
) {
  return {
    session_id: sessionId,
    transcript_path: transcriptPath,
    cwd,
    hook_event_name: name,
    tool_name: toolName ?? null,
    tool_input: toolInput ?? null,
    tool_response: null,
    tool_use_id: null,
  };
}

export default function tracevault(pi: ExtensionAPI): void {
  pi.on("session_start", async (_event, ctx) => {
    const sessionId = ctx.sessionManager.getSessionId();
    if (!sessionId) return;
    const t = ctx.sessionManager.getSessionFile() ?? "";
    // Bare `session-start` (no --event/--agent flags) mirrors the Claude Code
    // and Codex SessionStart wiring (see `codex_hooks()` in init.rs). NOTE: its
    // side effects — exporting TRACEVAULT_SESSION_ID and injecting repo policy
    // context — are Claude/Codex-only (they need CLAUDE_ENV_FILE / a stdout the
    // agent reads, neither of which pi provides). For pi this call is effectively
    // inert; we keep it for parity. Capture does not depend on it — every stream
    // event below carries `session_id` explicitly.
    await enqueue(sessionId, () =>
      runTracevault(["session-start"], hookEvent(sessionId, t, ctx.cwd, "SessionStart"), ctx.cwd),
    );
  });

  pi.on("tool_execution_end", async (event, ctx) => {
    const sessionId = ctx.sessionManager.getSessionId();
    if (!sessionId) return;
    const t = ctx.sessionManager.getSessionFile();
    if (!t) return; // nothing to forward yet
    // Forward the tool arguments as `tool_input` so the server can use them —
    // e.g. software-usage extraction from a `bash` command's `command` arg.
    // Non-file tools (bash/read/…) are sourced from this hook event, so without
    // the args the server would have only the tool name. (write/edit hook
    // events are suppressed server-side in favour of the richer
    // transcript-sourced record, so their args here are ignored — harmless.)
    const toolInput = "args" in event ? (event as { args?: unknown }).args : undefined;
    await enqueue(sessionId, () =>
      runTracevault(
        ["stream", "--event", "post-tool-use", "--agent", "gsd"],
        hookEvent(sessionId, t, ctx.cwd, "PostToolUse", event.toolName, toolInput),
        ctx.cwd,
      ),
    );
  });

  pi.on("stop", async (_event, ctx) => {
    const sessionId = ctx.sessionManager.getSessionId();
    if (!sessionId) return;
    const t = ctx.sessionManager.getSessionFile() ?? "";
    // Best-effort settle: give pi a chance to flush the final assistant
    // message/usage to the session JSONL before we read it. pi may finish
    // writing the closing turn just after emitting `stop`, so this reduces —
    // but does not eliminate — the chance that turn is missed.
    await new Promise((r) => setTimeout(r, 100));
    await enqueue(sessionId, () =>
      runTracevault(
        ["stream", "--event", "stop", "--agent", "gsd"],
        hookEvent(sessionId, t, ctx.cwd, "Stop"),
        ctx.cwd,
      ),
    );
    // Clean up this session's chain entry now that stop has been enqueued and
    // awaited, so the map doesn't grow unboundedly across many sessions.
    chains.delete(sessionId);
  });

  console.error("[tracevault] pi/GSD extension loaded (streaming via tracevault CLI)");
}
