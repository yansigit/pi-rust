// Offline differential oracle: unchanged Pi 1.3.0 public entry points only.
import { readFile, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { pathToFileURL } from 'node:url';
import assert from 'node:assert/strict';

assert.equal(process.versions.node.split('.')[0], '24');
const root = '/home/user/Projects/provider-references/integrated/node_modules/';
const sources = {
  '@earendil-works/pi-ai/dist/api/openai-completions.js': '4b3875e5ec18c5e2d06ce12409feb2554563fd2c392a5c1773af968b265e2bea',
  '@earendil-works/pi-coding-agent/dist/core/system-prompt.js': 'ab02924fa078eef81addcf39bc0e43d3663f06d627cb42ead403662a004a9e5c',
};
for (const [file, hash] of Object.entries(sources)) {
  assert.equal(createHash('sha256').update(await readFile(root + file)).digest('hex'), hash, file);
}
const { stream } = await import(pathToFileURL(root + Object.keys(sources)[0]));
const { buildSystemPrompt } = await import(pathToFileURL(root + Object.keys(sources)[1]));
// forceSystemPrompt is opaque upstream, unlike customPrompt. The Rust CLI has
// no forceSystemPrompt entry point; do not treat its custom prompt as forced.
assert.equal(buildSystemPrompt({ customPrompt: 'custom', forceSystemPrompt: 'forced', cwd: '/work' }), 'forced');
const prompts = [
  { name: 'custom', customPrompt: 'Custom instructions.', cwd: '/work/repo' },
  { name: 'loaded', customPrompt: 'Loaded SYSTEM.md instructions.', cwd: '/work/loaded' },
  { name: 'resumed', customPrompt: 'Resumed instructions.', cwd: '/work/resumed' },
  { name: 'windows', customPrompt: 'Custom instructions.', cwd: 'C:\\work\\repo' },
].map(input => ({ ...input, result: buildSystemPrompt(input) }));
const requests = [];
async function request(name, provider, urlHint, reasoning, overrides = {}, content = [{ type: 'text', text: 'Hello.' }], systemPrompt = 'Instructions.') {
  // These overrides intentionally isolate unrelated default-role/thinking-format
  // differences. supportsStore/maxTokensField are NEVER overridden except in
  // the two explicitly named override cases.
  const compat = { supportsDeveloperRole: false, supportsReasoningEffort: false, thinkingFormat: 'openai', ...overrides };
  const model = {
    id: 'fixture', name: 'fixture', api: 'openai-completions', provider,
    baseUrl: `http://127.0.0.1/${urlHint}/v1`, reasoning, input: ['text', 'image'],
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 8192, maxTokens: 1024, compat,
  };
  const context = { messages: [
    { role: 'system', content: systemPrompt, timestamp: 0 },
    { role: 'user', content, timestamp: 1 },
  ] };
  let payload;
  const response = stream(model, context, {
    apiKey: 'offline-fixture', maxTokens: 37, cacheRetention: 'none', maxRetries: 0,
    onPayload: value => { payload = structuredClone(value); },
    fetch: async () => new Response('data: {"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n', { headers: { 'content-type': 'text/event-stream' } }),
  });
  const result = await response.result();
  assert.equal(result.stopReason, 'stop', result.errorMessage);
  assert.ok(payload);
  requests.push({ name, model, content, systemPrompt, result: payload });
}
const providers = ['openai', 'nvidia', 'cerebras', 'xai', 'together', 'deepseek', 'zai', 'zai-coding-cn', 'moonshotai', 'moonshotai-cn', 'opencode', 'cloudflare-workers-ai', 'cloudflare-ai-gateway', 'ant-ling', 'unknown'];
const hints = ['integrate.api.nvidia.com', 'cerebras.ai', 'api.x.ai', 'api.together.ai', 'api.together.xyz', 'chutes.ai', 'DEEPSEEK.COM', 'DEEPSEEK.COM', 'API.Z.AI', 'api.z.ai', 'open.bigmodel.cn', 'api.moonshot.ai', 'opencode.ai', 'api.cloudflare.com', 'gateway.ai.cloudflare.com', 'api.ant-ling.com'];
for (const reasoning of [false, true]) {
  for (const provider of providers) await request(`${provider}-${reasoning}`, provider, 'neutral', reasoning);
  for (const hint of hints) await request(`url-${hint}-${reasoning}`, 'custom', hint, reasoning);
}
await request('override-enable-store-completion', 'deepseek', 'neutral', true, { supportsStore: true, maxTokensField: 'max_completion_tokens' });
await request('override-disable-store-max', 'openai', 'neutral', false, { supportsStore: false, maxTokensField: 'max_tokens' });
await request('direct-string-preserved', 'openai', 'neutral', false, {}, 'Hello.');
await request('ordered-images', 'openai', 'neutral', false, {}, [
  { type: 'text', text: 'Hello.' },
  { type: 'image', mimeType: 'image/png', data: 'aGVsbG8=' },
  { type: 'image', mimeType: 'image/jpeg', data: 'd29ybGQ=' },
]);
for (const prompt of prompts) await request(`prompt-${prompt.name}`, 'openai', 'neutral', false, {}, undefined, prompt.result);
const fixture = JSON.stringify({ sources, prompts, requests }, null, 2) + '\n';
const destination = new URL('./request-parity-reference.json', import.meta.url);
if (process.argv.includes('--write')) await writeFile(destination, fixture);
else assert.equal(await readFile(destination, 'utf8'), fixture, 'reference differs; regenerate with --write');
console.log(`${requests.length} complete request bodies; ${prompts.length} complete system prompts verified`);
