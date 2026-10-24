# M09 scope-service lease verification

Lane: `lane/h-scope-lease`, base `f5002d7`. All builds/tests use offline Cargo,
8 jobs and an isolated mode-0700 temporary HOME. No package consumer changes.

## Proof boundaries

- `crates/rpi-extensions/src/services/tests/scope_leases.rs`: provider-only minting,
  forged IDs, exact owner and generation, manifest/method narrowing, 32-owner
  capacity, explicit release, completed invocation cancellation, child selection,
  scope closure, epoch revocation, service replacement (including instance mismatch
  before cleanup), caller unload, per-call cancellation, reclamation, 8-inflight
  admission and cancellation of every callback, acquire/closing races, independent
  scope/epoch isolation and session invalidation.
- SDK native tests cover capability refusal on an older host, lease calls after
  invocation expiry, clone/shared release state and RAII release.
- `crates/rpi-cli/tests/provider_packages/scope_service_lease.rs` loads a real DLL
  built from `crates/rpi-cli/tests/fixtures/scope_service_lease.rs`. The DLL registers
  both a provider and a Fabric-shaped native service forwarding to host IsIdle and
  Compact. It obtains the grant in the provider callback; the delayed worker first
  proves that an ordinary scoped client has expired, then waits for the test gate.
  Thus success cannot be an accidental overlap with a still-running callback.
- Seven loaded scenarios: child transcript compaction with main unchanged; main
  compaction with child unchanged; scope CLOSING; epoch revocation; caller/target
  unload; a newer retry admitted **after** idle observation; cancellation of admitted
  compaction. Assertions inspect persisted compaction entries, not only a reported
  result or tip (compaction can retain the same tip). Busy rejection leaves the
  newer retry running. Cancellation releases the selected lane without committing.

The loaded proof found a second task-local boundary: legacy `runtime_action`
spawns a host task. IsIdle/Compact now restore their invocation bridge's captured
scope there too, with cancellation. Restoring only the native service dispatcher
was insufficient: the service selected the child but its forwarded action selected
main. Both boundaries are covered by the transcript assertions.

## Mutation checks (each applied separately and reverted)

All eight mutations compiled and were killed by assertions (`cargo test` exit 101):

| Mutation | Proof that failed |
| --- | --- |
| Store completed invocation signal instead of epoch | callback-survival unit test: unexpected `scope service lease revoked` |
| Ignore owner equality on call | owner unit test accepted publisher's stolen consumer grant |
| Ignore narrowed method set | method unit test accepted another valid service method outside the grant |
| Ignore epoch in active check and call cancellation | epoch unit test accepted a revoked epoch |
| Reuse current service registration without matching captured instance | replacement unit test dispatched to replacement |
| Substitute main (`inherit(None, ...)`) for captured child | loaded child transcript had zero compaction entries |
| Omit selected scope in service invocation bridge | loaded child transcript had zero compaction entries |
| Abort/wait for the newer run instead of rejecting busy admission | loaded idle→busy scenario returned no required busy error |

Initial compile-only mutation attempts were corrected before recording these
results; compiler errors are not counted as mutation kills. No mutations remain.

## Gates

Installed Rust names are `1.78.0` and `1.95.0`; `cargo +1.78` itself reports an
uninstalled alias, so the equivalent installed `cargo +1.78.0` is used (no install
or network). Final gates:

- Rust 1.78.0, `RUSTFLAGS=-Dwarnings`, `check --workspace --all-targets`.
- Rust 1.78.0 SDK `check --no-default-features`, and with `--features provider`.
- Rust 1.95.0 `test --workspace`: **2427 passed, 0 failed, 26 ignored** (90 top-level suites; nested
  fixture-command test summaries excluded).
- `cargo fmt --check`.
- All seven `docs/provider-parity/check-*.mjs` scripts.

This is a host contract proof, not a claim that the read-only multiprovider package
has adopted the API. Its delayed worker must acquire before callback return and
use the lease for both `isIdle` and `compact`, with its own worker cancellation,
existing polling/deadline/debounce and inline quota/auth rotation. See
[scope-service-lease.md](scope-service-lease.md) for exact SDK usage and protocol.
