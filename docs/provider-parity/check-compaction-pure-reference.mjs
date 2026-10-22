// Reference for Pi 1.3.0's *pure* compaction preparation path:
//   - session-manager.ts  buildSessionProjection / sessionEntryToContextMessages
//   - compaction.ts       estimateTokens / estimateContextTokens /
//                         estimateProjectedContextTokens / prepareCompaction
//   - branch-summarization.ts collectEntriesForBranchSummary / prepareBranchEntries
//
// Imports the UNCHANGED upstream modules from the pinned Pi 1.3.0 install, asserts
// their sha256, drives scripted scenarios with numeric timestamps (no clock/random
// needed on this path) and emits a JSON fixture. `--write` regenerates it; without
// it the script compares byte-for-byte and exits non-zero on any difference.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile, writeFile } from "node:fs/promises";

const base = "../../../provider-references/integrated/node_modules/@earendil-works/";
const UPSTREAM = {
	"pi-coding-agent/dist/core/compaction/compaction.js":
		"573f0321e9a93a4e8dca8c327f6459082ac438d1bc8b6c3c171d90894dd1543c",
	"pi-coding-agent/dist/core/compaction/branch-summarization.js":
		"0327aead9ce78dab3c3c7e54779b5974ec41bcb769dd3024f0c61edcd9ebea7e",
	"pi-coding-agent/dist/core/compaction/utils.js":
		"c7e234cf33528670658080b0c2f599f40a70459565c6c2a71d4a27cb0e4c5cfb",
	"pi-coding-agent/dist/core/session-manager.js":
		"046b6a1109ac3f0ed893bb85bf0648709362fa926a5da75761216cf2fcf9d926",
	"pi-coding-agent/dist/core/messages.js":
		"8688b3f6eb28865f779cac998bd4754d1a4f08703200dfe0dd5a799aa0d42ef6",
};
const hashes = {};
for (const [path, expected] of Object.entries(UPSTREAM)) {
	const bytes = await readFile(new URL(base + path, import.meta.url));
	const digest = createHash("sha256").update(bytes).digest("hex");
	assert.equal(digest, expected, `upstream sha256 mismatch: ${path}`);
	hashes[path] = digest;
}

const { buildSessionProjection } = await import(base + "pi-coding-agent/dist/core/session-manager.js");
const { estimateTokens, estimateContextTokens, estimateProjectedContextTokens, prepareCompaction, getSummarizationFailure } =
	await import(base + "pi-coding-agent/dist/core/compaction/compaction.js");
const { collectEntriesForBranchSummary, prepareBranchEntries } =
	await import(base + "pi-coding-agent/dist/core/compaction/branch-summarization.js");
const { computeFileLists, createFileOps, extractFileOpsFromMessage, formatFileOperations } =
	await import(base + "pi-coding-agent/dist/core/compaction/utils.js");
const { version } = JSON.parse(
	await readFile(new URL(base + "pi-coding-agent/package.json", import.meta.url)),
);
assert.equal(version, "1.3.0");

// JS `Set`/`undefined` are not JSON; convert recursively keeping insertion order.
function toJson(value) {
	if (value instanceof Set) return [...value].map(toJson);
	if (Array.isArray(value)) return value.map(toJson);
	if (value && typeof value === "object") {
		const out = {};
		for (const key of Object.keys(value)) {
			if (value[key] === undefined) continue;
			out[key] = toJson(value[key]);
		}
		return out;
	}
	return value;
}

// Mirrors session-manager.ts `buildSessionPath` (not exported) so fixtures can
// feed `estimateProjectedContextTokens` the same branch the agent would use.
// `leafId === undefined` selects the last entry; `null` selects no path.
function sessionPath(entries, leafId) {
	const index = new Map(entries.map((entry) => [entry.id, entry]));
	let leaf;
	if (leafId === null) return [];
	if (leafId) leaf = index.get(leafId);
	leaf ??= entries[entries.length - 1];
	if (!leaf) return [];
	const path = [];
	let current = leaf;
	while (current) {
		path.push(current);
		current = current.parentId ? index.get(current.parentId) : undefined;
	}
	path.reverse();
	return path;
}

// `leaf` describes the leaf selector without losing the undefined/null/string
// distinction JSON cannot express: default (last entry), explicit none, or an id.
function leafSelector(leaf) {
	if (!leaf || leaf.mode === "default") return undefined;
	if (leaf.mode === "explicit") return null;
	return leaf.id;
}

