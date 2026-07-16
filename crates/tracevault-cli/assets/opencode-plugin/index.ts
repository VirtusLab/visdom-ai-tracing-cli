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

// --- translation to the Record Schema (field paths confirmed against a real
// OpenCode 1.18 session) ---
function isoNow(): string { return new Date().toISOString(); }

/**
 * Map OpenCode's tool-arg field names onto the names the server's OpenCodeAdapter
 * reads (which mirror pi's: `path` / `oldText` / `newText`). OpenCode's `write`
 * uses `{filePath, content}` and `edit` uses `{filePath, oldString, newString}`,
 * so file-change extraction only works if we rename them here. Other tools
 * (bash/read/…) are passed through unchanged — the adapter ignores them for
 * file changes and renders the raw args for display.
 */
function translateToolArgs(tool: string, args: any): any {
  if (!args || typeof args !== "object") return {};
  if (tool === "write") {
    return { path: args.filePath ?? args.path, content: args.content };
  }
  if (tool === "edit") {
    return {
      path: args.filePath ?? args.path,
      oldText: args.oldString ?? args.oldText,
      newText: args.newString ?? args.newText,
    };
  }
  return args;
}

/** Build an assistant record from OpenCode's tool.execute.after event. */
function toolCallRecord(input: any, _output: any): unknown {
  return {
    type: "message",
    timestamp: isoNow(),
    message: {
      role: "assistant",
      content: [
        { type: "toolCall", name: input.tool, arguments: translateToolArgs(input.tool, input.args) },
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
      } else if (type === "message.updated") {
        // Capture assistant token/model usage, but ONLY for the finalized
        // message. OpenCode emits many `message.updated` events per turn as
        // token counts stream in; the completed one carries a `finish` field.
        // Gating on it avoids spawning the CLI on every intermediate delta.
        const msg = event?.properties?.info;
        const sid = msg?.sessionID;
        if (!sid || msg?.role !== "assistant" || !msg?.finish || !msg?.tokens) return;
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
