// Reference for Pi 1.3.0's automatic-compaction trigger decisions:
//   - pi-ai overflow.ts        isContextOverflow / isRecoverableLength
//   - compaction.ts            shouldCompact
//   - agent-session.ts         _checkCompaction decision table (ordered side effects)
//                              _runAutoCompaction ordered lifecycle operations
//
// Imports UNCHANGED upstream modules from the pinned Pi 1.3.0 install, asserts
// their sha256, drives scripted scenarios with injected stubs, and emits a JSON
// fixture. `--write` regenerates; otherwise the script compares byte-for-byte.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile, writeFile } from "node:fs/promises";

const base = "../../../provider-references/integrated/node_modules/@earendil-works/";
const UPSTREAM = {
	"pi-ai/dist/utils/overflow.js":
		"85570a78e9ba3e0322fe1b3f14c991b6a439b1d3eb89af306e2e7e9ade3d53b4",
	"pi-coding-agent/dist/core/compaction/compaction.js":
		"573f0321e9a93a4e8dca8c327f6459082ac438d1bc8b6c3c171d90894dd1543c",
	"pi-coding-agent/dist/core/agent-session.js":
		"d0b4545a5da7fe97aa699269e64e27b37ee2fb5b7985efb1c0a66fc3d1883b52",
	"pi-coding-agent/dist/core/session-manager.js":
		"046b6a1109ac3f0ed893bb85bf0648709362fa926a5da75761216cf2fcf9d926",
	"pi-coding-agent/dist/core/compaction/index.js":
		"2ab40284c2650bf3c3f195442be00497f0ee3cb726ac47c29bfa4b3ae209697e",
};
const hashes = {};
for (const [path, expected] of Object.entries(UPSTREAM)) {
	const bytes = await readFile(new URL(base + path, import.meta.url));
	const digest = createHash("sha256").update(bytes).digest("hex");
	assert.equal(digest, expected, `upstream sha256 mismatch: ${path}`);
	hashes[path] = digest;
}

const { isContextOverflow, isRecoverableLength } = await import(base + "pi-ai/dist/utils/overflow.js");
const { shouldCompact } = await import(base + "pi-coding-agent/dist/core/compaction/compaction.js");
const { prepareCompaction } = await import(base + "pi-coding-agent/dist/core/compaction/compaction.js");
const { buildSessionProjection } = await import(base + "pi-coding-agent/dist/core/session-manager.js");
const { AgentSession } = await import(base + "pi-coding-agent/dist/core/agent-session.js");
const { version } = JSON.parse(
	await readFile(new URL(base + "pi-coding-agent/package.json", import.meta.url)),
);
assert.equal(version, "1.3.0");

const usage = (input, output, cacheRead = 0) => ({
	input,
	output,
	cacheRead,
	cacheWrite: 0,
	totalTokens: 0,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
});
const text = (t) => ({ type: "text", text: t });
const user = (id, parentId, content) => ({
	type: "message",
	id,
	parentId,
	timestamp: 1000,
	message: { role: "user", content, timestamp: 1000 },
});
const assistant = (id, parentId, extra = {}) => ({
	type: "message",
	id,
	parentId,
	timestamp: 1000,
	message: {
		role: "assistant",
		api: "fixture",
		provider: "fixture",
		model: "fixture",
		content: [text("answer")],
		usage: usage(10, 5),
		stopReason: "stop",
		timestamp: 1000,
		...extra,
	},
});
const compactionEntry = (id, parentId, extra = {}) => ({
	type: "compaction",
	id,
	parentId,
	timestamp: 2000,
	summary: "summary",
	firstKeptEntryId: parentId ?? id,
	tokensBefore: 100,
	...extra,
});
const contextEdit = (id, parentId, targetId, replacement) => ({
	type: "context_edit",
	id,
	parentId,
	timestamp: 3000,
	targetId,
	replacement,
});

