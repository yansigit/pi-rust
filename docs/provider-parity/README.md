# Provider compatibility work

Status: **not parity complete; Antigravity and Cursor are under implementation; Multiprovider remains pending**.

The host baseline is `90093c9e2d71ee9ce8228c1c27dc3262931b4a60`.
Package work belongs in the separate [rpi-package fork](https://github.com/yansigit/rpi-package),
on `codex/provider-parity`, based on `feature-1014` at
`88d305803dd58434c9a36059d28b0a2328d6d927`. Existing provider packages stay unchanged.
No workspace version bump is authorized by this work.

[references.json](references.json) pins the three upstream implementations,
Pi 1.3.0, Cursor SDK 1.0.37, and unchanged baseline commands. Source behavior is
authoritative. Baseline failures are evidence about the reference, never a
passing candidate test or an automatically approved exception.

The user approved an additional exception on 2026-10-21: Antigravity body reads
return timeout/cancellation errors and release resources, instead of retaining
the pinned Node transport's pending read. The original fixture and candidate
regressions remain recorded in the package ledger. Other transport checks remain
required; this approval does not declare A05 complete.

The user also approved Cursor emission-time partial snapshots on 2026-10-21.
Pi queues references to mutable partial messages, so earlier events can contain
later content. Native IPC captures each event at emission through the bounded
credit gate. Event order, deltas, signatures, usage and final results are not
exempted. The retained Pi entry path keeps its original queue behavior.

The [Cursor worker foundation](cursor-worker.md) retains the complete pinned
adapter and implements private, bounded Rust/Node transport with cancellation,
concurrent request ownership and shutdown draining. Its actual native provider,
host ports and packaged execution remain in progress.

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

[Configured native authentication](configured-auth.md) resolves private provider
and model headers per physical send, preserves stored/explicit key precedence,
and excludes ambient configuration from replacement attempts. Catalog inspection
and configured-key readiness checks do not run credential commands.

Response status and lossless header values have a private, awaited observation port
through built-in transports and the native SDK. Native receipt queues are drained
while the provider is opening its stream. Observer events omit keys and header
values. [Native composition](native-composition.md) adds session-owned wrappers,
opaque delegated calls and provider-resolved pooled account receipts. The loaded
SDK fixture verifies a pooled HTTP request, canonical endpoint restoration and
credential isolation. Stream snapshots retain timestamps, usage and signatures.
[Native runtime ports](native-runtime.md) now provide scoped catalog inspection,
publication, refresh, registration, removal and private resumable authentication.
Print/JSON session lifecycle dispatch, explicit legacy OAuth/key management and
both Pi stream entries are included. [Compatibility regressions](host-compatibility-results.json)
cover the adapters and nested wrappers. H01–H05 remain pending until interactive
TUI callers and complete differential tests are integrated. The
[SDK distribution review](cursor-sdk-distribution.md) keeps the pinned Cursor SDK
external to release archives.

[Native events](native-events.md) add awaited ordered callbacks, explicit run/turn/
request/session identities, authoritative message edits, tool-result chaining, and
compaction success/failure notifications. This is partial H06 infrastructure; tree
and UI dispatch and the Cursor private request-receipt boundary remain pending.
The native catalog fixture also verifies empty-catalog auth discovery and edits
through an actually loaded library. [Event evidence](host-events-results.json) records
the full workspace and targeted regression runs.

[Native services](native-services.md) add versioned private calls, bounded ordered
subscriptions, child ownership and leased invocation context across foreign workers.
[Service evidence](host-services-results.json) covers loaded libraries, nested
wrappers, old-library compatibility and the declared compiler. The multiprovider
account/Fabric implementations remain pending.

[Native tools](native-tools.md) add scoped inspection and direct invocation through
the existing tool pipeline, plus inert recorded activity that survives stream and
history serialization. Recovery cannot execute a recorded call, including when a
same-named tool declares safe reexecution. [Tool evidence](host-tools-results.json)
records host checks; retained Cursor/MCP and live tool acceptance remain pending.

[Native dialogs](native-dialogs.md) add private awaited replies, searchable stable
account IDs, nested settings actions and validated login inputs to both terminal
modes. Owner shutdown and cancellation release pending prompts. Package-specific
command traces and persisted outcomes remain pending. [Dialog evidence](host-dialogs-results.json)
includes 276 executable Pi search comparisons, affected-crate regressions, a
loaded-library dialog fixture and Rust 1.78 checks.

[Session storage](native-sessions.md) now exposes committed tree snapshots, branch
revisions and durable appends with origin-bound late journal writes. JSONL rejects
further writes after an uncertain append; SQLite uses durable transactions and
fresh state under its writer lock. CLI reopening shares live writers across
session switches. [Storage evidence](host-storage-results.json) covers Linux
backends, native library acknowledgements, owner-scoped shutdown and failure
recovery. Cursor now has a Rust file-journal port and awaited adapter writes, with
native cleanup using these committed session receipts. Full runtime dispatch,
inherited-origin journal access and background lifetime integration remain pending,
so H10 is not complete. The package's `tests/journal-evidence.json` records actual
Rust/Node IPC, four-process locking, 16 pinned writer byte comparisons, failure
injection and retained-suite checks.

The Antigravity reference revealed two generic transcript requirements:
`AssistantMessage.duration_ms` retains Pi 1.3.0's provider-measured duration,
including through cancellation, and JSON decoding preserves exact floating-point
values across native IPC and persistence. Untimed legacy messages remain untimed.
The optional duration field is a Rust source change for struct literals and must
ship through the maintainer's coordinated release; no C ABI field or version is
changed. The package ledger records request/stream fixture progress separately
from host acceptance and live inference. [Regression evidence](host-stream-metadata-results.json)
records the warning-denied workspace suite, Rust 1.78 and SDK feature checks.

[baseline-results.json](baseline-results.json) records upstream results, lock
hashes and evidence hashes. All three unchanged suites pass in
[CI run 39569889592](https://github.com/yansigit/pi-rust/actions/runs/39569889592):
Antigravity 4,009 passed/one upstream skip; multiprovider 107 passed;
Cursor 1,634 unit tests and 53 native tests passed/two upstream skips.
One authorized live Gemini 3.8 Flash request passed on Linux, including refresh
and durable persistence (588 reported tokens, exact `OK`, unchanged source Pi
files); see [live evidence](live-acceptance.json). Combined inference, vault live
acceptance and platform execution remain pending.
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
the upstream runner owns their expanded case counts. [File mappings](file-mapping.json)
assign every pinned file and its declarations to behavioral rows, explicit port
files and acceptance files. These paths are **planned**, not claims of existing
implementations. The generator rejects missing mappings, changed source hashes and
stale mappings. Every acceptance row remains pending until qualified by evidence.

[Antigravity reachability](antigravity-reachability.json) records Pi's runtime
imports, named barrel exports and lazy common-auth loader edges. Reached modules
are conservative: their unused branches are not automatically declared equivalent.
Pi uses sections-mode menus and its own converter; the unrelated OpenCode pool,
migration, quota polling and conversion modules are explicitly outside this scope.
Reference build/test-process fixtures retain their unchanged baseline, separately
from candidate acceptance. Mixed tests (such as Claustrum and pool quota counters)
remain required until their Pi-reachable cases have differential evidence.
Runtime-generated test names and imported test declarations still require review.

The behavioral ledger below supplements that file inventory. Each row requires
passing differential evidence before completion. All rows are currently pending.
Multiprovider's standalone scheduler and pin parsing/replay now have
[offline differential evidence](../../../rpi-package/packages/rpi-multiprovider/tests/scheduler-evidence.json):
selection, lease accounting, account/model health, registration changes during
awaited inventory, and inherited/recorded pin decisions. This qualifies the
implemented portions of M03–M05; native loading, persistence and lifecycle wiring
remain pending. File mappings now assign `service.ts` to M03/M04 and
`session-pins.ts` to M05.
The agreed exceptions are native rpi presentation, native-only extension
contracts, isolated import storage, fresh runtime ownership, and the approved
Antigravity body-read timeout/cancellation and Cursor partial snapshots above; original Pi keeps
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
node scripts/provider-parity-reachability.mjs /absolute/path/to/provider-references
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

The declared Rust 1.78 compiler now passes the warning-denied workspace/all-targets
check; [compiler compatibility](compiler-compatibility.json) records the targeted
lockfile updates and equivalent older standard-library APIs. The supported Rust
version and workspace versions are unchanged. macOS/Windows execution is pending.

The [Cursor SDK distribution review](cursor-sdk-distribution.md) requires the
pinned SDK to be installed separately. Release archives must not vendor its
proprietary dependencies or platform binaries.
Antigravity's account-suspension warning and Cursor's native tool/cloud execution
behavior remain operational risks. The bounded Antigravity local test above used
one isolated credential copy; live vault and Cursor acceptance remain pending.

Clean-runner reference baselines are also run by
[`provider-reference-baseline.yml`](../../.github/workflows/provider-reference-baseline.yml).
Antigravity's product build must precede its full test suite. Its pinned build
validator checks absolute paths against forbidden module directory names, so the
reference checkout is named `agy-reference` rather than `antigravity`. The fixture
cache is explicit (`COMMON_AUTH_BUN_CACHE`) and Docker is required by its unchanged
archive-context test. These are environment prerequisites, not source changes.
