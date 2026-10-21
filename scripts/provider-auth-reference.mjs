// Capture auth-resolution state from the pinned Pi 1.3.0 implementation.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const install = resolve(process.argv[2]);
const root = join(install, 'node_modules/@earendil-works/pi-ai');
assert.equal(JSON.parse(readFileSync(join(root, 'package.json'), 'utf8')).version, '1.3.0');
const { resolveProviderAuth } = await import(pathToFileURL(join(root, 'dist/auth/resolve.js')).href);
const originalNow = Date.now;
Date.now = () => 1_000_000;
const cases = [];
try {
  for (const [name, expires, refreshedExpiry, concurrent] of [
    ['valid', 1_300_001, 2_000_000, false],
    ['five-minute-boundary', 1_300_000, 2_000_000, false],
    ['expired', 0, 2_000_000, false],
    ['short-refreshed-token', 1, 1_100_000, false],
    ['concurrent', 1, 2_000_000, true],
  ]) {
    let credential = { type: 'oauth', access: 'old-access', refresh: 'old-refresh', expires };
    let tail = Promise.resolve();
    let refreshes = 0, writes = 0;
    const store = {
      read: async () => structuredClone(credential),
      modify: async (_provider, update, options) => {
        const work = tail.then(async () => {
          options?.signal?.throwIfAborted();
          const replacement = await update(structuredClone(credential));
          if (replacement !== undefined) { credential = replacement; writes++; }
          return structuredClone(credential);
        });
        tail = work.catch(() => {});
        return work;
      },
    };
    const provider = { id: 'test', auth: { oauth: {
      refresh: async previous => {
        refreshes++;
        return { ...previous, access: 'new-access', refresh: 'rotated-refresh', expires: refreshedExpiry };
      },
      toAuth: async current => ({ apiKey: current.access }),
    } } };
    const context = { env: async () => undefined, fileExists: async () => false };
    const requests = concurrent ? 2 : 1;
    const results = await Promise.all(Array.from({ length: requests }, () => resolveProviderAuth(provider, store, context)));
    cases.push({ name, expires, refreshedExpiry, requests, refreshes, writes, credential, results });
  }
} finally {
  Date.now = originalNow;
}
console.log(JSON.stringify({ reference: 'earendil-works/pi-mono@71023aec1546d6d677a65669c5665c0927ce0dd0', cases }, null, 2));