// ---------------------------------------------------------------------------
// 1. isContextOverflow / isRecoverableLength corpus
// ---------------------------------------------------------------------------
const errorMessage = (message, extra = {}) => ({
	role: "assistant",
	api: "fixture",
	provider: extra.provider ?? "fixture",
	model: "fixture",
	content: [],
	usage: usage(1, 1),
	stopReason: "error",
	errorMessage: message,
	timestamp: 0,
});
const overflowCorpus = [
	["anthropic_prompt_too_long", errorMessage("prompt is too long: 213462 tokens > 200000 maximum")],
	["anthropic_request_too_large", errorMessage('413 {"error":{"type":"request_too_large","message":"Request exceeds the maximum size"}}')],
	["bedrock_input_too_long", errorMessage("input is too long for requested model")],
	["openai_context_window", errorMessage("Your input exceeds the context window of this model")],
	["litellm_max_context", errorMessage("Requested token count exceeds the model's maximum context length of 131072 tokens")],
	["openai_compatible_parens", errorMessage("Input length (265330) exceeds model's maximum context length (262144).")],
	["google_input_token_count", errorMessage("The input token count (1196265) exceeds the maximum number of tokens allowed (1048575)")],
	["xai_max_prompt", errorMessage("This model's maximum prompt length is 131072 but the request contains 537812 tokens")],
	["groq_reduce", errorMessage("Please reduce the length of the messages or completion")],
	["openrouter_max_context", errorMessage("This endpoint's maximum context length is 200000 tokens. However, you requested about 300000 tokens")],
	["poolside_allowed_input", errorMessage("Input length 265330 exceeds the maximum allowed input length of 262144 tokens.")],
	["together_input_longer", errorMessage("The input (265330 tokens) is longer than the model's context length (262144 tokens).")],
	["copilot_limit", errorMessage("prompt token count of 300000 exceeds the limit of 200000")],
	["llamacpp_context_size", errorMessage("the request exceeds the available context size, try increasing it")],
	["lmstudio_keep", errorMessage("tokens to keep from the initial prompt is greater than the context length")],
	["minimax_window_limit", errorMessage("invalid params, context window exceeds limit")],
	["kimi_token_limit", errorMessage("Your request exceeded model token limit: 200000 (requested: 300000)")],
	["mistral_too_large", errorMessage("Prompt contains 300000 tokens ... too large for model with 200000 maximum context length")],
	["ds4_configured", errorMessage("Prompt has 300000 tokens, but the configured context size is 262144 tokens")],
	["zai_model_context_window", errorMessage("model_context_window_exceeded")],
	["ollama_exceeded", errorMessage("prompt too long; exceeded max context length by 10 tokens")],
	["dashscope_range", errorMessage("Range of input length should be [1, 131072]")],
	["generic_context_length", errorMessage("context_length_exceeded")],
	["generic_too_many_tokens", errorMessage("too many tokens")],
	["generic_token_limit", errorMessage("token limit exceeded")],
	["zai_prompt_exceeds_max", errorMessage("Prompt exceeds max length")],
	["throttling_excluded", errorMessage("ThrottlingException: Too many tokens, please wait before trying again.")],
	["throttling_prefix", errorMessage("Throttling error: Too many tokens")],
	["service_unavailable", errorMessage("Service unavailable: too many tokens")],
	["rate_limit", errorMessage("Rate limit reached for requests")],
	["too_many_requests", errorMessage("Too many requests")],
	["cerebras_413", errorMessage("413 (no body)", { provider: "cerebras" })],
	["cerebras_400_status", errorMessage("400 status code (no body)", { provider: "cerebras" })],
	["cerebras_429", errorMessage("429 (no body)", { provider: "cerebras" })],
	["cerebras_other_provider", errorMessage("413 (no body)")],
	["unrelated", errorMessage("something went wrong")],
	["empty_error", errorMessage("")],
	["silent_overflow", { role: "assistant", api: "fixture", provider: "fixture", model: "fixture",
		content: [], usage: usage(200001, 0), stopReason: "stop", timestamp: 0 }],
	["silent_within", { role: "assistant", api: "fixture", provider: "fixture", model: "fixture",
		content: [], usage: usage(199999, 0), stopReason: "stop", timestamp: 0 }],
	["length_zero_output", { role: "assistant", api: "fixture", provider: "fixture", model: "fixture",
		content: [], usage: usage(198000, 0), stopReason: "length", timestamp: 0 }],
	["length_with_output", { role: "assistant", api: "fixture", provider: "fixture", model: "fixture",
		content: [], usage: usage(198000, 5), stopReason: "length", timestamp: 0 }],
];
const overflow = overflowCorpus.map(([name, message]) => ({
	name,
	message,
	contextWindow: 200000,
	result: {
		withoutWindow: isContextOverflow(message),
		withWindow: isContextOverflow(message, 200000),
	},
}));
const recoverableCorpus = [
	["length_below_limit", { stopReason: "length", usage: usage(0, 5) }, 100],
	["length_at_limit", { stopReason: "length", usage: usage(0, 100) }, 100],
	["length_zero_desired", { stopReason: "length", usage: usage(0, 5) }, 0],
	["stop_reason", { stopReason: "stop", usage: usage(0, 5) }, 100],
	["error_reason", { stopReason: "error", usage: usage(0, 5) }, 100],
];
const recoverable = recoverableCorpus.map(([name, message, desired]) => ({
	name,
	message,
	desiredMaxOutput: desired,
	result: isRecoverableLength(message, desired),
}));

