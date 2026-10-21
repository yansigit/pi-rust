// Enumerate source modules and test declarations without executing upstream code.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const references = JSON.parse(readFileSync(join(root, 'docs/provider-parity/references.json'), 'utf8'));
const directory = resolve(process.argv[2] ?? '../provider-references');
// Reuse Cursor's locked compiler API; TypeScript 7 intentionally has none.
const ts = createRequire(join(directory, 'cursor/package.json'))('@typescript/typescript6');

function files(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap(entry =>
    entry.isDirectory() ? files(join(directory, entry.name)) : [join(directory, entry.name)]);
}

function testDeclarations(text, path) {
  const source = ts.createSourceFile(path, text, ts.ScriptTarget.Latest, true);
  const tests = [];
  function visit(node) {
    if (ts.isCallExpression(node)) {
      const callee = node.expression.getText(source);
      // Include parameterized/conditional tests as declarations. Runtime counts
      // belong to the runner; a declaration is not a claim that its test passed.
      if (/^(?:it|test)(?:\.(?:skip|only|todo|concurrent|each|skipIf|runIf)|\s*\()*/.test(callee)
          && /^(?:it|test)(?:\.|\(|$)/.test(callee)
          && node.arguments.length && ts.isStringLiteralLike(node.arguments[0])) {
        tests.push({ line: source.getLineAndCharacterOfPosition(node.getStart()).line + 1,
          title: node.arguments[0].text, invocation: callee });
      }
    }
    ts.forEachChild(node, visit);
  }
  visit(source);
  return tests;
}

assert.deepEqual(testDeclarations('it("one", () => {}); it.each([1])("case %s", () => {}); test.skip("later", () => {});', 'self-test.ts').map(row => row.title), ['one', 'case %s', 'later']);

const rows = [];
for (const [name, reference] of Object.entries(references.providers)) {
  const checkout = join(directory, name);
  const paths = [...new Set([...reference.sourceRoots, ...reference.testRoots].flatMap(path => files(join(checkout, path))))].sort();
  for (const path of paths) {
    if (!/\.(?:[cm]?[jt]sx?)$/.test(path) || /\.d\.(?:[cm]?ts)$/.test(path)) continue;
    const upstream = relative(checkout, path).replaceAll('\\', '/');
    const text = readFileSync(path, 'utf8');
    const isTest = /\.(test|spec)\.[cm]?[jt]sx?$/.test(path);
    const declarations = isTest ? testDeclarations(text, path) : [];
    const port = name === 'cursor'
      ? `packages/${reference.destination}/worker/${upstream}`
      : `packages/${reference.destination}/src/${upstream.replace(/^(?:packages\/)?/, '').replace(/\.[^.]+$/, '.rs').replaceAll('-', '_')}`;
    const base = { provider: name, upstream, sha256: createHash('sha256').update(text).digest('hex'),
      kind: isTest ? 'test-file' : 'source-or-fixture', intendedPort: port,
      acceptance: `packages/${reference.destination}/tests/parity/${upstream}`,
      status: 'pending', evidence: [],
      scope: name === 'antigravity' && !upstream.startsWith('packages/pi/') ? 'shared-reachability-review-required' : 'required' };
    rows.push({ id: `${name}:${upstream}`, ...base });
    for (const declaration of declarations) {
      rows.push({ ...base, id: `${name}:${upstream}:${declaration.line}`, kind: 'test-declaration', ...declaration });
    }
  }
}
assert.equal(new Set(rows.map(row => row.id)).size, rows.length, 'ledger identities must be unique');
const output = process.argv[3] ? resolve(process.argv[3]) : join(root, 'docs/provider-parity/inventory.jsonl');
writeFileSync(output, rows.map(row => JSON.stringify(row)).join('\n') + '\n');
console.log(`${rows.length} pending inventory rows written to ${output}`);
