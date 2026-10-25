# L2-A lifecycle implementation evidence

This lane does not close H05/H06/H10: loaded-host cross-lane qualification is separate.

## A1 — durable context edits

Typed `Entry::ContextEdit` is a Rust source-compatibility change (exhaustive
matches); no C ABI or EventTag changes. `SessionStorage::append_context_edit`
default-refuses; memory, JSONL, and SQLite opt in. Validation is under storage
serialization and also during replay, including parent-chain membership.
Missing replacement is rejected; explicit null is an omission. Existing
`retainedTail` storage is unchanged.

Reference: `/home/user/Projects/provider-references/multiprovider/node_modules/@earendil-works/pi-coding-agent/dist/core/session-manager.js:962-994`
(original TS in the adjacent source map: `session-manager.ts:1360-1380`).

Test: `lifecycle_context_edit::context_edit_backends_validate_and_reopen` passes
on Rust 1.95, memory/SQLite/JSONL reopen. JSONL uses the in-memory filesystem;
SQLite uses an isolated temporary database. Mutation A1 disabled the off-branch
check: killed (exit 101, `unwrap_err()` received a committed edit), reverted.
Typed user/assistant/tool messages and the generic `custom` message role are
editable. Arbitrary custom-entry projectors are not editable storage targets;
this remains an explicit residual gap, not full Pi parity.

## A2 — canonical projection

Canonical typed context now replays last-wins edits once and exposes source IDs
through `project_context_messages`. Native finalized views already call this
builder. Compaction summary inputs and retained tails use edited projected
messages rather than resurrecting raw source content. Fork inputs remain
branch-local; retained-tail ownership/serialization is unchanged.

`lifecycle_context_edit` (2 tests) passes. Mutation A2 skipped edit replay:
killed by the projection message-count assertion (exit 101), reverted.
Reference: original `session-manager.js:238-270` (same absolute directory as A1).
Unsupported typed replacement blocks are refused before acknowledgment rather
than silently falling back to old content. This is a host typed-model constraint,
not qualification of arbitrary custom-entry projectors or loaded Cursor.

## A3 — compaction continuation and brackets

`will_retry` is explicit, not inferred from overflow. The host progress guard
now includes before-hooks, extension overrides, cancellation, and persistence.
Success hooks precede end; failure/cancel end precedes the failure hook. Drop
also ends progress if the future is cancelled.

Rust 1.95 `lifecycle_compaction` tests default/override/veto/signal and a completed
over-window response with `willRetry:false`. Mutation A3 inferred retry from
reason again: killed by the false assertion, reverted. Existing `life_compaction`
tests also pass. This is a host-hook fixture, not a loaded native DLL.

Reference: `/home/user/Projects/provider-references/multiprovider/node_modules/@earendil-works/pi-coding-agent/dist/core/agent-session.js:2511-2610`
(embedded TS `agent-session.ts:3143-3272`). Public Pi-shaped start/end events
are not added: `events.rs` is outside lane ownership; the existing host progress
channel is explicitly not the Pi event payload. Lane integration needs that
public adapter if exact public event payload parity is required.

## A4 — persisted omission and one continuation

Runtime orchestration now gives ordinary retry scheduling precedence, settles the
failed generation, resolves source IDs before editing, writes null edits through
the opt-in backend API, prepares from the edited path, compacts and invokes the
agent loop once more. The recovery budget is separate from ordinary retries;
invocation settlement indices advance independently. A second overflow/length
returns the exact terminal error and emits host end before the failure hook.
No second summarizer is entered on exhaustion. Post-run compaction does not
repeat a recovery already evaluated in the invocation loop.

References (original source-map TS): `agent-session.ts:1241-1256`,
`agent-session.ts:1943`, `agent-session.ts:2977-3272`; executable
`/home/user/Projects/provider-references/multiprovider/node_modules/@earendil-works/pi-coding-agent/dist/core/agent-session.js:839-855`,
`:1501-1537`, `:2362-2610` (exact errors at 2418-2419, 2594).

Tests: `lifecycle_overflow` (3/3), plus restored A1/A3 tests (2/2), pass.
The Faux fixtures explicitly give continuation errors timestamps after the
compaction clock: default Faux timestamp 1 correctly triggers Pi's stale-error
suppression and is not a valid second-overflow fixture. Mutation A4 reset the
budget supplied to the decision before continuation: both exact-error tests
failed (exit 101); reverted and all tests rerun. The mutation did not necessarily
make a third call: after omission, preparation can correctly be empty.

Remaining cross-lane work: `HarnessEvent::CompactionProgress` carries only
active/manual/lane, not Pi end payload/error; it cannot by itself qualify public
Pi-shaped event parity. There is no host entry-appended event in this bus. Lane B
must bridge omission entries and compaction start/end into existing native JSON
transport without changing C ABI. These tests use real host/provider hooks but
not a loaded native DLL. Crash/restart budgeting and loaded queue/tool-result
recovery matrix remain unqualified; the in-process one-use budget is covered.

