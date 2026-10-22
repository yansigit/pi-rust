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


## Retained persistence authority

The negotiated `sessions.persistenceLease` capability adds operation `2` to the
existing `native_context` slot; `Client::session_persistence()` wraps it. Acquire
it while the originating invocation is live. It preserves the captured owner,
generation and child execution scope, but permits only session operations naming
an existing origin. New capture, auth, tools, dialogs and other native ports are
rejected. Rescoping retains this restriction. No C ABI layout changes are needed.

This separates a worker's late persistence from the requesting caller's lifetime.
Explicit request cancellation still applies, and retirement/reload revokes the
lease. A stopped child's retained writer can finish an origin-bound journal
entry. Ordinary append still checks the captured branch revision; a journal
append after navigation cannot advance the newly selected branch. The writer
must release its origins and lease after completion; handles remain bounded.

The host derives persistence authority from the opaque lease, never from request
JSON. Ordinary session calls retain their cancellation behavior. Child origins
hold only weak execution references; retaining a writer does not grant access to
another child or parent origin. The bridge and storage regressions are recorded
in `host-session-persistence-results.json`. Cursor's actual native router and
worker shutdown integration remain separate acceptance work.

## Conditional navigation

`SessionStorage::move_lane_at_revision` compares and moves a branch under the
backend's write lock/transaction, returning the committed branch revision.
Ordinary entry appends do not revoke the captured revision; any explicit move
does, even a move back to the same leaf. JSONL synchronizes before returning,
SQLite commits its FULL-synchronous transaction, and memory remains ephemeral.
A failed or cancelled JSONL write poisons that handle until recovery. Invalid
targets and stale revisions leave storage unchanged. The trait default reports
unsupported storage, so existing implementors continue compiling without an
unsafe check-then-write fallback. No C ABI or workspace version changes.

`session_origins` covers all three backends, competing moves, stale summary
appends and reopen; `m5c_jsonl_atomic_publish` injects synchronization, partial
append and cancellation faults. The tree operation's hook and summary wiring
is a separate change; this primitive alone does not establish tree parity.

## Branch summary generation

`generate_branch_summary` now matches Pi 1.3.0's `generateBranchSummary`
request and outcome rules: `maxTokens` is `min(4096, model.maxTokens)` (4096
when unbounded), no reasoning option is sent, empty custom instructions fall
back to the default prompt, and `length` stops or tool calls fail instead of
persisting an incomplete summary. Empty error messages report `Unknown error`.
`BranchSummaryEntry.fromHook` round-trips through JSONL, SQLite provisioning
and memory; extension-supplied summaries do not seed file-tracking details for
later summaries. Malformed `fromHook` values are rejected.

`check-branch-summary-reference.mjs` regenerates
`branch-summary-reference.json` from the pinned Pi 1.3.0 install (16 traces);
`generated_summaries_match_pi_reference_requests_and_failures` compares every
request and result. Numbers compare by value, since JavaScript records `0` for
Rust's `0.0`. Tree navigation hook and summary dispatch remain pending.

## Tree navigation

`AgentLane::navigate_tree_with(target, NavigateTreeOptions)` implements Pi
1.3.0 `AgentSession.navigateTree`; the four-argument `navigate_tree` remains and
delegates with `replaceInstructions` unset. Navigation is an admitted operation,
so it is refused while a run or compaction is active, and `abort` cancels an
in-progress summary. In order:

- A target equal to the current leaf completes without events. A missing entry
  fails with Pi's `Entry <id> not found` before any hook.
- `session_before_tree` receives Pi's preparation (`targetId`, `oldLeafId`,
  `commonAncestorId`, the entries to summarize, `userWantsSummary`, and any
  `customInstructions`/`replaceInstructions`/`label`) with operation identity
  `purpose: "tree"` and the navigation's cancellation signal. A returned
  `cancel` declines; a summary is used only when summarizing; defined
  instruction and label overrides replace the requested values; a hook error
  fails the navigation.
- Otherwise a requested summary uses `generate_branch_summary` with the session
  model, retry settings and `branchSummary.reserveTokens`. An aborted summary
  reports `Aborted`; other failures report `Failed`.
- A user message or `custom` message target moves the branch to its parent and
  returns its text as `editor_text`; any other target becomes the leaf.
- The move is `move_lane_at_revision` against the revision captured at
  admission. A summary is committed at the new position (`fromId` is the old
  leaf or `root`, `fromHook` records an extension summary), and the label goes
  to the summary entry, else to the target. The active tool set is restored from
  the new branch's latest `active_tools` entry, as Pi restores tools from the
  transcript.
- `session_tree` carries `newLeafId`, `oldLeafId` and, with a summary,
  `summaryEntry` and `fromExtension`.

Pi emits nothing after a cancelled or failed `session_before_tree`. Native
extensions that track the operation (Cursor ends its `"tree"` operation on
`session_tree`) instead receive the rpi-only `session_tree_failed`
(`EventTag::SessionTreeFailed = 38`, appended; `EVENT_TAG_COUNT` is 39) with
`targetId`, `oldLeafId`, `cancelled`, `aborted` and an optional `error`. It is
sent only when the before hook ran. The extension action reports Pi's
`cancelled` and rejects when Pi's `navigateTree` would throw.

`crates/rpi-harness/tests/tree_reference.rs` replays all ten Pi traces in
`tree-reference.json` (events, mutations, final leaf, result or error, shared
operation identity and the terminal event) and separately covers a generated
default summary and an abort mid-summary.

### Interactive tree selector prompt

`crates/rpi-cli/src/interactive_tui/tree_ui.rs` implements the Pi 1.3.0
interactive caller (`interactive-mode.js:4660-4720`). `/tree` opens the session
entry selector; choosing the current leaf short-circuits with "Already at this
point". Otherwise the caller asks "Summarize branch?" with Pi's three answers
("No summary", "Summarize", "Summarize with custom prompt"), honours
`branchSummary.skipPrompt` by navigating without a summary, and opens the
multi-line "Custom summarization instructions" editor for the third answer.
Escape walks back one level: the editor returns to the summary selector, and the
summary selector re-opens the tree selector with the same entry highlighted.

The answer becomes `NavigateTreeOptions` (`summarize`, `customInstructions`),
so the harness emits `session_before_tree` and `session_tree` for the dialog
path exactly as for a direct call. An aborted summary shows "Branch
summarization cancelled" and re-opens the tree selector; a hook-declined
navigation shows "Navigation cancelled"; a failed navigation shows the harness
error after the terminal `session_tree_failed` event. A user or custom-message
target fills the editor from `editorText` only when the draft is empty, and the
transcript is reloaded from the new branch. Repeated navigation is guarded by
the same compaction/navigation check Pi performs after its dialogs.

`crates/rpi-cli/src/interactive_tui/tree_ui/tests.rs` covers the selector
answers, the custom editor submit/cancel paths, Escape retries, the skip
preference, editor placement and the outcome statuses (11 tests).
`crates/rpi-cli/tests/tree_ui.rs` drives the runtime action the dialogs call
with the same option combinations and asserts the hook payloads plus the
terminal event for cancelled and failed trees (6 tests). The ten Pi traces
remain covered by `tree_reference.rs`.
