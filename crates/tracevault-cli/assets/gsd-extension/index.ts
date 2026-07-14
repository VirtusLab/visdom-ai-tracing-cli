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
import { existsSync, readdirSync } from "fs";
import { homedir } from "os";
import { join } from "path";

// pi session storage: GSD uses ~/.gsd/sessions; a bare pi client uses ~/.pi/sessions.
function sessionsRoots(): string[] {
  return [join(homedir(), ".gsd", "sessions"), join(homedir(), ".pi", "sessions")];
}

/** Locate the live session JSONL for `sessionId` (filename is `<ts>_<sessionId>.jsonl`). */
function findTranscript(sessionId: string): string | null {
  for (const root of sessionsRoots()) {
    if (!existsSync(root)) continue;
    for (const slug of readdirSync(root)) {
      const dir = join(root, slug);
      let files: string[];
      try {
        files = readdirSync(dir);
      } catch {
        continue;
      }
      const hit = files.find((f) => f.endsWith(`_${sessionId}.jsonl`));
      if (hit) return join(dir, hit);
    }
  }
  return null;
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
    const t = findTranscript(sessionId) ?? "";
    // Bare `session-start` (no --event/--agent flags) — mirrors the Claude
    // Code and Codex SessionStart hook wiring (see `gsd_hooks()` /
    // `codex_hooks()` in init.rs): it exports TRACEVAULT_SESSION_ID and
    // injects repo policy context. It is NOT `stream --event session-start`
    // — the `stream` subcommand's `--event` values are pre-tool-use /
    // post-tool-use / notification / stop only.
    await runTracevault(["session-start"], hookEvent(sessionId, t, ctx.cwd, "SessionStart"), ctx.cwd);
  });

  pi.on("tool_execution_end", async (event, ctx) => {
    const sessionId = ctx.sessionManager.getSessionId();
    if (!sessionId) return;
    const t = findTranscript(sessionId);
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
    const t = findTranscript(sessionId) ?? "";
    await runTracevault(
      ["stream", "--event", "stop", "--agent", "gsd"],
      hookEvent(sessionId, t, ctx.cwd, "Stop"),
      ctx.cwd,
    );
  });

  console.error("[tracevault] pi/GSD extension loaded (streaming via tracevault CLI)");
}
