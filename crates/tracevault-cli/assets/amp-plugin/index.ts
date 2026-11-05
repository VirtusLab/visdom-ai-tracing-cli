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
 *   (`Task`, `create_thread`, `wait_for_threads`, `read_thread`), and any
 *   context another plugin appended to the prompt. Tool calls are
 *   deduplicated on `toolUseID`, the prompt's text on its message id.
 * - A `tool.result` whose `toolUseID` never shows up in the turn's messages is
 *   a call made by a `Task` subagent (it carries the parent's thread id). It
 *   is sent like any other.
 * - `session.start` fires again on `amp threads continue`; the CLI treats a
 *   repeated SessionStart like a resumed session.
 *
 * Agent policies (VIS-1199): `agent.start` may return a message, which Amp
 * appends hidden to the user's turn. On the first `agent.start` of a thread
 * in this process the plugin runs `tracevault session-start` (which also binds
 * the session to the checkout's repo), on later ones `tracevault user-prompt`
 * (which answers only when the session's repo changed), and returns their
 * `hookSpecificOutput.additionalContext` with `display: false`. `session.start`
 * cannot inject: Amp ignores what it returns. Amp stores the injected text in
 * the thread, as a second text block of the user's message, so anyone who
 * opens the thread on ampcode.com or exports it sees it. A `Task` subagent
 * runs inside its parent's turn and fires no `agent.start`, so it gets no
 * injection of its own.
 *
 * The plugin cannot export `TRACEVAULT_SESSION_ID` into Amp's shell (it runs
 * in another process); the CLI falls back to the shell's
 * `AMP_CURRENT_THREAD_ID`, the thread id this plugin uses as the session id.
 *
 * Nothing here may break the Amp session: a missing or failing `tracevault`
 * is logged and ignored, sends are serialised per thread, the policy lookup
 * is bounded by a timeout after which the turn goes on without policies, and
 * `tool.call` always allows the call.
 */
import type { PluginAPI } from '@ampcode/plugin'
import { spawn } from 'node:child_process'
import { fileURLToPath } from 'node:url'

export const description = 'TraceVault: sends this Amp session to TraceVault (tracevault stream --agent amp)'

/**
 * A `tracevault` run that takes longer than this is killed: it must never hold
 * up Amp. The CLI gives up delivering after about 7 s and queues the rest.
 */
const SPAWN_TIMEOUT_MS = envMs('TRACEVAULT_AMP_SPAWN_TIMEOUT_MS', 30_000)
/** How long `onDispose` waits for sends still in flight. */
const DISPOSE_WAIT_MS = envMs('TRACEVAULT_AMP_DISPOSE_WAIT_MS', 10_000)
/**
 * How long `agent.start` waits for the policies before the turn goes on
 * without them. The CLI keeps its own network work under 8 s.
 */
const INJECT_TIMEOUT_MS = envMs('TRACEVAULT_AMP_INJECT_TIMEOUT_MS', 10_000)
/** How much of a failing run's stderr goes to the plugin log. */
const STDERR_LIMIT = 2_000
/** How much of a policy run's stdout is read; the CLI caps the context at 10k characters. */
const STDOUT_LIMIT = 1_000_000

/** A positive number of milliseconds from the environment (for tests), else `fallback`. */
function envMs(name: string, fallback: number): number {
	const value = Number(process.env[name])
	return Number.isFinite(value) && value > 0 ? value : fallback
}

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
	/** Per thread: what the current turn has sent so far. */
	const sent = new Map<string, Sent>()
	/** Per toolUseID: when `tool.call` fired, the tool call's own timestamp. */
	const callTimes = new Map<string, string>()

	function sentFor(threadId: string): Sent {
		let s = sent.get(threadId)
		if (!s) {
			s = { tools: new Set(), prompts: new Map() }
			sent.set(threadId, s)
		}
		return s
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

	/** Threads this plugin process has run `tracevault session-start` for. */
	const policiesStarted = new Set<string>()
	/**
	 * Policy runs not finished yet. `session-start` answers first and then
	 * sends earlier sessions' cost reports, so `onDispose` waits for these too.
	 */
	const policyRuns = new Set<Promise<void>>()

	/**
	 * The policies to inject into this turn of `threadId`, or `undefined`:
	 * `tracevault session-start` on the thread's first turn in this process,
	 * `tracevault user-prompt` (which answers only when the repo changed) on
	 * later ones. Never rejects, and gives up after `INJECT_TIMEOUT_MS`.
	 */
	function policyContext(threadId: string, prompt: string): Promise<string | undefined> {
		const first = !policiesStarted.has(threadId)
		policiesStarted.add(threadId)
		const [command, hookName] = first ? ['session-start', 'SessionStart'] : ['user-prompt', 'UserPromptSubmit']
		const payload: Json = {
			session_id: threadId,
			transcript_path: '',
			cwd,
			hook_event_name: hookName,
		}
		if (!first) payload.prompt = prompt
		return new Promise((resolve) => {
			let stdout = ''
			let settled = false
			const settle = (text: string | undefined) => {
				if (settled) return
				settled = true
				clearTimeout(timer)
				resolve(hookContext(text, command, log))
			}
			const timer = setTimeout(() => {
				log(`tracevault ${command}: no answer after ${INJECT_TIMEOUT_MS} ms, turn goes on without policies`)
				settle(undefined)
			}, INJECT_TIMEOUT_MS)
			const run = runTracevault([command], payload, cwd, log, (chunk) => {
				if (stdout.length < STDOUT_LIMIT) stdout += chunk
				// The answer is one JSON line; what the command does after it
				// (the cost reports) need not hold up the turn.
				const end = stdout.indexOf('\n')
				if (end >= 0) settle(stdout.slice(0, end))
			}).then(() => settle(stdout))
			policyRuns.add(run)
			run.finally(() => policyRuns.delete(run))
		})
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
		let policies: string | undefined
		try {
			const threadId = event?.thread?.id
			if (!threadId) return {}
			// A plugin that loaded late may see the turn before any session.start.
			void ensureStarted(threadId)
			const prompt = event.message ?? ''
			const message = {
				role: 'user',
				id: event.id,
				content: [{ type: 'text', text: prompt }],
			}
			if (event.id != null) sentFor(threadId).prompts.set(String(event.id), prompt)
			const [context] = await Promise.all([
				policyContext(threadId, prompt),
				send(threadId, 'user-prompt-submit', 'UserPromptSubmit', {}, [
					envelope('agent.start', threadId, message, isoNow()),
				]),
			])
			policies = context
		} catch (e) {
			log(`agent.start: ${String(e)}`)
		}
		// Hidden in Amp's UI, but stored in the thread (see the top of this file).
		return policies ? { message: { content: policies, display: false } } : {}
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
			sentFor(threadId).tools.add(toolUseID)
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
			const records = unsentMessages(event.messages, sentFor(threadId)).map((m) =>
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
		const pending = [...chains.values(), ...policyRuns]
		if (pending.length === 0) return
		let timer: ReturnType<typeof setTimeout> | undefined
		await Promise.race([
			Promise.allSettled(pending),
			new Promise((r) => {
				timer = setTimeout(r, DISPOSE_WAIT_MS)
			}),
		])
		// A pending timer would keep the plugin process alive.
		clearTimeout(timer)
	})
}

/** What one thread's current turn has sent: tool calls by `toolUseID`, prompts by message id. */
type Sent = { tools: Set<string>; prompts: Map<string, string> }

/**
 * The turn's messages minus what this plugin already sent: the prompt's text
 * (sent on `agent.start`) and the tool calls and results sent on
 * `tool.result` (matched by `toolUseID`). The rest of the prompt's message is
 * kept: Amp appends context a plugin adds on `agent.start` to it as further
 * text blocks. An assistant message is always kept, for its text, thinking
 * and `usage`; any other message left with no content is dropped.
 */
function unsentMessages(messages: Json[] | undefined, sent: Sent): Json[] {
	const out: Json[] = []
	for (const m of Array.isArray(messages) ? messages : []) {
		if (!m || typeof m !== 'object') continue
		if (!Array.isArray(m.content)) {
			if (!(m.id != null && sent.prompts.has(String(m.id)))) out.push(m)
			continue
		}
		let prompt = m.id != null ? sent.prompts.get(String(m.id)) : undefined
		const content = m.content.filter((b: Json) => {
			if (prompt !== undefined && b?.type === 'text' && b.text === prompt) {
				prompt = undefined // only the one block agent.start sent
				return false
			}
			if (b?.type === 'tool_use') return !sent.tools.has(b.id)
			if (b?.type === 'tool_result') return !sent.tools.has(b.toolUseID)
			return true
		})
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
 * The `additionalContext` of a Claude Code-style hook answer
 * (`{hookSpecificOutput: {additionalContext}}`), or `undefined` when there is
 * none or `text` is not such an answer. A `systemMessage` (a notice meant for
 * the user, such as "this repo is not registered") goes to the plugin log:
 * a plugin has no place to show it.
 */
function hookContext(text: string | undefined, command: string, log: (msg: string) => void): string | undefined {
	if (!text?.trim()) return undefined
	try {
		const answer = JSON.parse(text)
		if (typeof answer?.systemMessage === 'string' && answer.systemMessage) {
			log(`tracevault ${command}: ${answer.systemMessage}`)
		}
		const context = answer?.hookSpecificOutput?.additionalContext
		return typeof context === 'string' && context.trim() ? context : undefined
	} catch {
		log(`tracevault ${command}: unreadable answer: ${text.slice(0, 200)}`)
		return undefined
	}
}

/**
 * Run `tracevault <args>` with `stdinJson` on stdin. Always resolves: a
 * missing binary, a broken pipe, a crash or a hang only gets logged. Its
 * stdout goes to `onStdout` when given, else nowhere.
 */
function runTracevault(
	args: string[],
	stdinJson: unknown,
	cwd: string,
	log: (msg: string) => void,
	onStdout?: (chunk: string) => void,
): Promise<void> {
	return new Promise((resolve) => {
		let done = false
		const finish = () => {
			if (!done) {
				done = true
				resolve()
			}
		}
		const what = `tracevault ${args.join(' ')}`
		try {
			const child = spawn('tracevault', args, { cwd, stdio: ['pipe', onStdout ? 'pipe' : 'ignore', 'pipe'] })
			child.stdout?.setEncoding('utf8')
			child.stdout?.on('data', (chunk) => {
				try {
					onStdout?.(String(chunk))
				} catch (e) {
					log(`${what}: ${String(e)}`)
				}
			})
			// `tracevault stream` reports its errors on stderr and still exits 0,
			// so keep the start of stderr for the log.
			let stderr = ''
			child.stderr?.on('data', (chunk) => {
				if (stderr.length < STDERR_LIMIT) stderr += String(chunk)
			})
			const timer = setTimeout(() => {
				log(`${what}: no exit after ${SPAWN_TIMEOUT_MS} ms, killed`)
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
			child.on('close', (code, signal) => {
				clearTimeout(timer)
				const text = stderr.slice(0, STDERR_LIMIT).trim()
				if (code !== 0 || signal) log(`${what}: exit ${signal ?? code}${text ? `: ${text}` : ''}`)
				else if (text) log(`${what}: ${text}`)
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