// ---------------------------------------------------------------------------
// 2. shouldCompact threshold table
// ---------------------------------------------------------------------------
const shouldCompactRows = [
	["enabled_below", 100, 1000, { enabled: true, reserveTokens: 100 }],
	["enabled_at_boundary", 900, 1000, { enabled: true, reserveTokens: 100 }],
	["enabled_above", 901, 1000, { enabled: true, reserveTokens: 100 }],
	["disabled", 999999, 1000, { enabled: false, reserveTokens: 100 }],
	["zero_reserve", 1000, 1000, { enabled: true, reserveTokens: 0 }],
].map(([name, contextTokens, contextWindow, settings]) => ({
	name,
	contextTokens,
	contextWindow,
	settings,
	result: shouldCompact(contextTokens, contextWindow, settings),
}));

// ---------------------------------------------------------------------------
// 3. _checkCompaction decision table (ordered side effects)
// ---------------------------------------------------------------------------
const SETTINGS = { enabled: true, reserveTokens: 100, keepRecentTokens: 20 };
const MODEL = { id: "fixture", provider: "fixture", api: "fixture", contextWindow: 1000, maxTokens: 100 };

function checkCase(config) {
	const calls = [];
	const branchEntries = config.branch ?? [];
	const source = Object.create(AgentSession.prototype);
	const shadow = (props) => {
		for (const [key, value] of Object.entries(props)) {
			Object.defineProperty(source, key, { value, writable: true, configurable: true, enumerable: true });
		}
	};
	shadow({
		model: config.model ?? MODEL,
		settingsManager: { getCompactionSettings: () => config.settings ?? SETTINGS },
		sessionManager: {
			getBranch: () => branchEntries,
			buildSessionProjection: () => buildSessionProjection(branchEntries),
			getEntries: () => branchEntries,
		},
		agent: { state: { messages: config.agentMessages ?? [] } },
		_entryIdsByMessage: new Map(),
		_overflowRecoveryAttempted: config.overflowRecoveryAttempted ?? false,
		_failedResponse: undefined,
		_runAutoCompaction: async (reason, willRetry) => {
			calls.push(`runAutoCompaction:${reason}:${willRetry}`);
			return config.runReturns ?? (reason === "overflow" ? willRetry : false);
		},
		_omitRecoveryAttempt: () => calls.push("omitRecoveryAttempt"),
		_emit: (event) => calls.push(`emit:${event.type}`),
		_emitSessionCompactFailed: async (event) =>
			calls.push(`session_compact_failed:${event.reason}:aborted=${event.aborted}:willRetry=${event.willRetry}`),
	});
	const message = config.message;
	if (config.assistantEntryId !== undefined) source._entryIdsByMessage.set(message, config.assistantEntryId);
	return (async () => {
		const result = await source._checkCompaction(
			message,
			config.skipAbortedCheck ?? true,
			config.toolResults ?? [],
		);
		return {
			calls,
			result,
			overflowRecoveryAttempted: source._overflowRecoveryAttempted,
			failedResponse: source._failedResponse === message ? config.assistantEntryId ?? true : null,
		};
	})();
}

