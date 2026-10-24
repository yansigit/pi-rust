# Delegation v2 host verification

This records this lane's verification, not acceptance of the entire approved
package/host matrix. Base: `7dfadb0`; worktree: `pi-rust--h-deleg-v2`.
No ledger, mapping, inventory, or serialized parity reference fixture was changed.

## Commits

| Step | Commit | Change |
| --- | --- | --- |
| H1 | `6290f56` | Host-only scope, epoch, ancestry, target and receipt authority |
| H2 | `1f0b571` | Execution-scoped route publication and epoch revocation |
| H3 | `4ffdc3e` | Opaque target lookup, sealed ambient/account authentication |
| H4 | `ed6c732` | Full-chain target dispatch, cancellation and bounded forwarding |
| H5 | `e813df3` | Negotiated SDK target and receipt adapters |
| H6 | `b322a22` | Compaction selects the current execution target |
| H7 | this commit | Loaded-library adversarial tests and raw-stream unload cancellation |

The loaded cancellation test uncovered a further lifecycle requirement: raw
plugin provider streams, and wrapper dispatch monitors, must observe the session's
shutdown token. H7 adds this observation without changing the C layout. Otherwise
unloading while a foreign emit callback was blocked could leave it pinned.

## ABI and SDK

No C layout, callback signature, vtable slot, numeric runtime action ID, or SDK
enum discriminant was added or reordered. The existing `provider_continuation`
and `delegate_provider_stream` callbacks carry the versioned JSON envelopes.
The continuation descriptor remains `{version:1,id,generation}`.

New public SDK API under `rpi_plugin_sdk::provider::composition`:

```rust
#[derive(Clone)]
pub struct AmbientTarget { /* private */ }
impl AmbientTarget {
    pub fn model(&self) -> &Model;
}
impl Continuation {
    pub async fn supports_delegation_v2(&self) -> Result<bool, String>;
    pub async fn resolve_target(
        &self, provider: &str, model: &str, signal: CancellationToken,
    ) -> Result<AmbientTarget, String>;
    pub async fn resolve_ambient(
        &self, target: &AmbientTarget, signal: CancellationToken,
    ) -> Result<AuthReceipt, String>;
    pub async fn resolve_target_auth(
        &self, target: &AmbientTarget, store_id: &str, account_id: &str,
        options: &SimpleStreamOptions,
    ) -> Result<AuthReceipt, String>;
    pub fn delegate(
        &self, target: &AmbientTarget, receipt: Option<AuthReceipt>,
    ) -> Result<Arc<dyn Provider>, String>;
    pub async fn release_target(&self, target: AmbientTarget) -> Result<(), String>;
}
```

`Continuation::resolve_auth`, `provider`, and
`release_auth(&self, receipt: AuthReceipt) -> Result<(), String>` retain their
public signatures. `release_auth` now releases either receipt version and checks
its issuing continuation. `AuthReceipt` remains cloneable with private fields;
neither it nor `AmbientTarget` implements Debug or Serialize.

String action names (not numeric IDs):

- v1 `capabilities` returns `delegationVersions:[1,2]`, `opaqueTargets:true`,
  `ambientReceipts:true`, `epochRevocation:true`.
- v2 `resolveTarget {provider,model}` returns `{version:2,target,model}`.
- v2 `auth.resolveAmbient {provider,model,target}` returns `{version:2,receipt}`.
- v2 `resolveAuth {target,storeId,accountId,options}` returns `{version:2,receipt}`.
- v2 `releaseTarget {target}` and `releaseAuth {receipt}` return `{version:2}`.

All operation requests include `continuation`. Delegated streams accept
`{version:2,continuation,target,authReceipt?,context?,options?}`. Authority-bearing
model descriptors are rejected. Targets and receipts are continuation-owned;
tables are independently bounded at 64 entries. Releasing a target cascades to
its receipts and cancels admission waiters.

## Error strings

Read-only TypeScript references under
`/home/user/Projects/provider-references/multiprovider`:

