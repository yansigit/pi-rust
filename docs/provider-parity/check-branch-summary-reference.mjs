import assert from "node:assert/strict";
import { readFile, writeFile } from "node:fs/promises";
import { generateBranchSummary } from "../../../provider-references/integrated/node_modules/@earendil-works/pi-coding-agent/dist/core/compaction/branch-summarization.js";
const version = JSON.parse(await readFile(new URL("../../../provider-references/integrated/node_modules/@earendil-works/pi-coding-agent/package.json", import.meta.url))).version;
assert.equal(version, "1.3.0");
const rows = [];
const clock = Date.now;
try {
	Date.now = () => 1000;
	for (const mode of ["default", "small", "unlimited", "emptyInstructions", "focus", "replace", "emptyReplace", "length", "error", "emptyError", "tool", "aborted", "emptyEntries", "priorNative", "priorHook", "grace"]) {
		const requests = [];
		const model = { id: "fixture", provider: "fixture", api: "fixture", reasoning: true, contextWindow: 128000, maxTokens: mode === "small" ? 512 : mode === "unlimited" ? 0 : 8192 };
		const options = { model, retry: { enabled: false }, streamFn: async (_, context, options) => {
			assert.match(options.sessionId, /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
			requests.push({ context, maxTokens: options.maxTokens, cacheRetention: options.cacheRetention,
				reasoning: options.reasoning ?? null });
			const stopReason = ["error", "emptyError"].includes(mode) ? "error" : ["length", "aborted"].includes(mode) ? mode : "stop";
			const response = { role: "assistant", api: "fixture", provider: "fixture", model: "fixture", timestamp: 1000,
				content: mode === "tool" ? [{ type: "toolCall", id: "call", name: "write", arguments: {} }] : [{ type: "text", text: "summary text" }],
				usage: { input: 1, output: 2, cacheRead: 0, cacheWrite: 0, totalTokens: 3, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
				stopReason, ...(mode === "error" ? { errorMessage: "fixture failure" } : mode === "emptyError" ? { errorMessage: "" } : {}) };
			return { result: async () => response };
		} };
		if (mode === "grace") options.reserveTokens = model.contextWindow - 3;
		if (["emptyInstructions", "emptyReplace"].includes(mode)) options.customInstructions = "";
		if (["focus", "replace"].includes(mode)) options.customInstructions = "Keep the commands";
		if (["replace", "emptyReplace"].includes(mode)) options.replaceInstructions = true;
		const entries = mode === "emptyEntries" ? [] : [{ type: "message", id: "user", parentId: null, timestamp: 1000,
			message: { role: "user", content: "Summarize this branch", timestamp: 1000 } }];
		if (["priorNative", "priorHook", "grace"].includes(mode)) {
			if (mode === "grace") entries[0].message.content = "12345678";
			entries.unshift({ type: "branch_summary", id: "prior", parentId: null, timestamp: 1000,
				fromId: "origin", summary: "older summary", fromHook: mode === "priorHook",
				details: { readFiles: ["/r"], modifiedFiles: ["/m"] } });
		}
		const result = await generateBranchSummary(entries, options);
		rows.push({ mode, requests, result });
	}
} finally { Date.now = clock; }
const text = JSON.stringify({ reference: "Pi 1.3.0", projection: "Summary prompt, request options and terminal outcome; generated routing IDs are checked for UUIDv7 shape, not captured", rows }, null, 2) + "\n";
const destination = new URL("branch-summary-reference.json", import.meta.url);
if (process.argv.includes("--write")) await writeFile(destination, text);
else assert.equal(await readFile(destination, "utf8"), text);
process.stdout.write(`${rows.length} original branch summary traces verified\n`);
