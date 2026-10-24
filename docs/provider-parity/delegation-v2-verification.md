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

## Limits and coordinator decisions

The core host path is implemented, but this is not full design acceptance:

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
