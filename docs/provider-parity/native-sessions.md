# Native session persistence

Status: storage and native session ports implemented; provider file journals pending.
This is partial H10, not package parity.

`SessionStorage::snapshot` returns the complete entry tree, labels, name, session
metadata and committed mutation revision. Each lane also carries its last explicit
navigation revision. Appending entries preserves that revision; navigating away
and back changes it, even when the final leaf is the same.

`commit_entry` returns an actual entry, revision, whether it advanced the lane, and
`durable` or `ephemeral`. `AppendTarget::Current` refuses a stale branch revision.
`AppendTarget::Origin` preserves a late journal entry under its captured anchor
without advancing a newly selected branch. Unknown anchors, future revisions and
reused IDs fail before writing. These methods leave legacy append signatures and
file formats unchanged. Unsupported storage implementations return explicit errors.

JSONL acknowledges after synchronizing the file and, on Unix, its ancestor
directories. Failed or cancelled appends disable that writer: later writes,
snapshots and durability acknowledgements fail until reopening recovers the tail.
A complete uncertain frame survives recovery; a torn last frame follows the
existing recovery rules. Recovery never retries a remote operation. A standalone
synchronization failure may be retried, but is never reported as durable.

SQLite serializes read-modify-write under `BEGIN IMMEDIATE`, with WAL and
`synchronous=FULL`. Each mutation reads the current committed state; separate
connections cannot overwrite each other's updates. Failed transactions discard
uncommitted state. Memory and SQLite temporary/in-memory databases explicitly
report ephemeral persistence.

CLI reopening reuses a live JSONL writer by canonical path. Journal owners can
retain the original storage through session switches without introducing a
second in-process writer. Forks copy that live writer's consistent state. An
uncertain writer is permanently disabled before reopening replaces it. Existing
process-lifetime advisory locks still exclude competing rpi processes.

Tests cover all three backends, branch changes and reopening, failed synchronization,
partial/cancelled writes, SQLite rollback and concurrent connections, live-writer
reuse and file/symlink validation. Linux verification does not establish macOS or
Windows behavior. A loaded SDK fixture verifies actual native committed writes and
reopening. Worker acknowledgements and provider file-journal recovery remain pending.

## Private native port

Use SDK `provider::native::Client`, version 1, port `sessions`. The existing trailing
native-request/context slots carry these operations; no C ABI structure changed.

| Action | Arguments | Result |
| --- | --- | --- |
| `capture` | optional `lane` (default `main`) | Opaque `origin` handle and complete `snapshot` |
| `snapshot` | `origin` | Complete current tree and revisions of that origin's storage |
| `view` | `origin` | Atomic snapshot, origin branch leaf/revision and host-derived context projection; requires `sessions.view` |
| `location` | `origin` | Host-owned backing `path`, or null; requires `sessions.location` |
| `append` | `origin`, `customType`, optional `data` | Committed receipt; refuses a changed branch |
| `journalAppend` | `origin`, `customType`, optional `data` | Committed receipt; preserves the original branch after navigation |
| `sync` | `origin` | Explicit `durability` acknowledgement |
| `release` | `origin` | `null`; future requests with this handle fail |

Capture freezes the storage handle, not the mutable session facade. The current
request identity must match the captured session. Host-issued entry IDs appear only
in committed receipts. Missing data and explicit JSON null remain distinct.
Origins are scoped to the owning plugin, generation and child execution; expired
children never fall back to the parent. Limits are 1,024 live origins per host,
128 per owner and 16 MiB of custom entry data. Unknown fields are rejected.

`location` resolves only the captured storage. It cannot accept a caller-selected
path, switch to the newly active session or cross an owner/generation boundary.
JSONL returns its absolute addressed file; SQLite returns its database path.
Memory and temporary SQLite storage return null. The path is an identity/read
descriptor, not permission to bypass the host's writes, and does not acknowledge
durability. Consumers must still await `append` and `sync`. The Rust storage
trait adds a default optional location method; C ABI layouts and versions stay
unchanged. Other storage implementations may return no location.

`view` follows the captured lane while its navigation revision is unchanged.
After navigation it follows the origin's anchor and subsequent journal appends,
without selecting the newly active branch. Projection uses the captured execution's
real context transforms and custom-entry projectors, each exactly once per view.
Each contribution includes its source entry and messages. Compactions retain their
actual entry IDs and own their retained-tail messages; clients need not infer
old entry IDs from matching content. Projectors run after releasing the origin's
write mutex. The worker's synchronous getters can therefore read one acknowledged
view while retaining a stable manager identity across requests.

Execution-context inspection and session capture select the same child harness.
An expired child fails instead of returning the parent's session ID.
[View verification](host-session-view-results.json) includes the loaded SDK fixture,
branch/projector regressions and the warning-denied Rust 1.78 workspace check.

A cancelled waiter receives an error, while an already-started commit completes.
Inspect the journal before retrying an uncertain operation. Appends serialize per
origin so late journal entries form their own parent chain. Shutdown callbacks may
flush before the bridge stops admission; normal shutdown and reload then drain
only that bridge's owners, retaining the libraries through completion. Unloading
a child or older generation does not wait on or retire unrelated owners.

`journalAppend` writes custom session entries. It does not yet replace Cursor's
separate cloud/usage/cleanup file journals: those require their original framing,
validation, intent ordering and recovery decisions in the retained adapter.