const cases = [];
async function addCase(name, config, described) {
	const result = await checkCase(config);
	cases.push({
		name,
		described,
		settings: config.settings ?? SETTINGS,
		model: config.model ?? MODEL,
		skipAbortedCheck: config.skipAbortedCheck ?? true,
		overflowRecoveryAttempted: config.overflowRecoveryAttempted ?? false,
		assistantEntryId: config.assistantEntryId ?? null,
		branch: config.branch ?? [],
		agentMessages: config.agentMessages ?? [],
		message: config.message,
		runReturns: config.runReturns ?? null,
		result,
	});
}

const okAssistant = (id, parentId, extra = {}) => assistant(id, parentId, extra);
const overflowAssistant = (id, parentId, errorText, extra = {}) =>
	assistant(id, parentId, { stopReason: "error", errorMessage: errorText, content: [], usage: usage(0, 0), ...extra });

await addCase("disabled", {
	message: okAssistant("a1", "u1").message,
	branch: [user("u1", null, "hi"), okAssistant("a1", "u1")],
	settings: { enabled: false, reserveTokens: 100, keepRecentTokens: 20 },
}, "settings.enabled false returns before any check");
await addCase("aborted_skipped", {
	message: okAssistant("a1", "u1", { stopReason: "aborted" }).message,
	branch: [user("u1", null, "hi"), okAssistant("a1", "u1")],
}, "skipAbortedCheck true skips aborted messages");
await addCase("aborted_included", {
	message: okAssistant("a1", "u1", { stopReason: "aborted", usage: usage(950, 10) }).message,
	branch: [user("u1", null, "hi"), okAssistant("a1", "u1", { stopReason: "aborted", usage: usage(950, 10) })],
	skipAbortedCheck: false,
	assistantEntryId: "a1",
}, "skipAbortedCheck false continues into threshold checks");
await addCase("stale_before_compaction", {
	message: okAssistant("a1", "u1", { timestamp: 1000 }).message,
	branch: [user("u1", null, "hi"), okAssistant("a1", "u1", { timestamp: 1000 }), compactionEntry("c1", "a1")],
	assistantEntryId: "a1",
}, "assistant older than latest compaction boundary is skipped");
await addCase("explicit_overflow_retry", {
	message: overflowAssistant("a1", "u1", "prompt is too long: 1 tokens > 0 maximum").message,
	branch: [user("u1", null, "hi"), overflowAssistant("a1", "u1", "prompt is too long: 1 tokens > 0 maximum")],
	assistantEntryId: "a1",
	runReturns: true,
}, "explicit overflow error omits the attempt then compacts and retries");
await addCase("explicit_overflow_second_attempt", {
	message: overflowAssistant("a1", "u1", "prompt is too long: 1 tokens > 0 maximum").message,
	branch: [user("u1", null, "hi"), overflowAssistant("a1", "u1", "prompt is too long: 1 tokens > 0 maximum")],
	assistantEntryId: "a1",
	overflowRecoveryAttempted: true,
}, "a second overflow recovery attempt emits session_compact_failed");
await addCase("overflow_completed_response", {
	message: overflowAssistant("a1", "u1", "prompt is too long: 1 tokens > 0 maximum", { stopReason: "stop", usage: usage(1100, 5) }).message,
	branch: [user("u1", null, "hi"), overflowAssistant("a1", "u1", "prompt is too long: 1 tokens > 0 maximum", { stopReason: "stop", usage: usage(1100, 5) })],
	assistantEntryId: "a1",
}, "a completed response above the window compacts without retry");
await addCase("overflow_usage_after_edit", {
	message: overflowAssistant("a1", "u1", "prompt is too long: 1 tokens > 0 maximum").message,
	branch: [user("u1", null, "hi"), overflowAssistant("a1", "u1", "prompt is too long: 1 tokens > 0 maximum"),
		contextEdit("e1", "a1", "a1", { content: "kept" })],
	assistantEntryId: "a1",
	settings: { enabled: true, reserveTokens: 100000, keepRecentTokens: 20 },
}, "a post-assistant context edit suppresses explicit overflow recovery");
await addCase("overflow_after_compaction", {
	message: overflowAssistant("a1", "u1", "prompt is too long: 1 tokens > 0 maximum", { timestamp: 3000 }).message,
	branch: [user("u1", null, "hi"),
		overflowAssistant("a1", "u1", "prompt is too long: 1 tokens > 0 maximum", { timestamp: 3000 }),
		user("u2", "a1", "kept"),
		compactionEntry("c1", "u2", { firstKeptEntryId: "u2", timestamp: 2000 })],
	assistantEntryId: "a1",
}, "a summarized-away assistant with a later compaction suppresses explicit overflow recovery");
await addCase("silent_overflow", {
	message: okAssistant("a1", "u1", { usage: usage(1100, 0) }).message,
	branch: [user("u1", null, "hi"), okAssistant("a1", "u1", { usage: usage(1100, 0) })],
	assistantEntryId: "a1",
	runReturns: true,
}, "usage above the context window is overflow without an error message");
await addCase("silent_overflow_unprojected", {
	message: okAssistant("a1", "u1", { usage: usage(1100, 0) }).message,
	branch: [user("u1", null, "hi"), okAssistant("a1", "u1", { usage: usage(1100, 0) })],
	assistantEntryId: "elsewhere",
	settings: { enabled: true, reserveTokens: 100000, keepRecentTokens: 20 },
}, "an unprojected assistant suppresses silent overflow detection");
await addCase("recoverable_length", {
	message: okAssistant("a1", "u1", { stopReason: "length", usage: usage(10, 5) }).message,
	branch: [user("u1", null, "hi"), okAssistant("a1", "u1", { stopReason: "length", usage: usage(10, 5) })],
	assistantEntryId: "a1",
	runReturns: true,
}, "length below maxTokens is recoverable");
await addCase("unrecoverable_length", {
	message: okAssistant("a1", "u1", { stopReason: "length", usage: usage(10, 100) }).message,
	branch: [user("u1", null, "hi"), okAssistant("a1", "u1", { stopReason: "length", usage: usage(10, 100) })],
	assistantEntryId: "a1",
	settings: { enabled: true, reserveTokens: 100000, keepRecentTokens: 20 },
}, "length at maxTokens falls through to threshold, which is below reserve");
await addCase("other_model_overflow", {
	message: overflowAssistant("a1", "u1", "prompt is too long: 1 tokens > 0 maximum", { provider: "other" }).message,
	branch: [user("u1", null, "hi"), overflowAssistant("a1", "u1", "prompt is too long: 1 tokens > 0 maximum", { provider: "other" })],
	assistantEntryId: "a1",
	settings: { enabled: true, reserveTokens: 100000, keepRecentTokens: 20 },
}, "overflow from another model does not trigger compaction");
await addCase("threshold_direct_usage", {
	message: okAssistant("a1", "u1", { usage: usage(950, 10) }).message,
	branch: [user("u1", null, "hi"), okAssistant("a1", "u1", { usage: usage(950, 10) })],
	assistantEntryId: "a1",
}, "direct usage above window minus reserve triggers threshold compaction");
await addCase("threshold_below", {
	message: okAssistant("a1", "u1", { usage: usage(100, 10) }).message,
	branch: [user("u1", null, "hi"), okAssistant("a1", "u1", { usage: usage(100, 10) })],
	assistantEntryId: "a1",
}, "direct usage below the threshold does nothing");
await addCase("threshold_error_estimate", {
	message: okAssistant("a1", "u1", { stopReason: "error", errorMessage: "boom", usage: usage(0, 0) }).message,
	branch: [user("u1", null, "hi"), okAssistant("a1", "u1", { stopReason: "error", errorMessage: "boom", usage: usage(0, 0) })],
	assistantEntryId: "a1",
	agentMessages: [
		{ role: "user", content: "x".repeat(4000), timestamp: 0 },
		okAssistant("a1", "u1", { stopReason: "error", errorMessage: "boom", usage: usage(0, 0) }).message,
	],
}, "error responses estimate from message sizes");
await addCase("threshold_stale_usage_estimate", {
	message: okAssistant("a1", "u1", { stopReason: "error", errorMessage: "boom", usage: usage(0, 0), timestamp: 3000 }).message,
	branch: [okAssistant("a1", "u1", { stopReason: "error", errorMessage: "boom", usage: usage(0, 0), timestamp: 3000 }),
		compactionEntry("c1", "a1", { timestamp: 2000 })],
	assistantEntryId: "a1",
	agentMessages: [
		{ role: "assistant", api: "fixture", provider: "fixture", model: "fixture", content: [text("x".repeat(4000))],
			usage: usage(950, 10), stopReason: "stop", timestamp: 1000 },
	],
}, "stale pre-compaction usage estimates are rejected");
await addCase("threshold_with_context_edits", {
	message: okAssistant("a1", "u1", { usage: usage(10, 10) }).message,
	branch: [user("u1", null, "hi"), okAssistant("a1", "u1", { usage: usage(10, 10) }),
		contextEdit("e1", "a1", "u1", { content: "x".repeat(4000) })],
	assistantEntryId: "a1",
}, "context edits switch the threshold check to the projected estimate");

