# L2-B fresh session runtime ownership

## B1 — authority generations

Replacement now retires the outgoing registry snapshot, ActionBridge and native
owners after shutdown. It drains started native writes, shuts down auth and
plugin work, then registers the loaded paths against a newly constructed bridge.
Tools, hooks, emitter and provider bindings are rebuilt. Library mapping reuse
is not authority reuse. The factory uses weak provider cells to avoid a
library → bridge → host → factory ownership cycle.

Rust host API change: `SessionReplacementContext` has an optional
`runtime_factory`; extension-bearing embedders must supply one. No SDK API,
PluginApi layout, EventTag or native JSON transport changes.

Reference: original Pi executable `agent-session-runtime.js:102-144,147-172`
and `agent-session.js:996-1008`, under
`/home/user/Projects/provider-references/multiprovider/node_modules/@earendil-works/pi-coding-agent/dist/core/`.
Their embedded TS sources are `agent-session-runtime.ts:167-223` and
`agent-session.ts:1397-1410` respectively.

Qualification: `loaded_replacement_retires_old_authority_and_constructs_a_fresh_registration`
loads a real minimal DLL, performs two replacements, verifies old registry,
command/native bridge and shutdown-token retirement and live new registrations.
This is not actual Cursor worker integration (lane C owns that).
Mutation: skipping outgoing `bridge.invalidate()` is killed (exit 101, assertion
`old command/native contexts must be stale`); mutation reverted.

## B2 — ordered replacement admission

External callers now run before-hook → validate → abort/settle → shutdown →
invalidate/drain → install/rebuild → start. Runtime callbacks use an idle-only
entry point, refusing busy calls rather than deadlocking on their invoking run.
A shared factory admission guard refuses overlapping/reentrant replacements.
TUI switch/import/fork no longer abort before the veto hook. Destination metadata,
entries and JSONL cwd header are checked before shutdown. Missing/cross-cwd
services fail closed for resume; cwd service migration is not claimed. Import
copies history into the caller's repository: its preserved source cwd is
provenance, not a project-switch instruction (existing cross-cwd import test).

Tests cover active veto, active external settlement, immediate busy refusal,
missing-cwd preparation with a live DLL, and replacement admission refusal.
Mutation: abort-and-settle before the veto is killed (exit 101, `veto must not
abort the response`); reverted; five integration tests and loaded fixture pass.
The harness tests require a multi-thread Tokio runtime for its blocking hooks.

## B3 — host commands

`/new` has a distinct command and TUI message: it creates and activates a native
JSONL session through the coordinator; `/clear` clears widgets only. RPC now
advertises `new_session`, `switch_session` (`sessionPath`) and `fork` (`entryId`,
`position` before/at), runs replacement off the input loop, and reads live
registrations for command/auth discovery. Legacy factory-free RPC embedders
refuse replacement. The host-owned API `rpc_with_context` enables it. No SDK or
ABI change. The TUI and RPC both reuse `replace_host_session`.

Fork command default is now `before`, matching
`/home/user/Projects/provider-references/multiprovider/node_modules/@earendil-works/pi-coding-agent/dist/core/agent-session-runtime.js:175-195`.
The explicit `at` path remains supported without changing harness defaults.

Tests: real offline RPC process creates a different session/file, fails a bad
resume without changing identity, resumes the original, and rejects invalid fork
IDs; loaded fixture checks before excludes and at retains the selected message.
Command-registry test distinguishes `/clear` and `/new`. Mutation: alias `/new`
back to ClearChat, killed by registry dispatch (exit 101), reverted.

## B4 — ordered loaded-tree integration and failure boundaries

All host/native new/resume/fork requests now admit and await the before-hook
before creating/opening a destination. TUI import uses the same staged path;
selectors and harness tree semantics are unchanged. A failed unpublished runtime
is revoked by an RAII guard; provider-runtime publication is deferred until all
bindings are installed. Failed registration leaves the selected durable session
and old authority closed; a subsequent host replacement can recover (not roll
back into a closed package registration).

The loaded DLL fixture now records registration generations and awaited native
lifecycle/tree events. It verifies veto-before-open, shutdown → new registration
→ start, stale old bridges, fresh before/at forks, tree veto with unchanged leaf,
extension-provided summary + label persistence, real JSONL append failure with
terminal tree-failed and no tree-success, then registration rejection and retry.
Mutation: bypass staged before-hook; killed by attempting the nonexistent resume
instead of accepting its veto (exit 101); reverted and loaded test passed.
`tree_ui` and real-process RPC tests also passed.

Boundaries: loaded fixture is not the Cursor package. Cross-cwd replacement is
refused before teardown. A post-shutdown construction failure is fail-closed,
not atomic rollback; the host can retry. Lane C must join late inherited journal
leases to its actual worker routing. Lane D owns refresh/config recomposition
and availability; this lane only rebuilds provider bindings per fresh runtime.

## Final qualification

All commands were offline with isolated mode-0700 HOME/TMP outside the worktree,
explicit shared toolchain homes, six build jobs and lane-specific target dirs.
Cargo ran foreground under `timeout 270`; incomplete compilation was rerun.

- Rust 1.78.0 `RUSTFLAGS=-Dwarnings cargo +1.78.0 check --workspace --all-targets`: pass.
- Rust 1.78.0 SDK checks `--no-default-features` and
  `--no-default-features --features provider`, both with `-Dwarnings`: pass.
- Rust 1.95.0 `cargo +1.95.0 test --workspace`: **2437 passed, 0 failed, 26 ignored**
  across 92 test/doc-test suites (baseline 2432/0/26; no new ignores).
- `cargo +1.95.0 fmt --check`: pass.
- Seven `docs/provider-parity/check-*.mjs` in default check mode: all pass
  (branch summary, compaction pure, compaction, compaction trigger, selection,
  tree, turn). No evidence regenerator or generated JSON was modified.

Gate recovery: the first full suite exposed the existing cross-cwd import
contract; the coordinator now explicitly preserves it. A subsequent default
parallel run exposed an unrelated harness environment race in
`child_turn_budget_is_per_instance_and_does_not_change_parent_or_environment`
(`Some("7")` vs `None`); no harness edit was made. A serial attempt exceeded
270 seconds, and the final unmodified default-parallel workspace run passed.

SDK/ABI: no `rpi-plugin-sdk` source, PluginApi layout/slot, EventTag number or
native JSON contract changed. Rust CLI embedders must add `runtime_factory` to
`SessionReplacementContext` literals and use `rpc_with_context` for replacement.
Package pins need no SDK source update from this lane. Runtime behavior changes
intentionally: captured ordinary contexts become stale on replacement; narrow
persistence/scope leases retain only their pre-existing authority.

Do not interpret these component tests as full closure of H05/H06/H10 or C09.
No lane A/C/D-owned source was edited. Integration interfaces: C uses existing
`ActionBridge::invalidate`/`finish_native_writes` and native owner retirement;
D continues to own `ProviderRuntime::initialize/new/activate`, refresh and
catalog availability. Construction failure never reactivates old registrations.