const usage = (input, output, cacheRead = 0, cacheWrite = 0) => ({
	input,
	output,
	cacheRead,
	cacheWrite,
	totalTokens: 0,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
});
const text = (t) => ({ type: "text", text: t });
const toolCall = (name, args) => ({ type: "toolCall", id: `call-${name}`, name, arguments: args });

const user = (id, parentId, content) => ({
	type: "message",
	id,
	parentId,
	timestamp: 1000,
	message: { role: "user", content, timestamp: 1000 },
});
const assistant = (id, parentId, content, extra = {}) => ({
	type: "message",
	id,
	parentId,
	timestamp: 1000,
	message: {
		role: "assistant",
		api: "fixture",
		provider: "fixture",
		model: "fixture",
		content,
		usage: usage(10, 5),
		stopReason: "stop",
		timestamp: 1000,
		...extra,
	},
});
const toolResult = (id, parentId, content, extra = {}) => ({
	type: "message",
	id,
	parentId,
	timestamp: 1000,
	message: {
		role: "toolResult",
		toolCallId: "call-read",
		toolName: "read",
		content: [text(content)],
		isError: false,
		timestamp: 1000,
		...extra,
	},
});
const compactionEntry = (id, parentId, summary, firstKeptEntryId, extra = {}) => ({
	type: "compaction",
	id,
	parentId,
	timestamp: 1000,
	summary,
	firstKeptEntryId,
	tokensBefore: 123,
	...extra,
});
const branchSummaryEntry = (id, parentId, summary, extra = {}) => ({
	type: "branch_summary",
	id,
	parentId,
	timestamp: 1000,
	summary,
	fromId: "origin",
	...extra,
});
const customMessageEntry = (id, parentId, customType, content, extra = {}) => ({
	type: "custom_message",
	id,
	parentId,
	timestamp: 1000,
	customType,
	content,
	display: true,
	...extra,
});
const contextEdit = (id, parentId, targetId, replacement) => ({
	type: "context_edit",
	id,
	parentId,
	timestamp: 1000,
	targetId,
	replacement,
});
const meta = (id, parentId, type, extra = {}) => ({ type, id, parentId, timestamp: 1000, ...extra });

