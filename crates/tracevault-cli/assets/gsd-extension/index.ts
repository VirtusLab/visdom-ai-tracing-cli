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

function hookEvent(sessionId: string, transcriptPath: string, cwd: string, name: string, toolName?: string) {
  return {
    session_id: sessionId,
    transcript_path: transcriptPath,
    cwd,
    hook_event_name: name,
    tool_name: toolName ?? null,
    tool_input: null,
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
    await runTracevault(["session-start"], hookEvent(sessionId, t, ctx.cwd, "SessionStart"), ctx.cwd);
  });

  pi.on("tool_execution_end", async (event, ctx) => {
    const sessionId = ctx.sessionManager.getSessionId();
    if (!sessionId) return;
    const t = ctx.sessionManager.getSessionFile();
    if (!t) return; // nothing to forward yet
    await runTracevault(
      ["stream", "--event", "post-tool-use", "--agent", "gsd"],
      hookEvent(sessionId, t, ctx.cwd, "PostToolUse", event.toolName),
      ctx.cwd,
    );
  });

  pi.on("stop", async (_event, ctx) => {
    const sessionId = ctx.sessionManager.getSessionId();
    if (!sessionId) return;
    const t = ctx.sessionManager.getSessionFile() ?? "";
    await runTracevault(
      ["stream", "--event", "stop", "--agent", "gsd"],
      hookEvent(sessionId, t, ctx.cwd, "Stop"),
      ctx.cwd,
    );
  });

  console.error("[tracevault] pi/GSD extension loaded (streaming via tracevault CLI)");
}
