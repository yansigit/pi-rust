# Host session lifecycle and compaction wiring (H05/H06/H10, partial)

Status: Pi 1.3.0's automatic-compaction decisions and session replacement
lifecycle are now wired into the host. H05 refresh phases, Pi's full runtime
rebuild on replacement, and `context_edit`-based overflow retry remain open and
are listed at the end.

This closes the gaps recorded in [host-compaction-results.json](host-compaction-results.json)
("complete compaction preparation" and "automatic-trigger parity") and in the
[ledger](../../../docs/provider-parity/README.md) ("fresh runtime ownership
across session replacement", "inherited-origin journal access") only partially.

## Compaction wiring

The pure port from H-PURE (`compaction::pure`, fixture-verified against the
unchanged upstream modules) used to be host-independent. `compaction::wiring`
is the bridge over the host's typed `Entry`/`AgentMessage` model:

- `pi_path_values` serializes a branch path to Pi-shaped entries. A host
  compaction entry keeps its retained tail *inside* the entry while Pi 1.3.0
  records `firstKeptEntryId` pointing into the path, so the conversion splices
  the retained tail back in as virtual message entries, rewrites the parent
  chain, and records the synthetic kept id. The pure projection then replays
  exactly what `build_session_context` replays (test:
  `retained_tail_is_spliced_so_the_projection_matches_the_host_replay`).
- `projected_preparation` selects the cut with `findProjectedCutPoint` (Pi
  1.3.0's projected algorithm) and converts the selected entries with the
  host's own message constructors. Custom-entry projectors, the summary text
  envelopes and `retainedTail` stay host-owned; file operations come from the
  pure port, so nested codemode calls and a previous compaction's details are
  included. On a session without compactions it produces the same messages,
  split-turn flag, retained tail and file operations as the host
  `prepare_compaction` (test:
  `projected_preparation_matches_the_host_cut_without_compaction`).
- `auto_compaction_decision` runs `_checkCompaction`'s ordered table and maps it
  to `Run { reason, will_retry }` / `Exhausted` / `None`. It therefore gains
  the checks the host did not have: the context-overflow classifier (including
  the provider error corpus), recoverable `length` stops, the stale
  pre-compaction assistant check, abort skipping, and the one-attempt overflow
  budget.

Call sites in `AgentHarness::run_core`:

1. **Before a run** — the last assistant message is checked with
   `skip_aborted_check: false`, matching Pi's `_prepareRun`. A previous turn's
   aborted or overflowing state is compacted before the new prompt is
   persisted.
2. **After tool results** (host-specific safety valve, Pi checks after the run)
   — the host estimate still gates, because the in-flight turn is not persisted
   yet; the cut itself is the projected Pi algorithm over a self-consistent
   virtual path whose parent chain is now real.
3. **After a completed run** — the finished turn is persisted first, then
   `_checkCompaction(skip_aborted_check: true)` runs, matching
   `_handlePostAgentRun`. `Run { will_retry: false }` (threshold, or a completed
   response over the window) compacts and the compaction entry becomes the
   reported leaf. A failure is logged and does not fail the run, like Pi.
4. **Manual compaction** uses the projected preparation and now declines with
   "Nothing to compact (session too small)" *before* the hook when Pi's
   `prepareCompaction` returns undefined. `compaction_hooks_acknowledge_commits…`
   was given a session large enough to summarize so it keeps covering every
   hook mode; its assertions are unchanged.

Reasons reach the lifecycle events unchanged (`manual`, `threshold`,
`overflow`), including `willRetry`.

Not implemented: Pi's case-1 overflow recovery that omits the failed attempt
(`appendContextEdit(targetId, null)`) and retries the turn once. The host
session model has no `context_edit` entry, so the omission cannot be
represented; `Exhausted` is classified but the retry path is not driven.
Related: the host persists `retainedTail` on the compaction entry instead of
`firstKeptEntryId`, so the entry representation still differs from Pi's file
format even though the in-memory decisions match.

## Session replacement lifecycle

`rpi_cli::session::replace_active_session` implements Pi
`agent-session-runtime.ts`'s order for `new`/`fork`/`switchSession`:

1. abort and settle the active response (the outgoing session keeps the aborted
   turn's tool results);
2. `session_before_switch { reason, targetSessionFile }` for new/resume/import,
   or `session_before_fork { reason, entryId, position, targetSessionFile }` for
   a fork — a handler abort cancels the replacement and the old session stays
   active;
3. `session_shutdown { reason, targetSessionFile }` (drains auth refresh and
   native writes exactly like `/reload`'s shutdown);
4. swap the durable backing through `set_active_session`;
5. `session_start { reason, previousSessionFile }`.

Callers: the TUI `/session` switch (`resume`), `/fork` (`fork`, with the current
leaf as `entryId`), and the plugin session actions `newSession` (`new`), `fork`
(`fork`) and `switchSession` (`resume`). The plugin path gets the same live
extension cells `/reload` swaps, attached once in `build_prepared`.

Verified by `crates/rpi-cli/tests/life_session.rs` (in-process registry):
dispatch order, cancellation keeping the old session, fork using the before-fork
hook, and the plugin tool channel naming the new session.

"Fresh runtime ownership" is only partial. `AgentHarness::set_session` already
rebuilds the lane queues, drops pending resume/context state and retires child
executions; the tool context and `RPI_SESSION_ID` are republished. Pi additionally
tears down the whole `AgentSession` (extension runner, services, captured `ctx`)
and builds a new one. rpi deliberately keeps the harness and the loaded plugins
across a switch, so a captured plugin `ctx` is not invalidated. RPC mode has no
`new_session`/`switch_session`/`fork` commands to wire.

## History-authorized origin resolution

`native_sessions` keeps an origin's captured storage for reads, location,
`append` and `sync` — the documented rule is that an origin never silently
switches to the newly active session. The one addition is the anchored late
journal write: when the active session records the captured session as its
`parentSessionId` **and** actually contains the origin's anchor entry, a
`journalAppend` commits to the inherited (fork) storage at that session's lane
revision. That is history authority: the fork copied the journal the writer was
appending to. An unrelated session, a missing anchor, or any other operation
falls back to the captured storage, so request JSON still cannot widen
authority, and the owner/generation checks are unchanged. Regression:
`a_late_journal_write_follows_the_fork_that_inherited_the_history`.

## Still open

- H05 refresh phases / config recomposition / availability: unchanged. Catalog
  publication and refresh remain the provider-runtime implementation described
  in [native-runtime.md](native-runtime.md); this lane did not restructure the
  phases, recompose configured models on refresh, or expose an availability
  snapshot.
- Overflow compact-and-retry (needs `context_edit` entries) and Pi's compaction
  entry representation (`firstKeptEntryId` vs `retainedTail`).
- Pi's full runtime rebuild across session replacement and captured-`ctx`
  invalidation; RPC session commands.
- `/new` in the TUI is still an alias of `/clear`; it does not create a new
  session file.