// ---------------------------------------------------------------------------
// 1. buildSessionProjection — complete projected entries + messages
// ---------------------------------------------------------------------------
const projectionCases = [
	{
		name: "linear",
		leaf: { mode: "default" },
		entries: [user("u1", null, "hi"), assistant("a1", "u1", [text("hello")]), user("u2", "a1", "again")],
	},
	{
		name: "settings_and_system",
		leaf: { mode: "default" },
		entries: [
			meta("m1", null, "model_change", { provider: "fixture", modelId: "fixture" }),
			meta("t1", "m1", "thinking_level_change", { thinkingLevel: "high" }),
			{
				type: "message",
				id: "s1",
				parentId: "t1",
				timestamp: 1000,
				message: { role: "system", content: "system prompt", timestamp: 1000 },
			},
			user("u1", "s1", "hi"),
		],
	},
	{
		name: "branch_leaf",
		leaf: { mode: "id", id: "b2" },
		entries: [
			user("u1", null, "root"),
			assistant("a1", "u1", [text("left")]),
			assistant("b2", "u1", [text("right")]),
			user("z9", "a1", "unused"),
		],
	},
	{
		name: "empty_entries",
		leaf: { mode: "default" },
		entries: [],
	},
	{
		name: "explicit_null_leaf",
		leaf: { mode: "explicit" },
		entries: [user("u1", null, "hi")],
	},
	{
		name: "compaction_boundary",
		leaf: { mode: "default" },
		entries: [
			user("u1", null, "old"),
			assistant("a1", "u1", [text("old answer")]),
			user("u2", "a1", "kept"),
			compactionEntry("c1", "u2", "summary one", "u2"),
			assistant("a2", "c1", [text("after")]),
		],
	},
	{
		name: "older_compaction_retained",
		leaf: { mode: "default" },
		entries: [
			user("u1", null, "old"),
			compactionEntry("c0", "u1", "summary zero", "u1"),
			user("u2", "c0", "kept"),
			compactionEntry("c1", "u2", "summary one", "c0"),
			assistant("a2", "c1", [text("after")]),
		],
	},
	{
		name: "compaction_system_message",
		leaf: { mode: "default" },
		entries: [
			user("u1", null, "old"),
			compactionEntry("c1", "u1", "summary", "u1", {
				systemMessage: { role: "system", content: "replayed system", timestamp: 1000 },
			}),
		],
	},
	{
		name: "context_edits",
		leaf: { mode: "default" },
		entries: [
			user("u1", null, "prompt"),
			assistant("a1", "u1", [text("answer"), toolCall("read", { path: "/a" })]),
			toolResult("r1", "a1", "file body"),
			contextEdit("e1", "r1", "u1", { content: [{ type: "text", text: "edited prompt" }] }),
			contextEdit("e2", "e1", "a1", { content: "edited answer" }),
			contextEdit("e3", "e2", "r1", { content: "edited result" }),
			user("u2", "e3", "next"),
		],
	},
	{
		name: "context_edit_omission",
		leaf: { mode: "default" },
		entries: [
			user("u1", null, "prompt"),
			assistant("a1", "u1", [text("answer")]),
			contextEdit("e1", "a1", "a1", null),
			user("u2", "e1", "next"),
		],
	},
	{
		name: "branch_summary_and_custom",
		leaf: { mode: "default" },
		entries: [
			branchSummaryEntry("b1", null, "left branch summary"),
			branchSummaryEntry("b2", "b1", ""),
			customMessageEntry("cm1", "b2", "note", "custom body"),
			customMessageEntry("cm2", "cm1", "note", null, { details: { k: 1 } }),
			user("u1", "cm2", "hi"),
		],
	},
	{
		name: "null_message_content",
		leaf: { mode: "default" },
		entries: [
			{
				type: "message",
				id: "s1",
				parentId: null,
				timestamp: 1000,
				message: { role: "system", content: null, timestamp: 1000 },
			},
			{ type: "message", id: "u1", parentId: "s1", timestamp: 1000, message: { role: "user", content: null, timestamp: 1000 } },
			{
				type: "message",
				id: "a1",
				parentId: "u1",
				timestamp: 1000,
				message: {
					role: "assistant",
					api: "fixture",
					provider: "fixture",
					model: "fixture",
					content: null,
					usage: usage(1, 1),
					stopReason: "stop",
					timestamp: 1000,
				},
			},
		],
	},
	{
		name: "display_only_entries",
		leaf: { mode: "default" },
		entries: [
			meta("c1", null, "custom", { customType: "state", data: { a: 1 } }),
			meta("l1", "c1", "label", { label: "checkpoint" }),
			meta("i1", "l1", "session_info", { name: "named" }),
			meta("at1", "i1", "active_tools_change", { toolNames: ["read"] }),
			user("u1", "at1", "hi"),
		],
	},
	{
		name: "system_tools_replay",
		leaf: { mode: "default" },
		entries: [
			{
				type: "message",
				id: "s1",
				parentId: null,
				timestamp: 1000,
				message: {
					role: "system",
					content: "base prompt",
					sections: { skills: "one", notes: null },
					toolsAdded: [{ name: "read", description: "reads" }],
					timestamp: 1000,
				},
			},
			{
				type: "message",
				id: "s2",
				parentId: "s1",
				timestamp: 1000,
				message: {
					role: "system",
					content: "more prompt",
					sections: { skills: "two" },
					toolsAdded: [{ name: "write", description: "writes" }],
					toolsRemoved: [{ name: "read" }],
					timestamp: 1000,
				},
			},
			user("u1", "s2", "hi"),
		],
	},
];
const projection = projectionCases.map(({ name, leaf, entries }) => {
	const leafId = leafSelector(leaf);
	const projectionResult = buildSessionProjection(entries, leafId);
	// The local path helper must agree with the unchanged projection on sources.
	const pathIds = new Set(sessionPath(entries, leafId).map((entry) => entry.id));
	for (const entry of projectionResult.entries) assert(pathIds.has(entry.sourceEntry.id));
	return {
		name,
		leaf: leaf ?? { mode: "default" },
		entries,
		result: {
			entries: projectionResult.entries.map((entry) => ({
				sourceId: entry.sourceEntry.id,
				sourceType: entry.sourceEntry.type,
				messages: entry.messages,
			})),
			messages: projectionResult.messages,
			thinkingLevel: projectionResult.thinkingLevel,
			model: projectionResult.model,
		},
	};
});