| Exact string/template | Reference |
| --- | --- |
| `multiprovider: virtual backend unavailable: provider "<provider>" is not registered` | `src/virtual.ts:164` |
| `multiprovider: virtual backend unavailable: provider "<provider>" has no model "<model>"` | `src/virtual.ts:168` |
| `Provider stream ended without a terminal event` | `src/virtual.ts:345`, `src/lift.ts:322` |
| `multiprovider: exhausted virtual backends for "<id>"` | `src/virtual.ts:384`; package policy, not a new host policy |
| `multiprovider: exhausted account attempts for "<provider>"` | `src/lift.ts:361`; package policy |
| `multiprovider: session not ready` | `extensions/multiprovider.ts:927`; package policy |
| `multiprovider: compacted session context before account failover` | `extensions/multiprovider.ts:734`; package notification |

New host security errors have no TS parity claim:
`delegation target belongs to a different continuation`,
`delegation epoch revoked`, `provider delegation cycle`,
`provider wrapper instance reentry`, `auth receipt target mismatch`,
`delegation target capacity exhausted`, `auth receipt capacity exhausted`.
The SDK additionally refuses unsupported hosts with `host lacks secure delegation v2`.
Other malformed-envelope diagnostics are validation errors, not translated TS policy.

## Tests and negative controls

New loaded-library tests, included by `crates/rpi-cli/tests/provider_packages.rs`:

- `loaded_frozen_legacy_prefix_still_registers_on_the_new_host`: frozen old short
  vtable prefix loads without accessing the new tail.
- `loaded_active_instance_reentry_fails_instead_of_skipping_the_node`.
- `loaded_concurrent_invocations_do_not_share_active_ancestry`.
- `loaded_sequential_retries_derive_from_the_original_ancestry`.
- `loaded_nested_virtuals_run_every_target_wrapper_and_refuse_poison_and_cycles`:
  exact MP_A→MP_B→MP_C wrapper order; poisoned descriptor/options; absent
  provider/model; A→B→A; receipt misuse and releases.
- `loaded_cli_runtime_resolves_the_target_binding_without_source_credentials`:
  actual CLI runtime, distinct source/middle/target fake credentials, then a
  target pool account replacing target ambient auth. Physical C asserts the
  exact selected key; CLI output must contain no fake credentials.
- `loaded_bounded_forwarding_resumes_without_loss_or_reordering`: pause a stream
  of 1024 deltas, verify the producer cannot finish, resume, assert every delta
  in exact order and exactly one success terminal.
- `loaded_cancel_revoke_and_unload_drain_blocked_foreign_emit`: caller cancel,
  epoch revoke and extension unload while bounded forwarding is blocked; file
  handshakes verify the foreign producer exits and finishes.

Unit coverage is in `provider/delegation/tests.rs`,
`provider/wrappers/v2_tests.rs`, `provider/wrappers/chain_tests.rs`,
`provider_runtime/tests/delegation.rs`, SDK composition tests, and
`extensions_actions/lifetime_tests.rs`. It covers authority binding,
provider/instance recurrence, independent retries, finite tables, cascade/drop
cancellation, sanitized metadata, negotiation, canonical auth binding, committed
versus failed publication, runtime drop, and child compaction selection.

Temporary mutations were reverted. Observed expected failures:

| Mutation | Failing test/assertion |
| --- | --- |
| Omit committed epoch revocation | `successful_publication_revokes_but_failed_install_keeps_epoch`, target signal not cancelled |
| Bypass ambient provider/model identity check | `opaque_operations_bind_receipts_and_redact_target_metadata` |
| Preserve source API key at delegated boundary | full-chain and concurrent invocation tests, `options.api_key.is_none()` |
| Bypass SDK target-owner check | `opaque_sdk_envelopes_and_owner_checks` |
| Restore main-lane compaction selection | `compaction_refuses_missing_child_target_instead_of_falling_back_to_main` |
| Remove session shutdown cancellation | loaded cancel/revoke/unload test, foreign drain handshake timed out |
| Disable provider-cycle guard | `provider_cycles_and_retry_branches` |
| Disable instance-reentry guard | `wrapper_guard_is_instance_local` |
| Disable receipt-target comparison | `capabilities_are_continuation_target_scope_and_epoch_bound` |
| Disable receipt-release cancellation | `releasing_or_dropping_capabilities_cancels_admitted_waiters` |
| Disable scope/epoch/revocation validation | authority-binding test returned wrong-continuation instead of epoch-revoked |

