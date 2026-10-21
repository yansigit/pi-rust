# Provider compatibility work

Status: **not parity complete; the three packages have not been implemented**.

The host baseline is `90093c9e2d71ee9ce8228c1c27dc3262931b4a60`.
Package work belongs in the separate [rpi-package fork](https://github.com/yansigit/rpi-package),
on `codex/provider-parity`, based on `feature-1014` at
`88d305803dd58434c9a36059d28b0a2328d6d927`. Existing provider packages stay unchanged.
No workspace version bump is authorized by this work.

[references.json](references.json) pins the three upstream implementations,
Pi 1.3.0, Cursor SDK 1.0.37, and unchanged baseline commands. Source behavior is
authoritative. Baseline failures are evidence about the reference, never a
passing candidate test or an automatically approved exception.

The initial host foundation adds `rpi_ai::auth` and
`rpi_cli::config::auth_store::CompatibilityCredentialStore`. Existing login and
logout mutations now share the store lock; the opt-in resolver implements
millisecond expiry, provider-owned auth derivation, double-checked refresh,
independent refresh timeout, persistence after caller cancellation and an awaited
shutdown drain. An appended SDK auth registration slot and the
[native callback bridge](native-auth.md) support method discovery, login
continuations, refresh and request-auth derivation, with a separate legacy OAuth
adapter. Registered native auth now resolves at the physical request boundary;
legacy providers keep their existing resolver. Replacement attempts discard bound
credentials, credential headers, request environment and account endpoint overrides.
The SDK preserves replacement isolation across the native request boundary. Public
models cannot redirect a bound credential. Native CLI login, method discovery,
refresh and logout use the same locked store; shutdown drains refresh persistence.
Reload validates its replacement before shutting down the old session.

Response status and lossless header values have a private, awaited observation port
through built-in transports and the native SDK. Native receipt queues are drained
while the provider is opening its stream. Observer events omit keys and header
values. H01–H03 remain pending until the native runtime/delegation API, interactive
TUI callers and the complete differential suite are integrated.

[baseline-results.json](baseline-results.json) records upstream results, lock
hashes and evidence hashes. Cross-platform and inference acceptance remain untested.
Read-only live discovery with one authorized Cursor account confirmed Composer 2.5
and Grok 4.7, including their `fast=false` setting; this is not inference acceptance.
`scripts/provider-auth-reference.mjs INSTALL_DIRECTORY` executes Pi 1.3.0's
auth resolver to regenerate `crates/rpi-ai/tests/fixtures/auth-pi-1.3.0.json`.
The Rust fixture test compares resulting credentials, auth/source, refresh counts
and write counts for five scenarios. Cancellation, timeout, lock re-check and
shutdown additionally have deterministic Rust tests; they are not yet the full
native callback differential suite.

## Ledger

[inventory.jsonl](inventory.jsonl) records source/test file hashes and individual
statically named test declarations. Parameterized declarations are recorded once;
the upstream runner owns their expanded case counts. Intended port and acceptance
paths are **planned**, not existing implementations. Every row starts pending.
The shared Antigravity core inventory is deliberately conservative: barrel exports,
embedded common-auth and shared test infrastructure need symbol-level reachability
review before rows can be excluded. This inventory is not yet a completed parity
ledger. Runtime-generated test names and imported test declarations need review too.

The behavioral ledger below supplements that file inventory. Each row requires
passing differential evidence before completion. All rows are currently pending.
The only agreed exceptions are native rpi presentation, native-only extension
contracts, isolated import storage and fresh runtime ownership; original Pi keeps
authority over existing Cursor agents and vault enrollments.

| ID | Required behavior | Source / intended port | Acceptance evidence required |
| --- | --- | --- | --- |
| H01 | Provider-owned auth methods, nested login, continuation, refresh, credential-to-request conversion; legacy OAuth adapter | Pi `ai/src/auth`; SDK + extensions + CLI | Native library callback tests, old OAuth regression tests |
| H02 | Five-minute validity, lock double-check, 15-second refresh deadline, persist rotation after caller cancellation | Pi `ai/src/auth/resolve.ts`; host credential resolver/store | Injected clock, concurrent requests/processes, cancelled waiter, persistence failure |
| H03 | Private ambient/replace binding; no default credential, credential header, account endpoint or environment restoration; model descriptors cannot redirect secrets | Pi auth + multiprovider `lift.ts`; host binding | Two-account requests with poisoned public descriptors; no process environment mutation |
| H04 | Nonrecursive delegated streaming, session/generation ownership, re-registration, parent/child isolation, invocation keepalive | Pi native providers + multiprovider registration; SDK + extensions | Nested wrappers, unload during calls, reload once, short vtables |
| H05 | Provider inspection, dynamic catalog publication/refresh, private response status/headers | Pi model runtime; SDK + extensions | Real native callback/response ordering and secret isolation |
| H06 | Ordered awaited result-bearing hooks; request/turn/operation/session IDs; compaction failure events | Pi extension runner; host event dispatch | Ordered modification and lifecycle traces; cancellation |
| H07 | Scoped versioned services and bounded subscriptions | Multiprovider `announcement.ts`, `register.ts`; SDK registry | Inventory/change services, owner unload, slow consumers |
| H08 | Real tool invocation through existing hooks/context/cancellation; separate inert recorded activity | Cursor bridge/replay; host actions + UI | Execution counts: one Cursor, one bridge, zero replay |
| H09 | Searchable accounts, nested settings, login prompts, virtual-provider editing | Upstream commands/settings; host dialogs | Scripted choices, messages, cancellation, persisted outcomes |
| H10 | Full session tree, branch revisions, committed custom entries and durable journals | Cursor session ports; harness storage + host actions | JSONL sync/SQLite transaction failures, returned IDs, ephemeral memory, branch-change races |
| A01 | Original `google` identity, explicit catalog replacement, model IDs/mappings/reasoning/limits, zero prices, no image-generation routes | Pi `index.ts`, core model registry; `rpi-antigravity-auth` catalog | Exact catalog comparison; offline load and retired-model refusal before auth/network |
| A02 | PKCE callback URL/code, token refresh, project discovery/onboarding, packed refresh, access/project cache | Pi `index.ts`, `credential-cache.ts`, core OAuth/project | Fake OAuth/project services, cache eviction, expiry units, error redaction |
| A03 | Text/images/system/thinking/tool schemas/calls/results; signature preservation and cross-model omissions | Pi `convert.ts`; native request conversion | Golden request/history comparisons including malformed history |
| A04 | Session/request metadata, ordered HTTP/1.1 headers, chunking, gzip, TLS/proxies/timeouts | Core `agy-*`; native transport | Captured upstream wire bytes and scripted proxy/TLS peers |
| A05 | SSE partials, late usage, stop reasons, malformed frames, deadlines, redaction | Pi `stream.ts`; native streaming | Ordered event/error/usage comparisons and cancelled/stopped consumers |
| A06 | Enrollment proposal/approval/poll/rotation, roster/select/accept/decline/disconnect | Pi `vault-setup.ts`, `vault-source.ts`, common-auth Claustrum | Producer fixtures and enrolled-consumer adapter, pinned Rust Subc/Claustrum revisions |
| A07 | Backup + non-secret marker before custody; conditional restore; ambiguous/unavailable/mismatched custody refuses without local fallback | Pi `vault-setup.ts`, core Pi slot | Mutation ordering, concurrent slot replacement, corrupt mode/identity |
| A08 | Fresh credential/project each physical send; receipt/version-aware single 401 retry and failure report | Pi `vault-source.ts`, core vault source | Rotated receipt fixtures and exact send/report counts |
| A09 | Complete `/antigravity` menu and notifications, no quota recording/local account rotation | Pi `commands.ts`, common-auth menu | Scripted menu traces and exact informational messages |
| M01 | Account store schema/validation, locks, atomic private writes, redaction | `auth-store.ts`; `rpi-multiprovider` store | Existing auth-store cases plus competing processes and write failures |
| M02 | `/multilogin`, `/multilogout`, `/accounts`, `/switch-account`, `/vprovider`; auth method choice, nested login, labels/reauth/enable/weight/priority/confirmed removal | `multilogin.ts`, extensions/settings/pool-manager | Scripted UI and resulting files; upstream credential kept separate |
| M03 | Round-robin/bias/weighted/least-inflight/priority, affinity/pins/spillover/leases | `service.ts`; scheduler | Deterministic selections and release accounting |
| M04 | Account/account-model health, exact classifiers/configurable cooldowns | `service.ts`, `managed.ts`; health | Status/header/message cases and injected-clock transitions |
| M05 | Journal pins, child inheritance, stale warnings and parent/child lifetime | `session-pins.ts`, extension; host journals | Branch/reload/child termination scenarios |
| M06 | Native discovery, registration, identity-preserving wrapping, services | `register.ts`, `announcement.ts`, extension | Dynamic provider replacement, service announcements, native fixture extension |
| M07 | Virtual model templates, metadata choice, nested pools and backend failover | `virtual.ts`, settings UI | Model/config comparisons and backend-attempt traces |
| M08 | Both stream entry paths; per-attempt auth/options, delegated retries off, exact initial buffering/output boundary/same-account tolerance/250ms pause/terminal and missing-terminal behavior | `lift.ts`; wrapper | Full ordered stream/attempt trace; thinking/tools close retry window; preserve source quirks |
| M09 | Fabric retry/idle compaction via native service when available, inline failover otherwise | multiprovider extension | Compatible fixture service plus absent-service scenarios; Fabric itself excluded |
| C01 | Persistent Node 24+ worker, installed assets, private versioned IPC, IDs/generations/cancel, bounded byte/count queues and control progress | Retained adapter + native worker owner | Packaged-worker execution, oversized frames, blocked streams, concurrent host calls |
| C02 | API key, offline/cache/live catalogs, variants/thinking/fast/slow | Cursor binding/discovery/settings; retained adapter | Upstream unit/native tests and host-port differential tests |
| C03 | Local create/reuse/resume, prompt/images/settings/custom agents/HTTP controls, SDK pool ownership | Cursor turn/session modules; retained adapter | Fake SDK contract tests, concurrent owners, cold start and cancellation |
| C04 | Branch continuity, compaction/tree summary agents, store/lineage/exact-ID cleanup | Cursor session/resume/store modules; awaited session ports | Authoritative acknowledgements, async durability, late branch responses, exact cleanup IDs |
| C05 | Native/MCP/bridge tools, questions/skills and recorded activity | Cursor tool modules; host ports | Exact execution counts, failures, replay, ownership, trust |
| C06 | Cloud selection/ack/repository/env restrictions/lifecycle/explicit archive/delete | Cursor cloud modules; durable ports | Intent before mutation, identifier persistence, failed sync, crash/uncertain outcome; no automatic replay |
| C07 | Usage/billing refresh/export/interrupted recovery | Cursor usage modules; durable ports | Origin and branch identity, durable ordering, partial/crashed-worker recovery |
| C08 | All commands/flags/config precedence/trust/limitations, stable reconstructed header/signal identities | Cursor index/settings/provenance; host facade | Command/receipt/operation identity comparisons |
| C09 | Stop admission, cancel, close MCP/SDK, flush and reap; preserve partial results and recovery records | Cursor lifecycle + native owner | Reload/shutdown/crash sequences and DLL lifetime |
| I01 | Explicit source-selection import and reviewable summary; credentials/settings/pools/history/custom entries; conflict refusal/idempotency/expiry units | New import command | Source files/locks unchanged, destination conflicts, repeat import, refresh-rotation notice |
| I02 | No imported resume/cleanup/cloud authority or active custody/backups; fresh agent + continuity notice, fresh vault enrollment | New import command + Cursor/vault ports | Imported malicious/obsolete markers and identifiers cannot activate authority |
| R01 | Independent install/upgrade/uninstall/offline/disabled discovery; retained licenses, reviewed SDK redistribution | Package manifests/distribution | Fresh packaged installations; no change to existing providers |
| R02 | Linux/macOS/Windows native loading and packaged worker; old compiled/short-vtable plugins; supported Rust compiler | Host/package CI | Actual platform executions, warning-denied workspace + SDK feature checks |
| R03 | All three together, pooling over Antigravity/Cursor | Reference and candidate integration fixtures | Same scripted providers/OAuth/vault/SDK/UI with controlled clock/random/IDs; normalize approved differences only |
| R04 | Controlled local/vault/refresh and Cursor local/tools/images/cancel/resume/cloud lifecycle acceptance | Disposable controlled accounts | Bounded real usage and verified cleanup; missing credentials/resources are **not tested** |

## Reproduction

Python 3.12+, `gh`, the locked dependency managers, Node 24+ and Rust are needed.
Use a separate directory for reference code and logs:

```sh
python3 scripts/provider-parity.py fetch /absolute/path/to/provider-references
python3 scripts/provider-parity.py baseline /absolute/path/to/provider-references
node scripts/provider-parity-inventory.mjs /absolute/path/to/provider-references
```

Antigravity's pinned build source explicitly requires Bun **1.4.2**; its older
agent instructions still mention 1.3. Source wins. Its full suite inspects Git
history and running processes, so an archive alone is insufficient. On Linux,
the isolated PID namespace trials avoided interference from unrelated OpenCode/Pi
processes without stopping them, but did not qualify the suite: the user namespace
leaves ancestor owners unmapped, and Claustrum rejects them. Mounting a private
`/tmp` still left `/` unmapped. The final full run had 3,906 passes and 79 failures;
the separate Pi adapter run had 163 passes. These are recorded failures, not
accepted parity exceptions.
Cursor's tests need a temporary root without an ancestor `.git` or a group-writable
non-sticky ancestor. The passing run used a private directory directly under the
user's home, Node 24.22.0, `TERM=xterm-256color`, and
`CURSOR_API_KEY=offline-fixture`. The key is only a placeholder for the mocked
native binding test. `TERM=dumb` overrides its SDK color fixture's `FORCE_COLOR`.
Keep these environment corrections separate from source edits.

The integrated reference uses packed, built upstream packages in a separate npm
installation with all three Pi host packages fixed at 1.3.0. Its offline harness
is `scripts/provider-reference-integration.mjs INSTALL_DIRECTORY`. A passing
load/startup/shutdown harness does not qualify streaming, tools or live behavior.
The executed integration installation's manifest and lock are retained in
`reference-install/`. Pack the four upstream packages into the reference root
(Antigravity core and Pi adapter, Cursor, multiprovider), copy those two files
to `REFERENCE_ROOT/integrated/`, then run `npm ci --ignore-scripts` there. The
manifest's relative tarball paths refer only to those locally built inputs.

The declared Rust 1.78 compiler was checked explicitly: the standalone SDK with
default features disabled builds, but `rpi-ai --features providers --locked`
cannot parse the existing `idna_adapter 1.2.2` dependency's Edition 2024 manifest.
The host lockfile and supported-version declaration were not changed. Resolving
that baseline compatibility issue is a separate release prerequisite.

Cursor SDK's installed `LICENSE.md` says use is subject to Cursor's Terms of
Service. Redistribution review is outstanding; do not vendor its dependencies.
Antigravity's account-suspension warning and Cursor's native tool/cloud execution
behavior remain operational risks. This work has performed no live inference or
remote account/agent mutations.
