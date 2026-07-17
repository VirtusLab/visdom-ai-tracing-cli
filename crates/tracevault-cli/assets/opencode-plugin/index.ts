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
  // Self-clean once this task settles, but ONLY if it's still the tail — a
  // later enqueue may have replaced the entry while this task ran. Compare-and-
  // delete keeps the map bounded without the race of an unconditional delete
  // (which could drop a newer chain and let two `tracevault` spawns run
  // concurrently for the same session).
  next.finally(() => {
    if (chains.get(sessionId) === next) chains.delete(sessionId);
  });
  return next;
}

function runTracevault(args: string[], stdinJson: unknown, cwd: string): Promise<void> {
  return new Promise((resolve) => {
    try {
      const child = spawn("tracevault", args, { cwd, stdio: ["pipe", "ignore", "ignore"] });
      child.on("error", (e) => { console.error(`[tracevault] spawn: ${String(e)}`); resolve(); });
      child.on("close", () => resolve());
      // The stdin stream can emit its own async 'error' (EPIPE / write-after-end
      // if tracevault exits early or isn't on PATH). Without a listener Node
      // turns that into an uncaughtException in OpenCode's plugin host, crashing
      // the user's session — so swallow it and let 'close'/'error' resolve.
      child.stdin.on("error", (e) => { console.error(`[tracevault] stdin: ${String(e)}`); });
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

/** Build an assistant record (the tool CALL) from OpenCode's tool.execute.after event. */
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

/**
 * Build a `toolResult` record (the tool OUTPUT) from tool.execute.after, so the
 * session timeline shows bash stdout / file-read contents, not just the call.
 * Matches the schema the server adapter's toolResult branch parses (`toolName`,
 * `isError`, `content[]`).
 */
function toolResultRecord(input: any, output: any): unknown {
  // Most tools put their result string in `output.output`; for a tool whose
  // result is structured (non-string), JSON-stringify it so the body isn't lost
  // rather than dropped to "".
  const raw = output?.output;
  const text =
    typeof raw === "string" ? raw : raw == null ? "" : JSON.stringify(raw);
  const isError = Boolean(output?.metadata?.error) || output?.isError === true;
  return {
    type: "message",
    timestamp: isoNow(),
    message: {
      role: "toolResult",
      toolName: input?.tool ?? null,
      isError,
      content: [{ type: "text", text }],
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

/**
 * Accumulated assistant text parts, keyed by messageID → partID → latest text.
 * OpenCode delivers assistant prose via `message.part.updated` (type "text")
 * events; the finalized `message.updated` carries only metadata (tokens/model),
 * no body. We collect the parts here and attach them to the assistant record on
 * finish, then drop the entry. Keyed by partID so streaming updates overwrite
 * (each carries the full current text) rather than duplicate.
 */
const assistantText = new Map<string, Map<string, string>>();

/** Ordered, non-empty text-part strings accumulated for a message id. */
function collectedText(messageID: string): string[] {
  const parts = assistantText.get(messageID);
  if (!parts) return [];
  return [...parts.values()].filter((t) => t.length > 0);
}

/**
 * Build the finalized assistant record: prose (its text parts) plus token/model
 * usage. `texts` are the accumulated `message.part.updated` bodies.
 */
function assistantRecord(msg: any, texts: string[]): unknown {
  const t = msg?.tokens ?? {};
  return {
    type: "message",
    timestamp: isoNow(),
    message: {
      role: "assistant",
      // Forward OpenCode's stable, globally-unique message id so the server
      // dedups token usage on it (keyed by message.id) instead of a per-chunk
      // synthetic fallback — robust across retries and multiple sends.
      id: msg?.id ?? null,
      model: msg?.providerID && msg?.modelID ? `${msg.providerID}/${msg.modelID}` : (msg?.model ?? null),
      tokens: {
        input: t.input ?? 0, output: t.output ?? 0, reasoning: t.reasoning ?? 0,
        cacheRead: t.cache?.read ?? t.cacheRead ?? 0, cacheWrite: t.cache?.write ?? t.cacheWrite ?? 0,
      },
      content: texts.map((text) => ({ type: "text", text })),
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
          [toolCallRecord(input, output), toolResultRecord(input, output)]), cwd));
    },
    event: async ({ event }: { event: any }) => {
      const type = event?.type;
      if (type === "session.created") {
        const sid = event?.properties?.info?.id ?? event?.properties?.sessionID;
        if (!sid) return;
        await enqueue(sid, () => runTracevault(["session-start"],
          hookEvent(sid, cwd, "SessionStart", null, null, []), cwd));
      } else if (type === "message.part.updated") {
        // Accumulate assistant prose as it streams; forwarded on finish below.
        const part = event?.properties?.part;
        if (
          part?.type === "text" &&
          typeof part?.text === "string" &&
          typeof part?.messageID === "string" &&
          typeof part?.id === "string"
        ) {
          let parts = assistantText.get(part.messageID);
          if (!parts) { parts = new Map(); assistantText.set(part.messageID, parts); }
          parts.set(part.id, part.text);
        }
      } else if (type === "message.updated") {
        // Capture the finalized assistant turn — its prose plus token/model
        // usage — ONLY once it's complete (carries `finish`). OpenCode emits
        // many `message.updated` events per turn as tokens stream in; gating on
        // `finish` avoids spawning the CLI on every intermediate delta.
        const msg = event?.properties?.info;
        const sid = msg?.sessionID;
        if (!sid || msg?.role !== "assistant" || !msg?.finish || !msg?.tokens) return;
        const texts = collectedText(msg.id);
        assistantText.delete(msg.id);
        await enqueue(sid, () => runTracevault(
          ["stream", "--event", "post-tool-use", "--agent", "opencode"],
          hookEvent(sid, cwd, "PostToolUse", null, null, [assistantRecord(msg, texts)]), cwd));
      } else if (type === "session.idle") {
        const sid = event?.properties?.sessionID; if (!sid) return;
        // No explicit chains.delete here — enqueue() self-cleans the entry once
        // this task settles (compare-and-delete), which is race-safe against a
        // late event that enqueues during the stop await.
        await enqueue(sid, () => runTracevault(["stream", "--event", "stop", "--agent", "opencode"],
          hookEvent(sid, cwd, "Stop", null, null, []), cwd));
      }
    },
  };
}