This is targeted mutation coverage, **not** a claim that every individual new
validation predicate has been independently mutated.

## Review fixes on `lane/h-deleg-v2-fixes`

- `raw_v2_auth_rejects_caller_credentials_before_target_callbacks_or_account_lookup`
  exercises raw continuation JSON for both v2 auth actions. API keys, headers,
  authentication objects, secret and non-secret env overrides fail with a clear
  redacted options error before any registered target auth/store callback runs.
  Empty env/minimum validity still reaches account lookup (a missing account
  fails rather than producing a receipt). No real credentials or HOME access.
- Negative control: temporarily accepting `apiKey` in the v2 parser makes this
  test fail with `credential store operation failed` instead of the required
  options rejection (exit 101); reverted. Restored test passes.
- No new loaded-library fixture here: the raw host callback is exercised directly;
  the concurrent test lane owns loaded-library fixture changes.

## Gates

Offline Rust 1.78 workspace/all-target `-D warnings` checks passed at H2, H4 and
H7. Final SDK no-default-features and no-default-features+provider checks passed.

Final `cargo +1.95.0 test --workspace --locked`: **2368 passed, 0 failed,
26 ignored**, across 90 test summaries (including doc tests). Log:
`/tmp/h-deleg-v2/test-195.log`. Existing ignored tests were not modified; no new
ignore attributes were introduced. The new DLL subset also passed on 1.78:
8 passed, 0 failed, 0 ignored. Extension 1.78 unit suite: 227 passed, 0 failed,
2 existing ignored.

All seven `docs/provider-parity/check-*.mjs` scripts passed in check mode.
`git diff --check` passed.

**`cargo fmt --check` fails on existing baseline formatting outside this change.**
The first failure, `crates/rpi-cli/src/config.rs:1410`, reproduces on the file
extracted from `7dfadb0`; see `/tmp/h-deleg-v2/fmt-baseline.log`. Other reported
files are unchanged CLI lifecycle/TUI and harness/compaction files. Modified
Rust files and new DLL fixture source were rustfmt-formatted. The unrelated
baseline was deliberately not reformatted. Full output: `/tmp/h-deleg-v2/fmt.log`.

## Original H1–H7 limits and coordinator decisions (historical)

The following records the original handoff at `61baa99`; see the follow-up
verification below for the added coverage and current gate results.
The core host path was implemented, but this was not full design acceptance:

- Parent/child scope and wrong-epoch authority are covered by host unit tests,
  not a new loaded-DLL parent/child transfer scenario.
- The DLL epoch test revokes its injected real epoch; committed/failed runtime
  publication is tested separately at runtime unit level, not during a loaded
  DLL stream.
- Compatible/incompatible retry/compaction service negotiation and a paused
  async auth/preflight callback across unload do not have new combined
  loaded-library scenarios here. Existing service/lifecycle tests still pass.
- Mutation coverage is targeted, not exhaustive for every validation branch.
- The workspace formatting gate remains red on the supplied baseline.

These are verification gaps, not permission to weaken fail-closed behavior.
Coordinator decision: whether to authorize a separate baseline formatting change,
and whether these remaining acceptance scenarios must be supplied before package
lane acceptance. No architectural ambiguity was resolved by introducing a host
port, changing ABI layout, or reviving the superseded cross-provider refusal.

## M-VIRTUAL-B binding

Bind inside the existing global wrapper. Keep owned aliases or a fail-closed
base; do not put virtual dispatch on physical base providers. Negotiate v2,
resolve the target by registered provider/model identity, resolve its ambient
receipt, and use `delegate(target, Some(receipt))`. Target wrappers remain active;
a target pool may select its own account with existing same-provider receipts
or the target-aware account API. Do not copy source key/environment/header/
endpoint authority into the target call. Release receipts and targets on every
retry/exit; preserve the original continuation for independent retries. Do not
fallback to v1 cross-provider delegation. Preserve package error and notification
strings listed above and bind retry/compaction services to the selected execution
scope. Package service negotiation and retry policy remain package-lane work.

