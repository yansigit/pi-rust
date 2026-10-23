# H-DELEG — partial host/SDK safety slice

Base: `ae71d44`, branch `lane/parity-delegation`. Changes remain uncommitted
for mandatory independent review. This is **not full H-DELEG acceptance**.

## Implemented

- Append `RuntimeActionId::IsIdle = 26` after `SideQuestion = 25`, including
  integer conversion and the existing exhaustive numeric compatibility test.
  No ABI version, vtable layout, or existing action number changes.
- Dispatch `IsIdle` to the host; unsupported hosts return an error. CLI reads
  `for_current_execution().native_context().isIdle`, not global busy state.
  It returns only `{"isIdle": bool}` and ignores plugin-supplied state.
- Recompose a child's inherited catalog from base instances rather than sharing
  the parent's wrapper chain. Wrapper publication identities are distinct for
  parent and siblings; repeated access to the same child retains its chain.
  This does **not** claim fresh child auth bindings or a delegation catalog.
- Refuse retained continuation admission after its captured child scope closes.
  Parent scope and cancellation state remain unaffected.
- SDK missing continuation callbacks return `Err`; missing delegation streaming
  produces a terminal error event instead of a detached-task panic/hung consumer.
  Version-1 wire messages and successful callback behavior are unchanged.

## Focused regression tests

Six new tests cover:

1. SDK missing auth/release callbacks.
2. SDK missing stream callback and bounded terminal completion.
3. Real CLI root-active/child-idle and child-active/root-idle queries, plus
   before-startup, unbound-scope and disposed-child refusal.
4. Foreign-thread action trampoline transport of idle true/false and Compact
   successful result/error JSON.
5. Child/sibling wrapper-chain identity isolation and same-child reuse.
6. Retained child continuation revocation without parent revocation.

Compact's current CLI contract is preserved: it awaits `lane.compact`, converts
`HarnessResult::Err` into the action error, and returns `{runId, outcome}` with
the outcome formatted by `Debug` on success. The harness also represents some
compaction failures as `Ok(CompactionResult { outcome: Failed { ... } })`;
these remain visible in the outcome string, not converted to a new error ABI.
The new trampoline test uses an injected host result/error; it is **not** a
loaded-plugin or actual LLM compaction success/failure proof. No new Pi
`onComplete`/`onError` callback ABI is introduced or claimed.

The existing installed-library test
`native_wrapper_delegates_with_pooled_auth_and_canonical_endpoint` was explicitly
executed with `--ignored --exact` against freshly built `plugin-stub` (and
`RPI_TEST_PROVIDER_LIBRARY` set): **1 passed, 0 ignored**. It exercises the real
version-1 wrapper/SDK callbacks, isolated fake credentials, canonical endpoint
binding and a loopback stream. This is a backwards-compatibility baseline, not
cross-provider or loaded `IsIdle` acceptance. The `.so` hash is in the JSON.

## Still required

Cross-provider version 2 remains disabled (unsupported version), rather than
accepting unsafe caller-auth fallback. No public `resolve_ambient` or target
provider SDK method is exposed without a working secure host implementation.

Remaining work includes immutable canonical target catalogs with weak targets;
exact caller-registration exclusion; ancestry/depth propagation across auth and
stream tasks; host-private async ambient preparation combining `AuthBinding`
with configured/static `ExecutionBinding`; opaque sealed receipts bound to
continuation/model/provider/scope/owner generation/publication; atomic stale
admission revocation while retaining admitted physical snapshots; child-owned
resolver/auth/catalog publication; and per-attempt release/replay checks.

The configured provider currently keeps its `ExecutionBinding` and optional
`AuthBinding` inside `provider/factories.rs::ConfiguredProvider`. The existing
`Provider` surface supports streaming/preflight/catalog replacement, not private
prepared-attempt extraction. A follow-up must retain a typed host-private
preparation path through factory construction/catalog replacement/routed
providers before enabling delegation. This slice adds neither a session-attached
delegation slot nor a raw auth export to bridge that gap.

All cross-provider poisoned-descriptor, receipt replay, cycle/auth-reentry,
refresh/unload admission and loaded plugin-stub scenarios remain unverified.
No gated skip is counted as loaded-library proof. No live services or real
credentials are used. See `host-delegation-results.json` for mechanically
collected command results and source/log hashes.

Evidence collector: `/tmp/opencode/parity-delegation/collect-evidence.py` (run
with the parity environment sourced). It extracts Cargo test totals directly
from logs and hashes the modified sources and logs; it does not generate a
parity fixture or reinterpret gated skips as executed acceptance cases.
Broad suite counts can include pre-existing environment-gated tests and ignored
tests; they are not per-scenario native-library acceptance counts.

## Validation outcome

- Rust 1.78 workspace/all-targets `-D warnings` check: passed. SDK
  `--no-default-features` and `--features provider` checks: passed.
- Rust 1.95 SDK tests: 75 passed, 3 pre-existing ignored across reported suites.
- Rust 1.95 extensions tests: 214 passed, 3 pre-existing ignored across suites.
- Rust 1.95 CLI lib: 686 passed, 7 pre-existing ignored (do not double-count
  its nested one-test subprocess summary).
- Loaded version-1 plugin-stub baseline: 1 passed, 0 ignored.
- Rust 1.95 full `cargo test --workspace --locked --offline`: passed, exit 0.
  Per-runner result lines are retained in the evidence, without treating
  environment-gated passes as separately verified loaded acceptance.
- All six new regression tests passed. Scoped formatting and `git diff --check`
  passed. Rust 1.78's rustfmt component is unavailable; Rust 1.95 formatted the
  owned source files with child-module traversal disabled.

Initial implementation failures and a transient existing parallel
`saturated_pipeline_cancels_uncommitted_content_and_frees_every_frame`
metadata assertion failure (0 versus 3) are retained in the evidence/logs.
No stream-test assertion or scheduling logic was weakened; the subsequent full
extensions and workspace runs passed. A serial extensions attempt was
interrupted by the enclosing shell's 120-second timeout and is not counted as a
pass. Full H-DELEG remains partial despite these passing baseline gates.
