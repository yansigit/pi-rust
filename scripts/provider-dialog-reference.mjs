// Execute the pinned host's search policy; never infer rankings from README text.
import assert from 'node:assert/strict';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { resolve, dirname } from 'node:path';
import { pathToFileURL, fileURLToPath } from 'node:url';

assert.equal(process.versions.node.split('.')[0], '24', 'use the Node 24 reference runtime');
const installed = resolve(process.argv[2]);
const source = resolve(installed, 'node_modules/@earendil-works/pi-tui/dist/fuzzy.js');
const pkg = JSON.parse(readFileSync(resolve(installed, 'node_modules/@earendil-works/pi-tui/package.json'), 'utf8'));
assert.equal(pkg.version, '1.3.0');
const { fuzzyFilter } = await import(pathToFileURL(source));
const cases = [];
function record(query, labels) {
  cases.push({ query, labels, expected: fuzzyFilter(labels, query, value => value) });
}
const labels = ['Work', 'work one', 'Work Two', 'Personal', 'Account 2', '2 Account', 'Grok 4.7',
  'Composer 2.5', 'Google/Flash', 'ＡＢ', 'İstanbul', 'ΣΟΣ', 'ΟΣ', '😀account', '😀a😀b',
  '\ufeffaccount', '\u0085account', 'a/b', 'a-b', 'a_b', 'a.b', 'a:b', 'a b', 'a123b', '123ab',
  'abcdefghijklmn', 'aXXb', 'XaXb', 'a12-!', '12-a', 'a\u00a0b', 'a\u2000b', 'a\u2028b'];
for (const query of ['', 'work', 'a', 'ab', 'a b', 'a/b', 'Account2', '2Account', 'a12-!', '1ab2',
  'a\u0085b', '\ufeffa', 'a\ufeffb', 'i̇', 'ΟΣ', '😀ab', '😀', 'Ａ', 'Grok47', 'account 😀']) record(query, labels);
let seed = 0x1302026;
const random = max => { seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0; return seed % max; };
const alphabet = [...'abAB012_ ./:😀Σİ', '\ufeff', '\u0085', '\u2028'];
const word = length => Array.from({ length }, () => alphabet[random(alphabet.length)]).join('');
for (let i = 0; i < 256; i++) record(word(1 + random(5)), Array.from({ length: 20 }, () => word(1 + random(18))));
const output = resolve(dirname(fileURLToPath(import.meta.url)), '../crates/rpi-cli/tests/fixtures/dialog-search-pi-1.3.0.json');
mkdirSync(dirname(output), { recursive: true });
writeFileSync(output, JSON.stringify({ host: pkg.version, node: process.version,
  sourceSha256: createHash('sha256').update(readFileSync(source)).digest('hex'), cases }, null, 2) + '\n');
console.log(`${cases.length} executable reference cases written`);