## Verification-gap follow-up

All new loaded tests compile the same independent Rust cdylib, without a package
checkout, network fetch, environment-dependent success skip, or new ignored test.
The build helper is shared between the integration target and runtime unit test.
Unloading tests wait for explicit entry notifications, then check both the DLL's
error return and destruction of the pending Rust future. File handshakes wait for
nonempty/completed content rather than racing file creation. No sleeps were added.

### Coverage by requested gap

1. **Capability transfer** —
   `loaded_parent_child_capabilities_cannot_be_used_or_released_in_either_direction`
   passes actual target/receipt strings between the loaded parent's `a` and
   delegated child's `b` continuations. Both directions attempt ambient use,
   target release, receipt release, and delegate with an own target plus foreign
   receipt. The test then starts another loaded session with a fresh execution
   scope/epoch and continuation and attempts reuse of the saved authority.
   Own-capability delegation still succeeds after all denied foreign releases.
2. **Publication during a loaded stream** —
   `loaded_runtime_publication_terminates_active_stream_and_rejects_stale_capabilities`
   uses an actual `ProviderRuntime` and its catalog `publish` action while the
   loaded synchronous emit path is blocked by backpressure. It checks epoch
   cancellation, exactly one error terminal (`delegation epoch revoked`), stream
   closure, and rejected target/receipt lookup. In addition, all three loaded
   wrappers try stale ambient use, receipt release, and delegation through the
   real C continuation/delegate callbacks after publication, with the same error.
   The test waits for the C emitter and every wrapper's completion handshake.
3. **Pending auth/preflight unload** —
   `loaded_unload_cancels_pending_target_auth_and_drops_the_future` and
   `loaded_unload_cancels_pending_target_preflight_and_drops_the_future` stop the
   loaded session only after the selected target's ambient resolver or provider
   preflight has entered a deliberately pending future. Each verifies bounded
   stream completion, `authentication cancelled` returned to the DLL, and the
   pending future's drop notification. Auth must not reach its success marker.
4. **Retry/compaction host boundaries** —
   `retry_targets_accept_selected_scope_and_reject_other_scope_or_revoked_epoch`
   resolves two compatible retry targets in the selected runtime, rejects a
   different runtime scope, rejects the old epoch after publication, and accepts
   the freshly published epoch. Actual repeated loaded delegation is already
   covered by `loaded_sequential_retries_derive_from_the_original_ancestry`.
   `compaction_succeeds_on_selected_child_and_rejects_foreign_or_closed_scope`
   invokes the host compaction action on a real child harness, then refuses a
   foreign scope and the closed child instead of falling back to main. This
   exercises successful host operation admission (empty history can decline
   summarization), not a model-generated summary. Existing
   `calls_are_private_versioned_and_retain_the_callers_identity` covers generic
   service version incompatibility. These tests cover host execution/authority
   contracts; they do **not** introduce or claim an end-to-end package-specific
   retry/compaction service negotiation protocol. That protocol/policy remains
   package-lane work, as in the original design handoff.
5. **C adapter → typed bounded queue** — existing
   `loaded_bounded_forwarding_resumes_without_loss_or_reordering` already sends
   1,024 events through the actual synchronous C callback and consumes in order
   after deliberate backpressure. Existing
   `loaded_cancel_revoke_and_unload_drain_blocked_foreign_emit` now additionally
   requires the DLL's nonzero-emit marker for cancel, revoke, and unload.
   `loaded_c_emit_drains_more_than_four_mib_in_order` adds 1,024 independently
   checked 8-KiB deltas (over 8 MiB total), waits until the producer is blocked,
   consumes every delta in order, and requires exactly one successful terminal.
   The real host defaults remain 64 events / 4 MiB; no test increases them.

### Bug proved and minimal production fix

The combined publication test first failed with `left: Aborted / right: Error`:
revocation cancelled the public producer token, which rewrote the epoch failure
into `Request aborted`. Commit `9d3a07a` changes only admission ownership and error
preservation: internal FFI admission uses a child of the public producer token;
the streaming driver retains the continuation's epoch signal and publishes the
revocation cause as an error terminal. User cancellation still propagates to the
child and keeps its aborted terminal. No ABI, service port, routing, auth policy,
or production queue limit changed.

