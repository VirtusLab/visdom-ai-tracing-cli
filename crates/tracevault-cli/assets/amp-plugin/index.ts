/**
 * TraceVault Amp plugin, installed by `tracevault init --agent amp`.
 *
 * Amp has no shell hooks, only in-process plugins, so this plugin listens to
 * Amp's plugin events and shells out to `tracevault stream --agent amp` with
 * the session's records inline (`transcript_records`). The CLI owns
 * credentials, repo binding, retry and offsets; the plugin has no config.
 *
 * Records are Amp's own thread messages, unchanged, each wrapped in an
 * envelope the server's `amp` adapter reads:
 *
 *   {type: "amp_message", event, thread_id, timestamp, message}
 *
 * `event` is the Amp event that delivered the message. `message` is a
 * `{role, content[], ...}` thread message: `text`, `thinking`, `tool_use`
 * `{id, name, input}` and `tool_result` `{toolUseID, status, output?}` blocks,
 * and `usage` on assistant messages.
 *
 * When to send (from the VIS-1196 recordings):
 * - `agent.end` is the only complete record of a turn, but a killed terminal
 *   fires neither `agent.end` nor `onDispose`. So the prompt is sent on
 *   `agent.start` and every tool call on `tool.result`, as they happen, and
 *   `agent.end` then sends only what was not sent yet: the assistant
 *   messages (text, thinking, usage) and the tools that fire no tool events
 *   (`Task`, `create_thread`, `wait_for_threads`, `read_thread`). Messages
 *   are deduplicated on their id, tool calls on `toolUseID`.
 * - A `tool.result` whose `toolUseID` never shows up in the turn's messages is
 *   a call made by a `Task` subagent (it carries the parent's thread id). It
 *   is sent like any other.
 * - `session.start` fires again on `amp threads continue`; the CLI treats a
 *   repeated SessionStart like a resumed session.
 *
 * Nothing here may break the Amp session: a missing or failing `tracevault`
 * is logged and ignored, sends are serialised per thread, and `tool.call`
 * always allows the call.
 */
import type { PluginAPI } from '@ampcode/plugin'
import { spawn } from 'node:child_process'
import { fileURLToPath } from 'node:url'

export const description = 'TraceVault: sends this Amp session to TraceVault (tracevault stream --agent amp)'

/** A `tracevault` run that takes longer than this is killed: it must never hold up Amp. */
const SPAWN_TIMEOUT_MS = 30_000
/** How long `onDispose` waits for sends still in flight. */
const DISPOSE_WAIT_MS = 10_000

type Json = any

export default function tracevault(amp: PluginAPI) {
	const log = (msg: string) => {
		try {
			amp.logger.log(`[tracevault] ${msg}`)
		} catch {
			// Logging must not fail a hook either.
		}
	}
	const cwd = workspaceDir(amp)

	/** Per-thread chain, so one thread's `tracevault` runs never overlap. */
	const chains = new Map<string, Promise<void>>()
	function enqueue(threadId: string, task: () => Promise<void>): Promise<void> {
		const next = (chains.get(threadId) ?? Promise.resolve()).then(task, task)
		chains.set(threadId, next)
		// Compare-and-delete: a later enqueue may have replaced the entry.
		next.finally(() => {
			if (chains.get(threadId) === next) chains.delete(threadId)
		})
		return next
	}

	/** Threads this plugin process has sent a SessionStart for. */
	const started = new Set<string>()
	/** Per thread: message ids and toolUseIDs sent during the current turn. */
	const sent = new Map<string, Set<string>>()
	/** Per toolUseID: when `tool.call` fired, the tool call's own timestamp. */
	const callTimes = new Map<string, string>()

	function sentFor(threadId: string): Set<string> {
		let ids = sent.get(threadId)
		if (!ids) {
			ids = new Set()
			sent.set(threadId, ids)
		}
		return ids
	}

	function send(threadId: string, event: string, hookName: string, extra: Json, records: Json[]) {
		const payload = {
			session_id: threadId,
			transcript_path: '', // fileless: the records ride inline
			cwd,
			hook_event_name: hookName,
			tool_name: null,
			tool_input: null,
			tool_response: null,
			tool_use_id: null,
			...extra,
			transcript_records: records,
		}
		return enqueue(threadId, () => runTracevault(['stream', '--event', event, '--agent', 'amp'], payload, cwd, log))
	}

	/** Send a SessionStart once per thread per process, before anything else of that thread. */
	function ensureStarted(threadId: string): Promise<void> {
		if (started.has(threadId)) return Promise.resolve()
		started.add(threadId)
		return send(threadId, 'notification', 'SessionStart', {}, [])
	}

	amp.on('session.start', async (event) => {
		try {
			const threadId = event?.thread?.id
			if (threadId) await ensureStarted(threadId)
		} catch (e) {
			log(`session.start: ${String(e)}`)
		}
	})

	amp.on('agent.start', async (event) => {
		try {
			const threadId = event?.thread?.id
			if (!threadId) return {}
			// A plugin that loaded late may see the turn before any session.start.
			void ensureStarted(threadId)
			const message = {
				role: 'user',
				id: event.id,
				content: [{ type: 'text', text: event.message ?? '' }],
			}
			if (event.id != null) sentFor(threadId).add(String(event.id))
			await send(threadId, 'user-prompt-submit', 'UserPromptSubmit', {}, [
				envelope('agent.start', threadId, message, isoNow()),
			])
		} catch (e) {
			log(`agent.start: ${String(e)}`)
		}
		return {}
	})

	amp.on('tool.call', async (event) => {
		try {
			if (event?.toolUseID) callTimes.set(event.toolUseID, isoNow())
		} catch {
			// Never let bookkeeping get in the way of the tool call.
		}
		return { action: 'allow' }
	})

	amp.on('tool.result', async (event) => {
		try {
			const threadId = event?.thread?.id
			const toolUseID = event?.toolUseID
			if (!threadId || !toolUseID) return
			void ensureStarted(threadId)
			sentFor(threadId).add(toolUseID)
			const calledAt = callTimes.get(toolUseID) ?? isoNow()
			callTimes.delete(toolUseID)
			const result: Json = { type: 'tool_result', toolUseID, status: event.status }
			if ('output' in event) result.output = event.output
			if (event.error != null) result.error = event.error
			await send(
				threadId,
				'post-tool-use',
				'PostToolUse',
				{ tool_name: event.tool ?? null, tool_input: event.input ?? null, tool_use_id: toolUseID },
				[
					envelope(
						'tool.result',
						threadId,
						{
							role: 'assistant',
							content: [{ type: 'tool_use', id: toolUseID, name: event.tool, input: event.input ?? {} }],
						},
						calledAt,
					),
					envelope('tool.result', threadId, { role: 'user', content: [result] }, isoNow()),
				],
			)
		} catch (e) {
			log(`tool.result: ${String(e)}`)
		}
	})

	amp.on('agent.end', async (event) => {
		try {
			const threadId = event?.thread?.id
			if (!threadId) return
			void ensureStarted(threadId)
			const ids = sent.get(threadId) ?? new Set<string>()
			const records = unsentMessages(event.messages, ids).map((m) =>
				envelope('agent.end', threadId, m, typeof m.createdAt === 'string' ? m.createdAt : isoNow()),
			)
			// The turn is over: what it sent is no longer needed for dedup.
			sent.delete(threadId)
			for (const m of event.messages ?? []) {
				for (const b of Array.isArray(m?.content) ? m.content : []) {
					if (b?.type === 'tool_use' && typeof b.id === 'string') callTimes.delete(b.id)
				}
			}
			if (records.length > 0) send(threadId, 'post-tool-use', 'PostToolUse', {}, records)
			await send(threadId, 'stop', 'Stop', {}, [])
		} catch (e) {
			log(`agent.end: ${String(e)}`)
		}
	})

	amp.onDispose(async () => {
		// `amp -x` ends with onDispose; let sends still in flight finish.
		const pending = [...chains.values()]
		if (pending.length === 0) return
		await Promise.race([Promise.allSettled(pending), new Promise((r) => setTimeout(r, DISPOSE_WAIT_MS))])
	})
}

