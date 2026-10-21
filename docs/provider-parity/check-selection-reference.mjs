import assert from "node:assert/strict";
import { readFile, writeFile } from "node:fs/promises";
import { AgentSession } from "../../../provider-references/integrated/node_modules/@earendil-works/pi-coding-agent/dist/core/agent-session.js";
const packageJson = JSON.parse(await readFile(new URL("../../../provider-references/integrated/node_modules/@earendil-works/pi-coding-agent/package.json", import.meta.url)));
assert.equal(packageJson.version, "1.3.0");
const rows = [];
const previous = { id: "one", provider: "fixture", api: "fixture-api", name: "One" };
for (const source of ["set", "cycle", "restore"]) {
	for (const [mode, next] of Object.entries({ same: previous, metadata: { ...previous, name: "Updated" },
		model: { ...previous, id: "two" }, provider: { ...previous, provider: "other" } })) {
		const events = [];
		const session = Object.assign(Object.create(AgentSession.prototype), {
			_extensionRunner: { emit: async event => { await new Promise(resolve => setImmediate(resolve)); events.push(event); } },
		});
		await session._emitModelSelect(next, previous, source);
		assert.equal(events.length, ["same", "metadata"].includes(mode) ? 0 : 1);
		rows.push({ source, mode, events });
	}
}
let name;
const events = [], writes = [];
const session = Object.assign(Object.create(AgentSession.prototype), {
	sessionManager: { appendSessionInfo: next => { writes.push(next); name = next; }, getSessionName: () => name },
	_emit: () => {}, _extensionRunner: { emit: async event => { events.push(event); } },
});
for (const next of ["First", "First", "Second", ""]) session.setSessionName(next);
assert.deepEqual(events.map(event => event.name), writes);
const result = JSON.stringify({ reference: "Pi 1.3.0", modelRows: rows, nameEvents: events }, null, 2) + "\n";
const destination = new URL("selection-reference.json", import.meta.url);
if (process.argv.includes("--write")) await writeFile(destination, result);
else assert.equal(await readFile(destination, "utf8"), result);
process.stdout.write(`${rows.length} model traces and ${events.length} committed name events verified\n`);
