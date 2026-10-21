// Conservative Pi runtime closure; this does not prove which branches execute.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {createRequire} from 'node:module';
import {fileURLToPath} from 'node:url';
const references = path.resolve(process.argv[2] ?? '../provider-references');
const ts = createRequire(path.join(references, 'cursor/package.json'))('@typescript/typescript6');
const preferred = path.join(references, 'agy-reference');
const root = fs.existsSync(preferred) ? preferred : path.join(references, 'antigravity');
const core = 'packages/core/src/';
const cache=new Map(),seen=new Map();
function parse(p){if(!cache.has(p))cache.set(p,ts.createSourceFile(p,fs.readFileSync(path.join(root,p),'utf8'),ts.ScriptTarget.Latest,true));return cache.get(p)}
function resolve(p,s){if(s==='@cortexkit/antigravity-auth-core')return core+'index.ts';if(s.startsWith('@cortexkit/antigravity-auth-core/')){const tail=s.slice('@cortexkit/antigravity-auth-core/'.length);return core+(tail.startsWith('common-auth/')?'common-auth-embedded/'+tail.slice(12)+'/index.js':tail+'.ts')};if(s.startsWith('.'))return path.normalize(path.join(path.dirname(p),s));return null}
function declarations(n){if(ts.isVariableStatement(n))return n.declarationList.declarations.map(d=>d.name.getText());return n.name?[n.name.getText()]:[]}
function exportFile(p,name,trail=new Set()){if(trail.has(p))return null;trail.add(p);for(const n of parse(p).statements){if(declarations(n).includes(name))return p;if(ts.isExportDeclaration(n)&&!n.isTypeOnly){if(n.exportClause&&ts.isNamedExports(n.exportClause)){for(const e of n.exportClause.elements)if(!e.isTypeOnly&&e.name.text===name){return n.moduleSpecifier?exportFile(resolve(p,n.moduleSpecifier.text),e.propertyName?.text??e.name.text,trail):p}}else if(n.moduleSpecifier){const q=resolve(p,n.moduleSpecifier.text);if(q){const found=exportFile(q,name,new Set(trail));if(found)return found}}}}return null}
const loaders={loadCommonAuthClaustrum:'claustrum',loadCommonAuthCommands:'commands',loadCommonAuthPiSlot:'pi-slot',loadCommonAuthFs:'fs',loadCommonAuthLogger:'logger',loadCommonAuthStore:'store',loadCommonAuthRouting:'routing',loadCommonAuthQuota:'quota',loadCommonAuthAuthMenu:'auth-menu'};
function walk(p,names,from){if(!p)return;const old=seen.get(p);if(!old)seen.set(p,{from:new Set(),names:new Set()});const row=seen.get(p);row.from.add(from);const added=names.filter(n=>!row.names.has(n));added.forEach(n=>row.names.add(n));if(old&&!added.length)return;
 if(p===core+'common-auth-runtime.ts'){for(const n of added)if(loaders[n])walk(core+'common-auth-embedded/'+loaders[n]+'/index.js',['*'],p);return}
 if(p===core+'index.ts'||p===core+'transform/index.ts'){for(const n of added){if(n==='*')throw Error('unexpected core namespace');const q=exportFile(p,n);if(!q)throw Error('missing export '+n);walk(q,[n],p)}return}
 const source=parse(p);for(const n of source.statements){if(ts.isImportDeclaration(n)){const c=n.importClause;if(c?.isTypeOnly)continue;const names=!c?['*']:c.namedBindings&&ts.isNamedImports(c.namedBindings)?c.namedBindings.elements.filter(e=>!e.isTypeOnly).map(e=>e.propertyName?.text??e.name.text):['*'];if(c?.name)names.push('default');if(names.length)walk(resolve(p,n.moduleSpecifier.text),names,p)}else if(ts.isExportDeclaration(n)&&!n.isTypeOnly&&n.moduleSpecifier){const names=n.exportClause&&ts.isNamedExports(n.exportClause)?n.exportClause.elements.filter(e=>!e.isTypeOnly).map(e=>e.propertyName?.text??e.name.text):['*'];walk(resolve(p,n.moduleSpecifier.text),names,p)}}}
walk('packages/pi/src/index.ts',['default'],'Pi extension entry');
assert(seen.has('packages/pi/src/vault-source.ts'));
assert(seen.has(core + 'common-auth-embedded/pi-slot/slot.js'));
assert(!seen.has(core + 'account-manager.ts'));
const modules = Object.fromEntries([...seen].sort().map(([p, v]) => [p, {
  from: [...v.from].sort(), names: [...v.names].sort(),
}]));
const output = process.argv[3] ?? path.join(path.dirname(fileURLToPath(import.meta.url)), '../docs/provider-parity/antigravity-reachability.json');
fs.writeFileSync(output, JSON.stringify({
  reference: 'a97f9055017f3c1546afbfe61a19d50b428be3d8',
  entry: 'packages/pi/src/index.ts',
  method: 'Runtime imports excluding type-only bindings; named core barrel export resolution; explicit lazy common-auth loader edges. Includes unused branches within reached modules. Dynamic external SDK behavior remains separately required.',
  modules,
}, null, 2) + '\n');
console.log(`${seen.size} conservative runtime modules written to ${output}`);
