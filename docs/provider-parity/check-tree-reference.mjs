import assert from "node:assert/strict";
import { readFile, writeFile } from "node:fs/promises";
import { AgentSession } from "../../../provider-references/integrated/node_modules/@earendil-works/pi-coding-agent/dist/core/agent-session.js";
const version = JSON.parse(await readFile(new URL("../../../provider-references/integrated/node_modules/@earendil-works/pi-coding-agent/package.json", import.meta.url))).version;
assert.equal(version, "1.3.0");
const rows = [];
for (const mode of ["same", "missing", "assistant", "user", "custom", "cancel", "override", "ignoredOverride", "overrideLabel", "hookError"]) {
	const message = (id, parentId, role, text) => ({ type: "message", id, parentId, timestamp: 0, message: { role, content: [{ type: "text", text }], timestamp: 0 } });
	const entries = [message("root", null, "user", "root prompt"), message("left", "root", "assistant", "abandoned"),
		message("target", "root", "assistant", "target response"), message("user", "root", "user", "edit this"),
		{ type: "custom_message", id: "custom", parentId: "root", timestamp: 0, customType: "fixture", content: "edit custom", display: true }];
	let leaf = "left";
	const mutations = [], events = [];
	const manager = {
		getLeafId: () => leaf,
		getEntry: id => entries.find(entry => entry.id === id),
		getBranch: id => { const path = []; while (id) { const entry = manager.getEntry(id); if (!entry) break; path.unshift(entry); id = entry.parentId; } return path; },
		branch: id => { mutations.push({ type: "branch", id }); leaf = id; },
		resetLeaf: () => { mutations.push({ type: "branch", id: null }); leaf = null; },
		branchWithSummary: (parentId, summary, details, fromHook, usage) => {
			const entry = { type: "branch_summary", id: "summary", parentId, fromId: leaf, summary, details, fromHook, usage };
			entries.push(entry); mutations.push(entry); leaf = entry.id; return entry.id;
		},
		appendLabelChange: (id, label) => { mutations.push({ type: "label", id, label }); },
	};
	const session = Object.assign(Object.create(AgentSession.prototype), {
		agent: { state: { isStreaming: false, model: { id: "fixture", provider: "fixture" } } }, sessionManager: manager,
		_refreshFinalizedContext: () => {}, _restoreToolsFromTranscript: () => {}, _resolveIdleWaitIfIdle: () => {},
		_extensionRunner: { hasHandlers: () => true, emit: async event => {
			await Promise.resolve();
			if (event.type === "session_before_tree") {
				assert(event.signal instanceof AbortSignal);
				const p = event.preparation;
				events.push({ type: event.type, preparation: { ...p, entriesToSummarize: p.entriesToSummarize.map(entry => entry.id) } });
				if (mode === "cancel") return { cancel: true };
				if (mode === "hookError") throw new Error("fixture hook failure");
				if (["override", "ignoredOverride"].includes(mode)) return { summary: { summary: "retained summary", details: { fixture: true } }, label: "summary label" };
				if (mode === "overrideLabel") return { customInstructions: "new instructions", replaceInstructions: true, label: "changed" };
			} else events.push(event);
		} },
	});
	const target = ({ same: "left", missing: "absent", user: "user", custom: "custom" })[mode] ?? "target";
	let result, error;
	try { result = await session.navigateTree(target, { summarize: mode === "override", customInstructions: "original", label: "original label" }); }
	catch (caught) { error = caught.message; }
	assert.equal(session._branchSummaryAbortController, undefined);
	rows.push({ mode, events, mutations, leaf, result, error });
}
const text = JSON.stringify({ reference: "Pi 1.3.0", projection: "Navigation hooks, target selection, extension summaries, labels and editor text; default summarization is tested separately", rows }, null, 2) + "\n";
const destination = new URL("tree-reference.json", import.meta.url);
if (process.argv.includes("--write")) await writeFile(destination, text);
else assert.equal(await readFile(destination, "utf8"), text);
process.stdout.write(`${rows.length} original tree navigation traces verified\n`);