// ---------------------------------------------------------------------------
// 2. Token estimation
// ---------------------------------------------------------------------------
const tokenMessages = [
	["user_text", { role: "user", content: "abcd", timestamp: 0 }],
	["user_unicode", { role: "user", content: "😀한글", timestamp: 0 }],
	["user_blocks", { role: "user", content: [text("abcd"), { type: "image", data: "x" }], timestamp: 0 }],
	["assistant_mixed", {
		role: "assistant",
		api: "fixture",
		provider: "fixture",
		model: "fixture",
		content: [text("abcd"), { type: "thinking", thinking: "efgh" }, toolCall("read", { path: "/a", n: [1, 2] })],
		usage: usage(1, 1),
		stopReason: "stop",
		timestamp: 0,
	}],
	["assistant_tool_args_unicode", {
		role: "assistant",
		api: "fixture",
		provider: "fixture",
		model: "fixture",
		content: [toolCall("edit", { path: "/é", text: "\"quoted\"\nline" })],
		usage: usage(1, 1),
		stopReason: "stop",
		timestamp: 0,
	}],
	["system_sections", {
		role: "system",
		content: "abcd",
		sections: { a: "efgh", b: null, c: "ij" },
		toolsAdded: { read: { description: "reads" } },
		timestamp: 0,
	}],
	["tool_result", { role: "toolResult", toolCallId: "c", toolName: "read", content: [text("12345678")], isError: false, timestamp: 0 }],
	["bash_execution", {
		role: "bashExecution",
		command: "echo hi",
		output: "hi",
		cancelled: false,
		exitCode: 0,
		timestamp: 0,
	}],
	["custom", { role: "custom", customType: "note", content: [text("1234")], display: true, timestamp: 0 }],
	["branch_summary", { role: "branchSummary", summary: "12345678", fromId: "x", timestamp: 0 }],
	["compaction_summary", { role: "compactionSummary", summary: "1234", tokensBefore: 5, timestamp: 0 }],
];
const estimateTokensRows = tokenMessages.map(([name, message]) => ({
	name,
	message,
	result: estimateTokens(message),
}));

const contextUsage = usage(100, 20, 5, 5);
const plainUser = (content) => ({ role: "user", content, timestamp: 0 });
const plainAssistant = (content, extra = {}) => ({
	role: "assistant",
	api: "fixture",
	provider: "fixture",
	model: "fixture",
	content,
	usage: usage(10, 5),
	stopReason: "stop",
	timestamp: 0,
	...extra,
});
const estimateContextRows = [
	{
		name: "no_usage",
		messages: [plainUser("12345678"), plainAssistant([text("1234")], { usage: usage(0, 0), stopReason: "error" })],
	},
	{
		name: "usage_and_trailing",
		messages: [
			plainAssistant([text("1234")], { usage: contextUsage }),
			plainUser("12345678"),
			plainUser("1234"),
		],
	},
	{
		name: "skips_aborted_error_zero",
		messages: [
			plainAssistant([text("1234")], { usage: contextUsage, stopReason: "aborted" }),
			plainAssistant([text("1234")], { usage: contextUsage, stopReason: "error" }),
			plainAssistant([text("1234")], { usage: usage(0, 0), stopReason: "stop" }),
			plainAssistant([text("1234")], { usage: contextUsage }),
			plainUser("12345678"),
		],
	},
];
const estimateContext = estimateContextRows.map(({ name, messages }) => ({
	name,
	messages,
	result: estimateContextTokens(messages),
}));

