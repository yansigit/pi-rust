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
Custom projected entries are not editable yet; only typed user/assistant/tool
messages are supported. This is an explicit residual gap, not full Pi parity.

## A2 — canonical projection

Canonical typed context now replays last-wins edits once and exposes source IDs
through `project_context_messages`. Native finalized views already call this
builder. Compaction summary inputs and retained tails use edited projected
messages rather than resurrecting raw source content. Fork inputs remain
branch-local; retained-tail ownership/serialization is unchanged.

`lifecycle_context_edit` (2 tests) passes. Mutation A2 skipped edit replay:
killed by the projection message-count assertion (exit 101), reverted.
Reference: original `session-manager.js:238-270` (same absolute directory as A1).
Unsupported typed replacement blocks are a remaining limitation; this is not
qualification of arbitrary custom-message projectors or loaded Cursor.

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