### Mutation checks (temporary changes, all reverted)

Each row was run with a single named test, offline, on Rust 1.95.0. Every killed
mutant compiled and exited 101 with `test result: FAILED`; none was a compile
failure or outer 270-second cargo timeout. Restored sources pass the final gates.

| New test | Temporarily broken production guard | Observed failure |
| --- | --- | --- |
| `loaded_parent_child_capabilities_cannot_be_used_or_released_in_either_direction` | Ignore `CapabilityTables::release_target` error in `provider/wrappers/v2.rs` | DLL assertion `foreign capability accepted`; host terminal Error instead of Stop |
| `loaded_runtime_publication_terminates_active_stream_and_rejects_stale_capabilities` | Restore `opts.signal = prod.cancellation_token()` in `provider.rs` | Aborted instead of Error, `Request aborted` |
| `loaded_unload_cancels_pending_target_auth_and_drops_the_future` | Replace `operate_v2` epoch/signal cancellation select with `work.await` | Pending resolver destruction misses bounded notification deadline (`Elapsed(())`) |
| `loaded_unload_cancels_pending_target_preflight_and_drops_the_future` | Same removal of cancellation select | Pending preflight destruction misses bounded notification deadline (`Elapsed(())`) |
| `retry_targets_accept_selected_scope_and_reject_other_scope_or_revoked_epoch` | Remove scope/epoch identity comparison from `InvocationAncestry::validate` (keep revoked-token check) | Other-scope resolution unexpectedly succeeds |
| `compaction_succeeds_on_selected_child_and_rejects_foreign_or_closed_scope` | Remove `for_current_execution()` from host `compact` | Foreign scope unexpectedly gets a successful main-lane compaction response |
| `loaded_c_emit_drains_more_than_four_mib_in_order` | Remove `.min(self.payload_bytes)` from typed queue permit weight | Oversized accumulated snapshot cannot acquire capacity; test's 15-second timeout fails |

An exploratory mutation removing only the outer `!continuation.active()` check
survived the auth test: the independent async cancellation select still drops the
pending future. It was reverted; the two pending-call rows above remove that
actual guarded cancellation mechanism and are killed. Runtime-publication
mutation was also rerun after adding the loaded stale-callback handshakes.

### Offline gates

Environment: sourced `/home/user/Projects/.parity-env.sh`, explicit existing
`RUSTUP_HOME` / `CARGO_HOME`, `RUSTUP_AUTO_INSTALL=0`, `CARGO_NET_OFFLINE=true`,
`CARGO_BUILD_JOBS=6`. Tests use an isolated mode-0700 temporary HOME and the explicit
Rust 1.95.0 compiler for DLL fixtures. No real auth files or credentials were used.
Cargo ran in the foreground under `timeout 270`, with exit status printed.

- Rust 1.78.0 `check --workspace --all-targets --locked`, `RUSTFLAGS="-D warnings"`:
  **PASS** (target `/home/user/Projects/.targets/h-deleg-v2-178`).
- Rust 1.95.0 `test -p rpi-cli --test provider_packages --locked`:
  **19 passed, 0 failed, 11 pre-existing ignored**; all 12 delegation tests run.
- Rust 1.95.0 `test --workspace --locked`: **2,374 passed, 0 failed,
  26 pre-existing ignored**, 89 test/doc-test result groups (excluding the
  separately spawned CLI test worker's nested result). CLI lib:
  **710 passed, 0 failed, 7 pre-existing ignored**.
- `rustfmt --check --edition 2021` on changed Rust files only and
  `git diff --check`: **PASS**. Unrelated baseline formatting is untouched.

The first full run used a HOME inside the worktree and hit the unrelated
self-update safety test's `refusing to create a self-update command directory
inside the caller project`. Moving isolated HOME outside the worktree corrected
that setup. The next run hit the existing updater test race `Text file busy (os
error 26)` in `direct_update_rolls_back_when_post_replace_validation_fails`.
A full, unchanged rerun passed; no assertions or unrelated code were modified.