const projectedCases = [
	{ name: "fresh_usage", entries: [
		user("u1", null, "12345678"),
		assistant("a1", "u1", [text("1234")], { usage: contextUsage }),
		user("u2", "a1", "1234"),
	] },
	{ name: "usage_before_compaction", entries: [
		assistant("a1", null, [text("1234")], { usage: contextUsage }),
		user("u2", "a1", "12345678"),
		compactionEntry("c1", "u2", "summary", "u2"),
	] },
	{ name: "usage_before_context_edit", entries: [
		assistant("a1", null, [text("1234")], { usage: contextUsage }),
		user("u2", "a1", "12345678"),
		contextEdit("e1", "u2", "a1", { content: "edited" }),
	] },
	{ name: "usage_after_edit_of_other", entries: [
		user("u1", null, "12345678"),
		assistant("a1", "u1", [text("1234")], { usage: contextUsage }),
		contextEdit("e1", "a1", "u1", { content: "edited" }),
		user("u2", "e1", "1234"),
	] },
	{ name: "no_usage_system_included", entries: [
		{ type: "message", id: "s1", parentId: null, timestamp: 1000,
			message: { role: "system", content: "system text", timestamp: 1000 } },
		user("u1", "s1", "12345678"),
	] },
	{ name: "no_usage_system_replay", entries: [
		{ type: "message", id: "s1", parentId: null, timestamp: 1000,
			message: { role: "system", content: "base", sections: { a: "one", b: null },
				toolsAdded: [{ name: "read", description: "reads" }], timestamp: 1000 } },
		{ type: "message", id: "s2", parentId: "s1", timestamp: 1000,
			message: { role: "system", content: "more", sections: { a: "two" },
				toolsAdded: [{ name: "write", description: "writes" }], toolsRemoved: [{ name: "read" }], timestamp: 1000 } },
		user("u1", "s2", "1234"),
	] },
];
const estimateProjected = projectedCases.map(({ name, entries }) => {
	const proj = buildSessionProjection(entries, undefined);
	const branch = sessionPath(entries, undefined);
	return {
		name,
		entries,
		branchEntryIds: branch.map((entry) => entry.id),
		branchEntries: branch,
		result: toJson(estimateProjectedContextTokens(proj, branch)),
	};
});

// ---------------------------------------------------------------------------
// 3. prepareCompaction
// ---------------------------------------------------------------------------
const long = (n) => "x".repeat(n * 4); // ~n tokens
const bigUser = (id, parentId, tokens) => user(id, parentId, long(tokens));
const settings = (keepRecentTokens) => ({ enabled: true, reserveTokens: 16384, keepRecentTokens });

const prepareCases = [
	{
		name: "empty",
		settings: settings(20),
		entries: [],
	},
	{
		name: "already_compacted",
		settings: settings(20),
		entries: [bigUser("u1", null, 5), compactionEntry("c1", "u1", "summary", "u1")],
	},
	{
		name: "cut_at_assistant_split",
		settings: settings(8),
		entries: [
			bigUser("u1", null, 4),
			assistant("a1", "u1", [text(long(4))]),
			bigUser("u2", "a1", 4),
			assistant("a2", "u2", [text(long(4))]),
			bigUser("u3", "a2", 4),
		],
	},
	{
		name: "split_turn",
		settings: settings(4),
		entries: [
			bigUser("u1", null, 2),
			assistant("a1", "u1", [text(long(6))]),
			assistant("a2", "a1", [text(long(6))]),
		],
	},
	{
		name: "split_turn_empty_history",
		settings: settings(1),
		entries: [bigUser("u1", null, 8), assistant("a1", "u1", [text(long(8))])],
	},
	{
		name: "previous_compaction",
		settings: settings(4),
		entries: [
			bigUser("u1", null, 6),
			compactionEntry("c1", "u1", "prior summary", "u2", {
				details: { readFiles: ["/read"], modifiedFiles: ["/mod"] },
			}),
			bigUser("u2", "c1", 2),
			assistant("a2", "u2", [text(long(6))]),
			bigUser("u3", "a2", 2),
		],
	},
	{
		name: "previous_compaction_from_hook",
		settings: settings(4),
		entries: [
			bigUser("u1", null, 6),
			compactionEntry("c1", "u1", "prior summary", "u2", {
				fromHook: true,
				details: { readFiles: ["/read"], modifiedFiles: ["/mod"] },
			}),
			bigUser("u2", "c1", 2),
			assistant("a2", "u2", [text(long(6))]),
		],
	},
	{
		name: "file_ops_from_tools",
		settings: settings(2),
		entries: [
			bigUser("u1", null, 4),
			assistant("a1", "u1", [toolCall("read", { path: "/r" }), toolCall("write", { path: "/w" }), toolCall("edit", { path: "/e" })]),
			toolResult("r1", "a1", "body"),
			assistant("a2", "r1", [toolCall("edit", { path: "/r" })]),
			bigUser("u2", "a2", 4),
		],
	},
	{
		name: "recovery_omission_advance",
		settings: settings(4),
		entries: [
			bigUser("u1", null, 8),
			assistant("a1", "u1", [text(long(4))]),
			contextEdit("e1", "a1", "a1", null),
		],
	},
	{
		name: "recovery_omission_external_replacement",
		settings: settings(4),
		entries: [
			bigUser("u1", null, 8),
			assistant("a1", "u1", [text(long(4))]),
			contextEdit("e1", "a1", "a1", null),
			contextEdit("e2", "e1", "u1", { content: "replaced" }),
		],
	},
	{
		name: "metadata_before_cut",
		settings: settings(4),
		entries: [
			bigUser("u1", null, 2),
			meta("m1", "u1", "model_change", { provider: "fixture", modelId: "fixture" }),
			meta("t1", "m1", "thinking_level_change", { thinkingLevel: "high" }),
			bigUser("u2", "t1", 6),
		],
	},
	{
		name: "branch_summary_message",
		settings: settings(4),
		entries: [
			branchSummaryEntry("b1", null, long(4)),
			bigUser("u1", "b1", 6),
			assistant("a1", "u1", [text(long(4))]),
		],
	},
	{
		name: "system_message_kept_out_of_summary",
		settings: settings(4),
		entries: [
			{ type: "message", id: "s1", parentId: null, timestamp: 1000,
				message: { role: "system", content: "system prompt", timestamp: 1000 } },
			bigUser("u1", "s1", 2),
			assistant("a1", "u1", [text(long(6))]),
		],
	},
	{
		name: "tool_result_only_tail",
		settings: settings(2),
		entries: [
			bigUser("u1", null, 2),
			assistant("a1", "u1", [toolCall("read", { path: "/r" })]),
			toolResult("r1", "a1", long(8)),
		],
	},
	{
		name: "previous_compaction_missing_kept",
		settings: settings(4),
		entries: [
			bigUser("u1", null, 6),
			compactionEntry("c1", "u1", "prior summary", "missing", {
				details: { readFiles: ["/read"], modifiedFiles: [] },
			}),
			bigUser("u2", "c1", 2),
			assistant("a2", "u2", [text(long(6))]),
		],
	},
	{
		name: "retain_none_previous_compaction",
		settings: settings(4),
		entries: [
			bigUser("u1", null, 6),
			compactionEntry("c1", "u1", "prior summary", "c1"),
			bigUser("u2", "c1", 2),
			assistant("a2", "u2", [text(long(6))]),
			bigUser("u3", "a2", 2),
		],
	},
];
const prepare = prepareCases.map(({ name, settings: s, entries }) => {
	const result = prepareCompaction(entries, s);
	return { name, settings: s, entries, result: result === undefined ? null : toJson(result) };
});