/**
 * The turn's messages minus what this plugin already sent: the prompt (sent
 * on `agent.start`, matched by message id) and the tool calls and results
 * sent on `tool.result` (matched by `toolUseID`). An assistant message is
 * always kept, for its text, thinking and `usage`; any other message left
 * with no content is dropped.
 */
function unsentMessages(messages: Json[] | undefined, sentIds: Set<string>): Json[] {
	const out: Json[] = []
	for (const m of Array.isArray(messages) ? messages : []) {
		if (!m || typeof m !== 'object') continue
		if (m.id != null && sentIds.has(String(m.id))) continue
		if (!Array.isArray(m.content)) {
			out.push(m)
			continue
		}
		const content = m.content.filter(
			(b: Json) =>
				!(b?.type === 'tool_use' && sentIds.has(b.id)) && !(b?.type === 'tool_result' && sentIds.has(b.toolUseID)),
		)
		if (content.length === 0 && m.role !== 'assistant') continue
		out.push(content.length === m.content.length ? m : { ...m, content })
	}
	return out
}

function envelope(event: string, threadId: string, message: Json, timestamp: string): Json {
	return { type: 'amp_message', event, thread_id: threadId, timestamp, message }
}

function isoNow(): string {
	return new Date().toISOString()
}

/** The workspace Amp runs in, as a directory path; the process cwd when Amp names none. */
function workspaceDir(amp: PluginAPI): string {
	try {
		const root = amp.system?.workspaceRoot?.toString()
		if (root && root.startsWith('file:')) return fileURLToPath(root)
	} catch {
		// Fall through.
	}
	return process.cwd()
}

/**
 * Run `tracevault <args>` with `stdinJson` on stdin. Always resolves: a
 * missing binary, a broken pipe, a crash or a hang only gets logged.
 */
function runTracevault(args: string[], stdinJson: unknown, cwd: string, log: (msg: string) => void): Promise<void> {
	return new Promise((resolve) => {
		let done = false
		const finish = () => {
			if (!done) {
				done = true
				resolve()
			}
		}
		try {
			const child = spawn('tracevault', args, { cwd, stdio: ['pipe', 'ignore', 'ignore'] })
			const timer = setTimeout(() => {
				log(`tracevault ${args.join(' ')}: no exit after ${SPAWN_TIMEOUT_MS} ms, killed`)
				try {
					child.kill()
				} catch {
					// Already gone.
				}
				finish()
			}, SPAWN_TIMEOUT_MS)
			child.on('error', (e) => {
				clearTimeout(timer)
				log(`spawn: ${String(e)}`)
				finish()
			})
			child.on('close', () => {
				clearTimeout(timer)
				finish()
			})
			// The stdin stream emits its own 'error' (EPIPE) when tracevault exits
			// early or is not on PATH; unhandled, it would crash the plugin host.
			child.stdin?.on('error', (e) => log(`stdin: ${String(e)}`))
			child.stdin?.end(JSON.stringify(stdinJson))
		} catch (e) {
			log(String(e))
			finish()
		}
	})
}
