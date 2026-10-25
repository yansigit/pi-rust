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
Import is validated too — by the cwd the copy runs in — and a later resume of the
archive is refused by name (B-3, see *B5* below).

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
refused before teardown. Post-shutdown failure is fail-closed: since the fix in
*B5*, every fallible construction step is staged before the first harness
mutation, so only a harness-setter failure can leave a replaced session without
extension authority — and that is reported as `ReplacedDegraded`, not as
success. Lane C must join late inherited journal leases to its actual worker
routing. Lane D owns refresh/config recomposition and availability; this lane
only rebuilds provider bindings per fresh runtime (and now runs lane D's
create-time refresh, see *B5*).

## B5 — review fixes (fix-l2bd)

Cross-lane (X-1): a fresh runtime is Pi's `ModelRuntime.create`, which refreshes
on create (`model-runtime.js:79-108`, create → `refresh({allowModelNetwork:false})`).
`SessionRuntimeFactory::rebuild` and the startup/`/reload` builders now run that
create-time phase (`ProviderRuntime::refresh_availability_on_create`) before the
catalog reaches the harness, so `/new`, `/resume`, `/fork`, `/import`, `/reload`
and process start all agree. Lane D's half of the fix is the config seed:
`ProviderRuntime::new` starts from `load_models_config()` instead of
`Default::default()`, so a publication cannot recompose against an empty config
and drop `models.json` overrides.

Replacement outcomes are explicit (B-1/B-2). `rebuild` stages every fallible
step (fresh extension session, provider runtime, credential pass, hooks, tools)
before the first harness mutation and returns `RebuildOutcome::Installed`,
`::Failed(error)` (nothing applied, safe to retry) or `::Degraded(reason)` (the
durable session is replaced but the harness has no/partial extension authority).
`SessionReplacement::ReplacedDegraded { reason }` carries the degraded case to
TUI, host actions (`"degraded"` in the JSON) and RPC. Unpublished authority is
retired with an awaited `auth_runtime().shutdown()` on every failure path (B-4),
the provider-runtime cell is published before `activate()` so a request can never
reach an unbound port (B-5), and settle is bounded (`SETTLE_TIMEOUT`) because it
runs under the admission gate (B-6).

Imports are archive-only in one rule (B-3): `validate_destination` runs for every
reason. `Import` validates the cwd the copy runs in and keeps the source bytes
verbatim; `import_session_file` writes a durable `<session>.import` provenance
marker (invisible to `.jsonl` listings), and `Resume` of a marked archive is
refused with `imported session from … is archive-only: Pi v1-v3 session logs are
not resumable` instead of a cross-cwd error from the preserved header. Import is
really validated, not bypassed: with the destination cwd gone it fails with
`Session working directory does not exist` before any teardown (existence is
checked before canonicalization, so the caller gets that message rather than a
bare io error). A failed import publish also drops this process's
`SESSION_LOCK_PATHS` entry and its `.lock` file (B-7).

Catalog publication no longer awaits the host install while holding the runtime
state mutex (D-2): the snapshot is staged under the lock, the lock is dropped for
`install_native_providers`/`replace_model_catalog`, then a monotonic commit
counter re-validates that nothing else published before the catalog/epoch/chain
swap. `inspect` derives availability from one snapshot (D-6), a transient status
read keeps the last committed credential status instead of flapping to `Unknown`
(D-4), the availability loop stops on cancellation (D-3), and a provider-local
refresh error is reported instead of the availability pass's error (D-5). Epoch
revocation on every catalog commit stays intentional and is now documented at the
commit site (D-7).

Tests: `construction_seeds_disk_config_and_credential_availability` (D-1/X-1,
mechanism), `imported_archive_is_archive_only_for_import_and_resume` (B-3,
through `validate_destination` rather than around it, including a second archive
refused for import once the destination cwd is gone),
`rebuild_failure_after_the_swap_is_reported_as_degraded` (B-1/B-2, failure after
`set_active_session`), `import_is_validated_and_only_import_accepts_an_archive`
(the per-reason rule at the predicate), and
`degraded_outcome_is_surfaced_and_failures_stay_retryable` (the coordinator's
`RebuildOutcome` translation, so a degraded swap can never be flattened into a
clean replacement). `host_install_runs_with_the_state_lock_released` states the
D-2 rule directly: the runtime state mutex is free while the host snapshot is
installed.

Mutation checks on the new guards (each killed, then reverted):
revert the config seed → D-1 test fails; restore the `reason != Import` bypass →
the import test fails on the missing destination cwd; delete the archive rule →
the per-reason test fails; flatten `Degraded` into `Replaced` → the outcome test
fails; hold the state mutex across the host install → the D-2 test fails.

### fix-l2bd qualification

Gates were re-run on the fixed tree (`lane/fix-l2bd` = `lane/l2-integration` +
this lane), foreground under `timeout 270`, with lane target dirs
`/home/user/Projects/.targets/fix-l2bd-178` (Rust 1.78) and
`…-195` (Rust 1.95):

- `cargo fmt --all --check`: pass.
- `RUSTFLAGS=-Dwarnings cargo +1.78.0 check --workspace --all-targets --locked`: pass.
- `cargo +1.78.0 check -p rpi-plugin-sdk --no-default-features` and `… --features
  provider`, both `--locked`: pass.
- `cargo +1.95.0 test --workspace --locked --tests`: 2444 passed, 0 failed, 23
  ignored across 95 test binaries; `… --doc`: 17 passed, 0 failed, 3 ignored.
  **Combined 2461 passed, 0 failed, 26 ignored** — integration baseline
  2455/0/26 plus this lane's six new tests.
- Seven `docs/provider-parity/check-*.mjs`: pass.

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
`SessionReplacementContext` literals and use `rpc_with_context` for replacement;
both are documented in rustdoc (`SessionReplacementContext::runtime_factory`,
`rpc_with_context`) including the `ReplacedDegraded`/`degraded` signal (B-8).
Package pins need no SDK source update from this lane. Runtime behavior changes
intentionally: captured ordinary contexts become stale on replacement; narrow
persistence/scope leases retain only their pre-existing authority.

Do not interpret these component tests as full closure of H05/H06/H10 or C09.
No lane A/C/D-owned source was edited. Integration interfaces: C uses existing
`ActionBridge::invalidate`/`finish_native_writes` and native owner retirement;
D continues to own `ProviderRuntime::initialize/new/activate`, refresh and
catalog availability. Construction failure never reactivates old registrations.
