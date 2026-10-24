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
