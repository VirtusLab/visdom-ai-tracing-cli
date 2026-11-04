// Replays a recorded Amp plugin session through the TraceVault Amp plugin, for
// tests/amp_stream_test.rs. Usage: node replay.mjs <plugin.mts> <fixture.jsonl>
//
// Each process recorded in the fixture (its `pid`) gets a fresh plugin
// instance, as Amp loads one per process. Events are delivered in the
// recorded order, one at a time, awaiting the handler as Amp does; a
// `dispose` line runs the onDispose callbacks. A process that ends without
// one (a killed terminal) just stops. Prints one JSON line per handler call:
// `{kind, result}`, so the test can check what `tool.call` returned.
import { readFileSync } from 'node:fs'
import { pathToFileURL } from 'node:url'

const [pluginPath, fixturePath] = process.argv.slice(2)
const lines = readFileSync(fixturePath, 'utf8')
	.split('\n')
	.filter((l) => l.trim())
	.map((l) => JSON.parse(l))

const byPid = new Map()
for (const line of lines) {
	if (!byPid.has(line.pid)) byPid.set(line.pid, [])
	byPid.get(line.pid).push(line)
}

const EVENTS = new Set(['session.start', 'agent.start', 'tool.call', 'tool.result', 'agent.end'])

for (const [pid, procLines] of byPid) {
	const handlers = new Map()
	const disposers = []
	const amp = {
		logger: { log: (...args) => console.error(...args) },
		system: { workspaceRoot: { toString: () => pathToFileURL(process.cwd()).toString() } },
		on(event, handler) {
			if (!handlers.has(event)) handlers.set(event, [])
			handlers.get(event).push(handler)
			return { unsubscribe() {} }
		},
		onDispose(cb) {
			disposers.push(cb)
			return { unsubscribe() {} }
		},
	}
	const mod = await import(`${pathToFileURL(pluginPath)}?pid=${pid}`)
	mod.default(amp)
	for (const line of procLines) {
		if (EVENTS.has(line.kind)) {
			for (const handler of handlers.get(line.kind) ?? []) {
				const result = await handler(line.data.event, {})
				console.log(JSON.stringify({ kind: line.kind, result: result ?? null }))
			}
		} else if (line.kind === 'dispose') {
			for (const cb of disposers) await cb()
		}
	}
}