## A5 — pre-prompt boundary and durable admission

The run operation intent is committed before pre-prompt evaluation. New prompt
transcript entries are placed only after prior-history compaction succeeds (or
is skipped). Accepted queued prompts keep their existing enqueue records.
Aborting compaction does not erase `operation_started.originalPrompt`; this
admission is distinct from transcript placement and does not promise automatic
resubmission. The host-only post-tool compaction safety valve is unchanged.

References: embedded TS `agent-session.ts:1890-1943`; executable
`/home/user/Projects/provider-references/multiprovider/node_modules/@earendil-works/pi-coding-agent/dist/core/agent-session.js:1449-1537`.

Tests: `lifecycle_pre_prompt` passes (2/2). Before-hook branch/preparation exclude
a marker in the newly accepted prompt; SQLite reopen verifies the operation
intent survives compaction abort, with no provider call or transcript placement.
A separate transient-429 → overflow → recovery sequence verifies a durable
ordinary retry record, one compaction, and three generation calls. These are
component tests, not a process-kill/loaded-plugin acceptance test.
Mutation A5 moved transcript placement back before preparation: killed (exit
101, marker found in preparation/branch); reverted and tests rerun. No network,
real user configuration, or retained-tail migration was involved.
The wider harness suite caught an admission regression on a lane with no
provider: its prompt must still be placed before returning `no_provider`.
That check now runs after the transcript boundary; all 210 harness unit tests
and both pre-prompt tests pass with the correction.

## Follow-up — replacement validation hardening

Storage now refuses unsupported content blocks before acknowledging an edit
(`Context edit content is not supported by the target message`, a host-only
constraint of the closed typed message model). Generic `custom` messages accept
string replacements with host-shell normalization; their durable edit still
contains the original string, unlike assistant/tool edits. Canonical and
compaction projection share that normalization. Other special custom roles and
arbitrary custom-entry projectors remain non-editable, as they can render from
opaque data rather than content.

The existing backend fixture now covers assistant normalization, custom string
replacement, and invalid-block refusal on memory/SQLite/JSONL. All eight new
lifecycle tests pass. Mutation disabled typed-content validation: killed by the
invalid-block refusal assertion (exit 101), reverted. This is a compatibility
restriction relative to TypeScript's open content model, explicitly not a new
C ABI contract.

## Final offline gates and integration handoff

Machine-readable commands, log hashes, mutation logs and TS references:
[l2a-lifecycle-results.json](l2a-lifecycle-results.json).

| Gate | Result |
| --- | --- |
| Rust 1.78 workspace/all-targets, `-Dwarnings` | BLOCKED: E0004, CLI exhaustive Entry match |
| Rust 1.78 harness/all-targets, `-Dwarnings` | PASS |
| Rust 1.78 SDK minimal / provider-only, `-Dwarnings` | PASS / PASS |
| Rust 1.95 full workspace tests | BLOCKED: same E0004 before tests |
| Rust 1.95 workspace tests excluding CLI | PASS: 1643 passed, 0 failed, 8 ignored, including doc-tests |
| Eight new lifecycle tests | PASS; no new ignores |
| `cargo fmt --all --check` / `git diff --check` | PASS / PASS |
| All seven `docs/provider-parity/check-*.mjs` | PASS; no `FAIL` lines |

All cargo invocations were foreground, bounded by `timeout 270`; cold non-CLI
builds timed out and were rerun to completion. The first broader run caught the
no-provider transcript regression documented under A5; it was fixed before the
passing final run. The full workspace baseline (2432/0/26) is **not** requalified.

**Lane B / shared events:** the new Rust Entry variant needs an exhaustive arm at
`crates/rpi-cli/src/interactive_tui/sessions.rs:365`; current full gates fail
there, so this branch is not independently merge-green. No B-owned file was
edited. Display/replay must not treat edits as ordinary messages. A shared host
event/native JSON adapter is needed for Pi-shaped `entry_appended`, compaction
start/end result/error/aborted/willRetry (including end-only exhaustion); the
current progress channel does not carry them. Preserve existing C ABI slots.

**Lane C:** consume authoritative edited context, not raw message entries; do
loaded Cursor acceptance for omission → compact → restore/continue, late writes,
queues/tools and cancellation. Existing retained-tail data stays host-owned.

**Lane D:** no required new catalog/runtime interface from this implementation.
Generation-bound hook providers still need the B/D integration matrix. No edits
to `model_registry.rs` or `provider_runtime.rs` were made.

H05/H06/H10 remain partial. Neither these component tests nor historical evidence
files qualify loaded native/plugin, crash, replacement or live provider behavior.
