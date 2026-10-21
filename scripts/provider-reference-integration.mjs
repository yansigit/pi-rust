// Run from a standalone install containing all three packed references and Pi 1.3.0.
// Usage: node /path/to/pi-rust/scripts/provider-reference-integration.mjs /path/to/install
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const install = resolve(process.argv[2]);
const home = await mkdtemp(join(tmpdir(), 'rpi-provider-reference-'));
const oldHome = process.env.PI_CODING_AGENT_DIR;
const oldFetch = globalThis.fetch;
process.env.PI_CODING_AGENT_DIR = home;
globalThis.fetch = async () => new Response('', { status: 503 });
let session;
try {
  const hostRoot = join(install, 'node_modules/@earendil-works/pi-coding-agent');
  const hostManifest = JSON.parse(readFileSync(join(hostRoot, 'package.json'), 'utf8'));
  const { createAgentSession, DefaultResourceLoader, ModelRuntime, SessionManager, SettingsManager, VERSION } = await import(pathToFileURL(join(hostRoot, hostManifest.exports['.'].import)).href);
  assert.equal(VERSION, '1.3.0');
  const packages = ['@cortexkit/pi-antigravity-auth', 'pi-cursor-sdk', 'pi-multiprovider'];
  const paths = packages.map(name => join(install, 'node_modules', name));
  for (let i = 0; i < paths.length; i++) {
    assert.equal(JSON.parse(readFileSync(join(paths[i], 'package.json'), 'utf8')).name, packages[i]);
  }
  const settingsManager = SettingsManager.inMemory({ packages: paths, compaction: { enabled: false }, retry: { enabled: false } });
  const resourceLoader = new DefaultResourceLoader({ cwd: home, agentDir: home, settingsManager,
    noSkills: true, noPromptTemplates: true, noThemes: true, noContextFiles: true });
  await resourceLoader.reload();
  const loaded = resourceLoader.getExtensions();
  assert.deepEqual(loaded.errors, []);
  assert.deepEqual(loaded.warnings ?? [], []);
  assert.equal(loaded.extensions.length, 3);
  const modelRuntime = await ModelRuntime.create({ authPath: join(home, 'auth.json'), modelsPath: null,
    modelsStorePath: join(home, 'models-cache'), allowModelNetwork: false });
  ({ session } = await createAgentSession({ cwd: home, agentDir: home, resourceLoader, modelRuntime,
    settingsManager, sessionManager: SessionManager.inMemory(home) }));
  const errors = [];
  session.extensionRunner.onError(error => errors.push(error));
  await session.bindExtensions({});
  await session.extensionRunner.emit({ type: 'session_shutdown', reason: 'quit' });
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ host: VERSION, packages, extensions: loaded.extensions.length,
    errors, qualification: 'offline load/startup/shutdown only; inference parity not tested' }));
} finally {
  session?.dispose();
  globalThis.fetch = oldFetch;
  if (oldHome === undefined) delete process.env.PI_CODING_AGENT_DIR;
  else process.env.PI_CODING_AGENT_DIR = oldHome;
  await rm(home, { recursive: true, force: true });
}
