import assert from "node:assert/strict";
import { readFile, writeFile } from "node:fs/promises";
import { AgentSession } from "../../../provider-references/integrated/node_modules/@earendil-works/pi-coding-agent/dist/core/agent-session.js";
const version = JSON.parse(await readFile(new URL("../../../provider-references/integrated/node_modules/@earendil-works/pi-coding-agent/package.json", import.meta.url))).version;
assert.equal(version, "1.3.0");
const events = [], requests = [];
const session = Object.assign(Object.create(AgentSession.prototype), {
	agent: { state: { model: { id: "old", provider: "fixture", api: "fixture" }, tools: [], thinkingLevel: "off" } },
	sessionManager: { buildSessionProjection: () => ({ messages: [] }) },
	_boundaryDispatchedMessages: new WeakSet(),
	_findPersistedMessageEntryId: () => "committed-fixture",
	_commitBoundaryDrafts: () => {},
	_extensionRunner: { hasHandlers: () => true, emit: async event => {
		await Promise.resolve();
		if (event.type === "turn_start") events.push(event);
	}, emitBoundary: async event => {
		events.push({ type: event.type, turnIndex: event.turnIndex });
		return { entries: [], continue: false };
	} },
});
session._installAgentRequestProjection();
const clock = Date.now;
try {
	Date.now = () => 1000;
	for (const count of [3, 1]) {
		await session._emitExtensionEvent({ type: "agent_start" });
		for (let index = 0; index < count; index++) {
			await session._emitExtensionEvent({ type: "turn_start" });
			session.agent.state = { model: { id: `selected-${index}`, api: "fixture", provider: "fixture" },
				tools: index === 0 ? [{ name: "write" }] : [], thinkingLevel: index === 0 ? "high" : "off" };
			const prepared = await session.agent.prepareRequest({ context: { messages: [], tools: [{ name: "stale" }] },
				model: { id: "old", provider: "fixture" }, thinkingLevel: "off" }, new AbortController().signal);
			requests.push({ model: prepared.model.id, tools: prepared.context.tools.map(tool => tool.name), thinking: prepared.thinkingLevel });
			await session._emitExtensionEvent({ type: "turn_end", message: { role: "assistant", stopReason: "stop" }, toolResults: [] });
		}
	}
} finally { Date.now = clock; }
assert.deepEqual(events.filter(event => event.type === "turn_start").map(event => event.turnIndex), [0, 1, 2, 0]);
const text = JSON.stringify({ reference: "Pi 1.3.0", projection: "Live request binding and turn index/timestamp fields; committed turn-end entry IDs and boundary drafts are separate contracts", events, requests }, null, 2) + "\n";
const destination = new URL("turn-reference.json", import.meta.url);
if (process.argv.includes("--write")) await writeFile(destination, text);
else assert.equal(await readFile(destination, "utf8"), text);
process.stdout.write(`${events.length} original turn projections and ${requests.length} live request bindings verified\n`);
