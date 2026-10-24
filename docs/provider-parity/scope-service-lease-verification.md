# M09 scope-service lease verification

Initial lane: `lane/h-scope-lease`, base `f5002d7`; review fixes:
`lane/h-scope-lease-fix`, base `229ce5a`. All builds/tests use offline Cargo,
8 jobs and an isolated mode-0700 temporary HOME. No package consumer changes.

## Proof boundaries

- `crates/rpi-extensions/src/services/tests/scope_leases.rs`: provider-only minting,
  forged IDs, exact owner and generation, manifest/method narrowing, 32-owner
  capacity, explicit release, completed invocation cancellation, child selection,
  scope closure, epoch revocation, service replacement (including instance mismatch
  before cleanup), caller unload, per-call cancellation, reclamation, 8-inflight
  admission and cancellation of every callback, acquire/closing races, independent
  scope/epoch isolation and session invalidation. A cancellation-ignoring foreign
  callback barrier proves that cancellation returns promptly while retaining all
  8 per-grant and 256 process-wide permits. A separate registered output
  deallocator barrier then holds all 256 outputs after the callbacks return;
  both local and global admission remain exhausted until that barrier is released,
  after which both recover.
- `crates/rpi-plugin-sdk/tests/provider_context_export.rs` uses a mock host handle
  registry to test old-host capability refusal, final-clone-only RAII release,
  shared/idempotent explicit release, Drop outside a Tokio runtime, and best-effort
  Drop after host invalidation or destruction. Post-invocation calls are proved
  by the loaded DLL below, not by the SDK mock tests.
- `crates/rpi-extensions/src/provider/wrappers/v2_tests.rs` waits for test-only
  monitor-exit notifications after normal continuation destruction, not merely
  the destruction signal.
- `crates/rpi-cli/tests/provider_packages/scope_service_lease.rs` loads a real DLL
  built from `crates/rpi-cli/tests/fixtures/scope_service_lease.rs`. The DLL registers
  both a provider and a Fabric-shaped native service forwarding to host IsIdle and
  Compact. It obtains the grant in the provider callback; the delayed worker first
  proves that an ordinary scoped client has expired, then waits for the test gate.
  Thus success cannot be an accidental overlap with a still-running callback.
- Nine loaded scenarios: main grant through a captured child transport and child
  grant through a captured main transport (service scope and persisted transcript
  assertions); child transcript compaction with main unchanged; main
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

The initial lane's eight mutations compiled and were killed by assertions (`cargo test` exit 101):

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

### Review-fix mutations (applied individually and reverted)

| Mutation | Proof that failed (`cargo test` exit 101) |
| --- | --- |
| Restore transport scope/identity for grant dispatch | loaded `main-via-child`: service scope was `scope-2`, expected main (`null`); persisted compaction assertions also select only the grant's transcript |
| Keep permits in the async caller instead of the blocking callback | cancellation-ignoring barrier test: `cancelled foreign work released per-grant admission: Elapsed(())` |
| Omit final-clone Drop's release request | SDK final-clone test observed 0 releases instead of 1 |
| Release blocking callback permits before the registered output deallocator | deallocator barrier test: `blocked output deallocator released admission: Elapsed(())` |

The first permit mutation attempt timed out because the test's extra call was
itself admitted and blocked; the regression now bounds that probe and fails an
assertion promptly. Only the subsequent exit-101 assertion failure is counted.
All mutations were reverted before the final gates. The deallocator mutation
compiled and failed with exit 101; after restoration all seven scope-lease tests
passed. The loaded nine-scenario test also passed, and grepping its captured
`--nocapture` output for `panicked` or `called .*unwrap` found no fixture panic.
The fixture now handles a nonzero runtime-action result before inspecting scope.

## Gates

Installed Rust names are `1.78.0` and `1.95.0`; `cargo +1.78` itself reports an
uninstalled alias, so the equivalent installed `cargo +1.78.0` is used (no install
or network). Final gates:

- Rust 1.78.0, `RUSTFLAGS=-Dwarnings`, `check --workspace --all-targets`.
- Rust 1.78.0 SDK `check --no-default-features`, and with `--features provider`.
- Rust 1.95.0 `test --workspace`: **2432 passed, 0 failed, 26 ignored** (90 top-level suites; nested
  fixture-command test summaries excluded).
- `cargo fmt --check`.
- All seven `docs/provider-parity/check-*.mjs` scripts.

The review fixes add five tests (one foreign-execution admission test and four
SDK RAII tests), and strengthen the existing loaded and monitor tests. The
`aa3dc5e` follow-up strengthens that same admission test with the separate
output-deallocator barrier; it adds no test cases. All gates above were rerun
successfully, with the workspace still at 2432 passed, 0 failed, 26 ignored.
The temporary HOME must be outside the worktree: an initial in-worktree HOME
triggered the self-update test's intentional project-directory refusal; rerunning
the full suite with an external mode-0700 temporary HOME passed. The SDK
public surface, implementation, and C ABI remain unchanged from `229ce5a`; the
package lane pinned to that revision needs no API migration for these fixes.

This is a host contract proof, not a claim that the read-only multiprovider package
has adopted the API. Its delayed worker must acquire before callback return and
use the lease for both `isIdle` and `compact`, with its own worker cancellation,
existing polling/deadline/debounce and inline quota/auth rotation. See
[scope-service-lease.md](scope-service-lease.md) for exact SDK usage and protocol.
