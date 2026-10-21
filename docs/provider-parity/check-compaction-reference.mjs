import assert from "node:assert/strict";
import { readFile, writeFile } from "node:fs/promises";
import { AgentSession } from "../../../provider-references/integrated/node_modules/@earendil-works/pi-coding-agent/dist/core/agent-session.js";
const packageJson = JSON.parse(await readFile(new URL("../../../provider-references/integrated/node_modules/@earendil-works/pi-coding-agent/package.json", import.meta.url)));
assert.equal(packageJson.version, "1.3.0");
const rows = [];
for (const reason of ["manual", "threshold", "overflow"]) {
	for (const mode of ["default", "extension", "cancel", "signal", "defaultError", "extensionError", ...(reason === "manual" ? ["empty", "already"] : [])]) {
		const entries = mode === "empty" ? [] : mode === "already" ? [{ type: "compaction", id: "prior", summary: "prior", tokensBefore: 1 }]
			: ["history", "recent"].map((id, index) => ({ type: "message", id, parentId: index ? "history" : null,
				timestamp: "1970-01-01T00:00:00.000Z", message: { role: "user", content: `fixture ${id}`, timestamp: 0 } }));
		const events = [], stored = [];
		const result = { summary: "fixture summary", tokensBefore: 100, firstKeptEntryId: "history" };
		const source = Object.assign(Object.create(AgentSession.prototype), {
			agent: { state: { model: { id: "fixture", provider: "fixture", contextWindow: 1000 } }, hasQueuedMessages: () => false },
			abort: async () => {}, _emit: () => {}, _clearManualCompactionState: () => {}, _resolveIdleWaitIfIdle: () => {}, _refreshFinalizedContext: () => {},
			settingsManager: { getCompactionSettings: () => ({ enabled: true, reserveTokens: 0, keepRecentTokens: 0 }) },
			sessionManager: { getBranch: () => entries, getEntries: () => stored, buildSessionProjection: () => ({ messages: [] }),
				appendCompaction: () => { if (mode === "extensionError") throw new Error("fixture failure"); stored.push({ type: "compaction", id: "committed", ...result }); } },
			_runDefaultCompaction: async () => { if (mode === "defaultError") throw new Error("fixture failure"); return result; },
			_extensionRunner: { hasHandlers: () => true, emit: async event => {
				const { type, reason, willRetry, fromExtension, aborted, errorMessage } = event;
				events.push(JSON.parse(JSON.stringify({ type, reason, willRetry, fromExtension, aborted, errorMessage })));
				if (type === "session_before_compact") {
					assert.deepEqual(event.branchEntries, entries);
					if (mode === "cancel") return { cancel: true };
					if (mode === "signal") (source._compactionAbortController ?? source._autoCompactionAbortController).abort();
					if (["extension", "signal", "extensionError"].includes(mode)) return { compaction: result };
				}
			} },
		});
		try {
			if (reason === "manual") await source.compact();
			else await source._runAutoCompaction(reason, reason === "overflow");
		} catch (error) { assert.equal(reason, "manual"); assert(error instanceof Error); }
		assert.equal(events.at(-1)?.type, ["default", "extension"].includes(mode) ? "session_compact" : "session_compact_failed", JSON.stringify({ reason, mode, events }));
		rows.push({ reason, mode, events });
	}
}
const destination = new URL("compaction-reference.json", import.meta.url);
const text = JSON.stringify({ reference: "Pi 1.3.0", projection: "Compaction lifecycle reason, retry, source and failure fields; before-event branch entries are also checked against the captured source branch", rows }, null, 2) + "\n";
if (process.argv.includes("--write")) await writeFile(destination, text);
else assert.equal(await readFile(destination, "utf8"), text);
process.stdout.write(`${rows.length} original Pi compaction traces verified\n`);