// ---------------------------------------------------------------------------
// 4. _runAutoCompaction ordered lifecycle operations
// ---------------------------------------------------------------------------
function lifecycleCase(config) {
	const ops = [];
	const events = [];
	const branchEntries = config.branch ?? [user("u1", null, "x".repeat(400)), okAssistant("a1", "u1", { content: [text("y".repeat(400))] })];
	const stored = [];
	const source = Object.create(AgentSession.prototype);
	const abortController = { signal: { aborted: config.aborted ?? false, throwIfAborted() {
		if (this.aborted) throw new Error("aborted");
	} } };
	const preparation = { firstKeptEntryId: "u1" };
	const shadow = (props) => {
		for (const [key, value] of Object.entries(props)) {
			Object.defineProperty(source, key, { value, writable: true, configurable: true, enumerable: true });
		}
	};
	shadow({
		model: config.hasModel === false ? null : MODEL,
		settingsManager: { getCompactionSettings: () => SETTINGS },
		sessionManager: {
			getBranch: () => branchEntries,
			getEntries: () => { ops.push("getEntries"); return stored; },
			appendCompaction: (...args) => {
				ops.push("appendCompaction");
				if (config.appendError) throw new Error("append failed");
				stored.push({ type: "compaction", id: "c1", summary: args[0], firstKeptEntryId: args[1], timestamp: 3000 });
			},
			buildSessionProjection: () => buildSessionProjection(branchEntries),
		},
		agent: { hasQueuedMessages: () => { ops.push("hasQueuedMessages"); return config.queued ?? false; } },
		_emit: (event) => { ops.push(`emit:${event.type}`); events.push({ type: event.type, reason: event.reason ?? null,
			aborted: event.aborted ?? null, willRetry: event.willRetry ?? null, errorMessage: event.errorMessage ?? null }); },
		_extensionRunner: {
			hasHandlers: () => config.extensionHandlers ?? false,
			emit: async (event) => {
				ops.push(`extension:${event.type}`);
				events.push({ type: event.type, reason: event.reason ?? null, willRetry: event.willRetry ?? null });
				if (event.type === "session_before_compact") {
					if (config.cancel) return { cancel: true };
					if (config.extensionCompaction) return { compaction: { summary: "ext", firstKeptEntryId: "u1", tokensBefore: 1 } };
				}
			},
		},
		_runDefaultCompaction: async () => {
			ops.push("runDefaultCompaction");
			if (config.defaultError) throw new Error("summary failed");
			return { summary: "sum", firstKeptEntryId: "u1", tokensBefore: 1, usage: undefined, details: undefined };
		},
		_refreshFinalizedContext: () => ops.push("refreshFinalizedContext"),
		_emitSessionCompactFailed: async (event) => {
			ops.push("emitSessionCompactFailed");
			events.push({ type: "session_compact_failed", reason: event.reason, aborted: event.aborted,
				willRetry: event.willRetry, errorMessage: event.errorMessage ?? null });
		},
		_clearManualCompactionState: () => {},
		_resolveIdleWaitIfIdle: () => ops.push("resolveIdleWaitIfIdle"),
	});
	// Replace the module-level AbortController with the injected one.
	const originalAbortController = globalThis.AbortController;
	globalThis.AbortController = function () { return abortController; };
	return (async () => {
		try {
			const result = await source._runAutoCompaction(config.reason ?? "threshold", config.willRetry ?? false);
			return { ops, events, result, threw: false };
		} catch (error) {
			return { ops, events, result: null, threw: true, errorMessage: error.message };
		} finally {
			globalThis.AbortController = originalAbortController;
		}
	})();
}

