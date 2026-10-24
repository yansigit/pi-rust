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
services fail closed; cwd service migration is not claimed.

Tests cover active veto, active external settlement, immediate busy refusal,
missing-cwd preparation with a live DLL, and replacement admission refusal.
Mutation: abort-and-settle before the veto is killed (exit 101, `veto must not
abort the response`); reverted; five integration tests and loaded fixture pass.
The harness tests require a multi-thread Tokio runtime for its blocking hooks.

Remaining qualification and final gates are recorded below as they run. Do not
interpret this lane's component tests as closure of H05/H06/H10 or C09.