// ---------------------------------------------------------------------------
// 4. Branch collection + preparation
// ---------------------------------------------------------------------------
const branchTree = [
	user("a", null, "root"),
	user("b", "a", "left one"),
	assistant("c", "b", [text("left two")]),
	user("d", "c", "left three"),
	user("e", "a", "right one"),
	user("f", "e", "right two"),
];
const collectCases = [
	{ name: "to_sibling", oldLeafId: "d", targetId: "f" },
	{ name: "to_ancestor", oldLeafId: "d", targetId: "b" },
	{ name: "from_root", oldLeafId: "a", targetId: "f" },
	{ name: "same_leaf", oldLeafId: "d", targetId: "d" },
	{ name: "no_old_leaf", oldLeafId: null, targetId: "f" },
];
const collect = collectCases.map(({ name, oldLeafId, targetId }) => {
	const index = new Map(branchTree.map((entry) => [entry.id, entry]));
	const fakeSession = {
		getBranch: (leafId) => sessionPath(branchTree, leafId),
		getEntry: (id) => index.get(id),
	};
	const result = collectEntriesForBranchSummary(fakeSession, oldLeafId, targetId);
	return {
		name,
		oldLeafId,
		targetId,
		entries: branchTree,
		result: { entries: toJson(result.entries), commonAncestorId: result.commonAncestorId },
	};
});

const branchEntries = [
	branchSummaryEntry("p1", null, "prior summary", {
		details: { readFiles: ["/r1"], modifiedFiles: ["/m1"] },
	}),
	branchSummaryEntry("p2", "p1", "hook summary", {
		fromHook: true,
		details: { readFiles: ["/r2"], modifiedFiles: ["/m2"] },
	}),
	user("u1", "p2", "12345678"),
	assistant("a1", "u1", [text("1234"), toolCall("read", { path: "/r3" }), toolCall("edit", { path: "/m3" })]),
	toolResult("t1", "a1", "ignored tool result"),
	compactionEntry("c1", "t1", "compaction summary", "u1"),
	customMessageEntry("cm1", "c1", "note", "1234"),
	user("u2", "cm1", "1234"),
];
const branchPrepare = [0, 3, 6, 1000].map((tokenBudget) => ({
	name: `budget_${tokenBudget}`,
	tokenBudget,
	entries: branchEntries,
	result: toJson(prepareBranchEntries(branchEntries, tokenBudget)),
}));

