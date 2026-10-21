import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { resolve, join } from "node:path";
import { pathToFileURL } from "node:url";

const root = resolve(process.argv[2] ?? "../provider-references/integrated");
const ai = join(root, "node_modules/@earendil-works/pi-ai");
assert.equal(JSON.parse(await readFile(join(ai, "package.json"), "utf8")).version, "1.3.0");
const { createModels } = await import(pathToFileURL(join(ai, "dist/index.js")).href);
function gate() {
	let resolve;
	const promise = new Promise(done => { resolve = done; });
	return { promise, resolve };
}
function provider(id, refreshModels) {
	return { id, name: id, auth: {}, getModels: () => [], refreshModels };
}
async function fixture() {
	const models = createModels(), callbacks = [], started = [];
	models.setProvider(provider("a", async context => {
		const done = gate(); callbacks.push({ context, done }); started.shift()?.(); await done.promise;
	}));
	const begin = () => {
		const ready = new Promise(resolve => { started.push(resolve); });
		const result = models.refresh({ providers: ["a"], allowNetwork: false });
		return ready.then(() => ({ ...callbacks.at(-1), result }));
	};
	return { models, callbacks, begin };
}
const output = { pi: "1.3.0", checks: {} };
{
	const f = await fixture(), a = await f.begin();
	f.models.setProvider(provider("b", async () => {}));
	let applied = false;
	const accepted = await a.context.publish({ update: () => { applied = true; } });
	a.done.resolve(); await a.result;
	output.checks.unrelatedProvider = { cancelled: a.context.signal.aborted, accepted, applied };
}
{
	const f = await fixture(), a = await f.begin(), b = await f.begin();
	let oldApplied = false, newApplied = false;
	try { await a.context.publish({ update: () => { oldApplied = true; } }); } catch (error) {
		assert(a.context.signal.aborted); assert.equal(error.name, "AbortError");
	}
	const accepted = await b.context.publish({ update: () => { newApplied = true; } });
	b.done.resolve(); await b.result; a.done.resolve(); await a.result;
	output.checks.supersededRefresh = { cancelled: a.context.signal.aborted, oldApplied, accepted, newApplied };
}
{
	const f = await fixture(), a = await f.begin();
	f.models.setProvider(provider("a", async () => {}));
	output.checks.manualReplacement = { cancelled: a.context.signal.aborted };
	a.done.resolve(); await a.result;
}
{
	const f = await fixture(), a = await f.begin();
	const result = await f.models.refresh({ providers: ["a"], allowNetwork: false, signal: AbortSignal.abort() });
	output.checks.cancelledBeforeStart = { aborted: result.aborted, previousCancelled: a.context.signal.aborted, calls: f.callbacks.length };
	a.done.resolve(); await a.result;
}
process.stdout.write(`${JSON.stringify(output, null, 2)}\n`);