const lifecycle = [];
async function addLifecycle(name, config, described) {
	const branch = config.branch ?? [user("u1", null, "x".repeat(400)), okAssistant("a1", "u1", { content: [text("y".repeat(400))] })];
	lifecycle.push({ name, described, reason: config.reason ?? "threshold", willRetry: config.willRetry ?? false,
		inputs: {
			hasModel: config.hasModel !== false,
			hasPreparation: prepareCompaction(branch, SETTINGS) !== undefined,
			extensionHandlers: config.extensionHandlers ?? false,
			extensionCancel: config.cancel ?? false,
			extensionCompaction: config.extensionCompaction ?? false,
			defaultError: config.defaultError ?? false,
			appendError: config.appendError ?? false,
			aborted: config.aborted ?? false,
			queued: config.queued ?? false,
		},
		...(await lifecycleCase(config)) });
}
await addLifecycle("success", {}, "threshold compaction emits start, commits, refreshes, emits session_compact and end");
await addLifecycle("success_queued", { queued: true }, "queued messages request one continuation");
await addLifecycle("success_will_retry", { reason: "overflow", willRetry: true }, "overflow retry returns true without consulting the queue");
await addLifecycle("extension_compaction", { extensionHandlers: true, extensionCompaction: true }, "extension-provided compaction skips the default generator");
await addLifecycle("extension_cancel", { extensionHandlers: true, cancel: true }, "extension cancellation aborts before any commit");
await addLifecycle("default_error", { defaultError: true }, "a summary failure emits compaction_end then session_compact_failed");
await addLifecycle("append_error", { appendError: true }, "a commit failure emits compaction_end then session_compact_failed");
await addLifecycle("no_model", { hasModel: false }, "no model returns false before preparation");
await addLifecycle("no_preparation", { branch: [user("u1", null, "hi"), okAssistant("a1", "u1"), compactionEntry("c1", "a1")] }, "no preparation returns false before emitting start");
await addLifecycle("abort_after_start", { aborted: true }, "a signal aborted after start emits end and failed");
await addLifecycle("no_extension_handlers", { extensionHandlers: false }, "no session_before_compact handlers skips the extension emit");
await addLifecycle("extension_default", { extensionHandlers: true }, "a handler that returns nothing keeps the default generator");

const fixture = {
	reference: "Pi 1.3.0",
	upstream: hashes,
	overflow: { contextWindow: 200000, rows: overflow },
	recoverableLength: recoverable,
	shouldCompact: shouldCompactRows,
	checkCompaction: cases,
	autoCompaction: lifecycle,
};

const destination = new URL("compaction-trigger-reference.json", import.meta.url);
const serialized = `${JSON.stringify(fixture, null, 2)}\n`;
if (process.argv.includes("--write")) await writeFile(destination, serialized);
else assert.equal(await readFile(destination, "utf8"), serialized);
process.stdout.write(
	`${overflow.length} overflow, ${recoverable.length} length, ${shouldCompactRows.length} threshold, ` +
		`${cases.length} trigger, ${lifecycle.length} lifecycle rows verified\n`,
);