// ---------------------------------------------------------------------------
// 5. File-operation utils + summarization failure classification
// ---------------------------------------------------------------------------
const extractFileOpsRows = [
	["assistant_calls", assistant("a1", null, [toolCall("read", { path: "/r" }), toolCall("write", { path: "/w" }), toolCall("edit", { path: "/e" })]).message],
	["unknown_and_missing_path", assistant("a1", null, [toolCall("bash", { path: "/b" }), toolCall("read", {}), toolCall("edit", { path: 3 })]).message],
	["non_array_content", { role: "assistant", api: "fixture", provider: "fixture", model: "fixture", content: "text", usage: usage(1, 1), stopReason: "stop", timestamp: 0 }],
	["nested_calls", { role: "toolResult", toolCallId: "c", toolName: "codemode", content: [], isError: false, timestamp: 0,
		nestedCalls: { calls: [{ name: "read", arguments: { path: "/nested" } }, { name: "write", arguments: { path: "/nested-w" } }] } }],
	["user_message", { role: "user", content: "hi", timestamp: 0 }],
].map(([name, message]) => {
	const fileOps = createFileOps();
	extractFileOpsFromMessage(message, fileOps);
	return { name, message, result: toJson(fileOps) };
});

const computeFileListsRows = [
	["disjoint", ["/read"], ["/write"], ["/edit"]],
	["read_also_modified", ["/a", "/b"], ["/b"], ["/a"]],
	["duplicates_and_order", ["/z", "/a", "/z"], [], ["/m", "/a"]],
	["unicode", ["/é", "/a", "/Z"], [], []],
	["empty", [], [], []],
].map(([name, read, written, edited]) => {
	const fileOps = createFileOps();
	for (const path of read) fileOps.read.add(path);
	for (const path of written) fileOps.written.add(path);
	for (const path of edited) fileOps.edited.add(path);
	const { readFiles, modifiedFiles } = computeFileLists(fileOps);
	return { name, read, written, edited, result: { readFiles, modifiedFiles } };
});

const formatFileOperationsRows = [
	["both", ["/a", "/b"], ["/m"]],
	["read_only", ["/a"], []],
	["modified_only", [], ["/m"]],
	["empty", [], []],
].map(([name, readFiles, modifiedFiles]) => ({
	name,
	readFiles,
	modifiedFiles,
	result: formatFileOperations(readFiles, modifiedFiles),
}));

const summarizationFailureRows = [
	["error", { stopReason: "error", errorMessage: "boom" }, "Summarization"],
	["error_empty_message", { stopReason: "error", errorMessage: "" }, "Summarization"],
	["error_missing_message", { stopReason: "error" }, "Turn prefix summarization"],
	["length", { stopReason: "length" }, "Summarization"],
	["stop", { stopReason: "stop" }, "Summarization"],
	["aborted", { stopReason: "aborted" }, "Branch summarization"],
].map(([name, response, label]) => ({
	name,
	response,
	label,
	result: getSummarizationFailure(response, label) ?? null,
}));

const fixture = {
	reference: "Pi 1.3.0",
	upstream: hashes,
	projection,
	tokens: {
		estimateTokens: estimateTokensRows,
		estimateContextTokens: estimateContext,
		estimateProjectedContextTokens: estimateProjected,
	},
	prepare,
	branch: { collect, prepare: branchPrepare },
	utils: {
		extractFileOps: extractFileOpsRows,
		computeFileLists: computeFileListsRows,
		formatFileOperations: formatFileOperationsRows,
		summarizationFailure: summarizationFailureRows,
	},
};

const destination = new URL("compaction-pure-reference.json", import.meta.url);
const serialized = `${JSON.stringify(fixture, null, 2)}\n`;
if (process.argv.includes("--write")) await writeFile(destination, serialized);
else assert.equal(await readFile(destination, "utf8"), serialized);
process.stdout.write(
	`${projection.length} projection, ${estimateTokensRows.length} token, ${estimateContext.length} context, ` +
		`${estimateProjected.length} projected, ${prepare.length} prepare, ${collect.length} collect, ` +
		`${branchPrepare.length} branch-budget rows verified\n`,
);
