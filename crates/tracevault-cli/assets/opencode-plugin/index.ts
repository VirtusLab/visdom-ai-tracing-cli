/**
 * TraceVault OpenCode plugin.
 *
 * OpenCode has no shell-hook config and no single JSONL transcript, so this
 * plugin subscribes to OpenCode's plugin hooks, translates each event into
 * TraceVault's normalized record shape, and shells out to the `tracevault` CLI
 * with those records inline (fileless). The CLI owns credentials, repo binding,
 * retry, and offset tracking; this plugin carries no config of its own.
 */
import { spawn } from "node:child_process";

/** Per-session serialization so overlapping tool events don't race the CLI. */
const chains = new Map<string, Promise<void>>();
function enqueue(sessionId: string, task: () => Promise<void>): Promise<void> {
  const prev = chains.get(sessionId) ?? Promise.resolve();
  const next = prev.then(task, task);
  chains.set(sessionId, next);
  return next;
}

function runTracevault(args: string[], stdinJson: unknown, cwd: string): Promise<void> {
  return new Promise((resolve) => {
    try {
      const child = spawn("tracevault", args, { cwd, stdio: ["pipe", "ignore", "ignore"] });
      child.on("error", (e) => { console.error(`[tracevault] spawn: ${String(e)}`); resolve(); });
      child.on("close", () => resolve());
      child.stdin.write(JSON.stringify(stdinJson));
      child.stdin.end();
    } catch (e) { console.error(`[tracevault] ${String(e)}`); resolve(); }
  });
}

function hookEvent(
  sessionId: string, cwd: string, name: string,
  toolName: string | null, toolInput: unknown, records: unknown[],
) {
  return {
    session_id: sessionId,
    transcript_path: "",          // fileless — content rides in transcript_records
    cwd,
    hook_event_name: name,
    tool_name: toolName,
    tool_input: toolInput ?? null,
    tool_response: null,
    tool_use_id: null,
    transcript_records: records,
  };
}

// --- translation to the Record Schema (adjust reads to Part-0 payloads) ---
function isoNow(): string { return new Date().toISOString(); }

/** Build an assistant record from OpenCode's tool.execute.after event. */
function toolCallRecord(input: any, output: any): unknown {
  return {
    type: "message",
    timestamp: isoNow(),
    message: {
      role: "assistant",
      content: [
        { type: "toolCall", name: input.tool, arguments: input.args ?? output?.metadata ?? {} },
      ],
    },
  };
}

/** Build a user record from OpenCode's chat.message event. */
function userRecord(output: any): unknown {
  const text = (output?.parts ?? [])
    .filter((p: any) => p.type === "text")
    .map((p: any) => p.text).join("\n");
  return { type: "message", timestamp: isoNow(), message: { role: "user", content: [{ type: "text", text }] } };
}

/** Build an assistant usage record from an OpenCode assistant message payload. */
function assistantRecord(msg: any): unknown {
  const t = msg?.tokens ?? {};
  return {
    type: "message",
    timestamp: isoNow(),
    message: {
      role: "assistant",
      model: msg?.providerID && msg?.modelID ? `${msg.providerID}/${msg.modelID}` : (msg?.model ?? null),
      tokens: {
        input: t.input ?? 0, output: t.output ?? 0, reasoning: t.reasoning ?? 0,
        cacheRead: t.cache?.read ?? t.cacheRead ?? 0, cacheWrite: t.cache?.write ?? t.cacheWrite ?? 0,
      },
      content: [],
    },
  };
}

export default async function tracevault(ctx: any) {
  const cwd: string = ctx?.directory ?? ctx?.worktree ?? process.cwd();
  return {
    "chat.message": async (input: any, output: any) => {
      const sid = input?.sessionID; if (!sid) return;
      await enqueue(sid, () => runTracevault(
        ["stream", "--event", "user-prompt-submit", "--agent", "opencode"],
        hookEvent(sid, cwd, "UserPromptSubmit", null, null, [userRecord(output)]), cwd));
    },
    "tool.execute.after": async (input: any, output: any) => {
      const sid = input?.sessionID; if (!sid) return;
      await enqueue(sid, () => runTracevault(
        ["stream", "--event", "post-tool-use", "--agent", "opencode"],
        hookEvent(sid, cwd, "PostToolUse", input?.tool ?? null, input?.args ?? null,
          [toolCallRecord(input, output)]), cwd));
    },
    event: async ({ event }: { event: any }) => {
      const type = event?.type;
      if (type === "session.created") {
        const sid = event?.properties?.info?.id ?? event?.properties?.sessionID;
        if (!sid) return;
        await enqueue(sid, () => runTracevault(["session-start"],
          hookEvent(sid, cwd, "SessionStart", null, null, []), cwd));
      } else if (type === "message.updated" || type === "message.part.updated") {
        // Capture assistant token usage when a turn's message finalizes.
        const msg = event?.properties?.info ?? event?.properties?.message;
        const sid = msg?.sessionID; if (!sid || msg?.role !== "assistant" || !msg?.tokens) return;
        await enqueue(sid, () => runTracevault(
          ["stream", "--event", "post-tool-use", "--agent", "opencode"],
          hookEvent(sid, cwd, "PostToolUse", null, null, [assistantRecord(msg)]), cwd));
      } else if (type === "session.idle") {
        const sid = event?.properties?.sessionID; if (!sid) return;
        await enqueue(sid, () => runTracevault(["stream", "--event", "stop", "--agent", "opencode"],
          hookEvent(sid, cwd, "Stop", null, null, []), cwd));
        chains.delete(sid);
      }
    },
  };
}
