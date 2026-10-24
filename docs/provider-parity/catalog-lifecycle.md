# H05 catalog refresh lifecycle — lane L2-D

Scope: catalog/runtime coordination in `crates/rpi-cli/src/model_registry.rs`
and `crates/rpi-cli/src/provider_runtime.rs`. This is offline component and
loaded-fixture qualification, **not closure of H05 or the cross-lane L2 matrix**.
No SDK Rust surface, C ABI slots, discriminants, or native protocol version changed.
The existing version-1 providers port now accepts a refresh without `providerId`
(all providers), or `providers: string[]` (deduplicated selection). The existing
single-provider response remains `{ revision }`; batch responses additionally
contain `aborted` and a provider-keyed `errors` object. `inspect` adds `available`
without removing existing fields.

## Original Pi evidence

Original embedded TypeScript, extracted without modifying the reference tree:

- `/home/user/Projects/provider-references/multiprovider/node_modules/@earendil-works/pi-coding-agent/dist/core/model-runtime.js.map`,
  `sourcesContent` `model-runtime.ts:848–888`: load config → selected recomposition
  or rebuild → model snapshot → provider refresh → model snapshot → availability
  → return aborted/errors.
- Same embedded `model-runtime.ts:333–340`, `model-runtime.ts:342–432`: model
  snapshot construction, latest global sequence, provider sequences, and
  credential-change invalidation of earlier global passes.
- Direct executable counterparts:
  `/home/user/Projects/provider-references/multiprovider/node_modules/@earendil-works/pi-coding-agent/dist/core/model-runtime.js:587–628`
  and
  `/home/user/Projects/provider-references/multiprovider/node_modules/@earendil-works/pi-coding-agent/dist/core/model-runtime.js:189–248`.

The host retains its existing publication-ticket errors and JSON transport rather
than claiming TS Error-object serialization equivalence.

## Implemented boundaries

1. Refresh reloads `models.json`, recomposes selected/all catalog overrides,
   installs execution-catalog replacements, updates the host model snapshot,
   and only then invokes supported native `refreshCatalog` callbacks.
2. Config removal restores the pre-override catalog; config-only discovery
   entries disappear. Provider-level URL overrides apply to existing models.
   Callback publications reapply the committed selected configuration.
3. All affected generations are reserved before callbacks. Single-use,
   owner/provider/generation-bound publication tickets remain authoritative.
   An older global callback cannot overwrite a newer provider refresh.
4. A committed catalog change still revokes the delegation epoch. Cancelled
   admission and failed host installation do not commit a replacement epoch.
5. Local credential-presence/expiry snapshots use catalog revision plus
   global/provider sequences. Credential mutations invalidate before invocation
   and after completion (including error/cancellation), then successful mutations
   refresh the local status. Older status reads cannot undo logout.
6. Public catalogs and availability results never contain configured headers.
   Status collection does not call provider auth resolution or OAuth refresh.
   `allowNetwork` defaults to false and is forwarded to refresh callbacks. The
   instrumented callback network branch is never entered when disabled. This
   does not sandbox an arbitrary plugin that ignores `allowNetwork`.

## Qualification

All commands used an isolated mode-0700 HOME/TMP outside the worktree, offline
Cargo, and `/home/user/Projects/.targets/l2d-{178,195}`. Cargo ran in the foreground
under `timeout 270`; timed-out workspace compilation was rerun to completion.

- Rust 1.78.0 `RUSTFLAGS=-Dwarnings cargo check --workspace --all-targets`: pass.
- Rust 1.78.0 SDK `--no-default-features`, and
  `--no-default-features --features provider`: both pass.
- Rust 1.95.0 `cargo test --workspace`: **2440 passed, 0 failed, 26 ignored**
  (supplied baseline 2432/0/26; seven added regression test functions; no new
  ignores). The final run used
  `RUST_TEST_THREADS=6`. Earlier default-concurrency runs passed, but later reruns
  encountered unrelated existing timing races: scope-lease cancellation returned
  `service callback failed or cancelled` instead of `service request cancelled`
  (`crates/rpi-extensions/src/services/tests/scope_leases.rs:443`), and the loaded
  SDK delegation fixture reported `DLL handshake timed out: Elapsed(())`
  (`crates/rpi-cli/tests/provider_packages/delegation_v2.rs:394`). No tests were
  modified, filtered, or newly ignored to obtain the final full-workspace pass.
- `cargo fmt --check`: pass.
- All seven `docs/provider-parity/check-*.mjs`: pass in comparison mode.
- `scripts/provider-catalog-reference.mjs`: regenerated only into isolated TMP;
  byte comparison with `docs/provider-parity/catalog-reference.json` passes.
  No tracked generated evidence or regenerators were changed.
- Built the existing `plugin-stub` and explicitly ran
  `native_catalog_refresh_and_login_commit_through_loaded_library --ignored --exact`:
  **1 passed**, covering the real shared-library callback and print/JSON host
  paths, durable login, shutdown, and secret redaction. Its pre-existing ignore
  annotation was not changed.
- Dedicated catalog tests cover selected/all refresh, callback failure, removal,
  private-header isolation, overlapping global/provider refresh, cancellation,
  retained stale tickets, sequence races, and logout invalidation. Existing
  provider-runtime/delegation tests also ran in the workspace gate.

### Mutation checks (all reverted)

Each mutation compiled and was killed by an assertion (exit 101), not a build
failure. The restored implementation subsequently passed the workspace gate.

| Mutation | Killing regression | Observed failure |
| --- | --- | --- |
| Accept a retained stale publication by disabling its generation check | `retained_stale_publication_is_rejected_before_host_installation` | `unwrap_err()` received `Ok({"revision":2})` |
| Skip coordinator config recomposition | `config_recomposition_precedes_callback_and_removal_restores_base_catalog` | installed snapshot lacked `configured` |
| Remove provider latest-sequence guard | `availability_sequences_reject_global_provider_overlap_and_logout_race` | an older local result was accepted |

## Residual gaps / interfaces for integration

- Availability here deliberately means **local stored credential presence and
  expiry**, not Pi's complete provider `checkAuth`/`getAvailable` result. Anonymous
  providers, environment/config-only credentials, provider-specific checks, and
  provider auth-error snapshots still need a non-networking status/composition
  interface. Do not interpret `Authenticated` as an online token-validity check.
- Config recomposition updates model catalogs and URLs, not the privately sealed
  request-auth bindings initially constructed by `provider.rs`. Refreshing private
  configured keys/headers, or making newly introduced config-only providers
  executable, needs a host-owned provider/auth factory and atomic installation
  contract, including an unconfigured base catalog at initial construction
  (already-composed startup providers cannot reconstruct their original defaults
  after startup-only config is removed). Config-only entries are discovery-only,
  not executable providers. Do not route private headers through the public
  catalog to bridge this gap.
- Lane B owns session/native dispatch. Integration must bind the runtime to the
  fresh session generation and invalidate the outgoing `ExtensionSession` before
  admitting more callbacks; Lane D retains owner/snapshot validation before and
  after callback awaits. No B-owned file was edited.
- No actual Cursor DLL/worker, live OAuth service, cross-lane replacement race,
  or full differential availability trace was qualified here. Local fixtures
  and the existing loaded plugin are not substitutes for that matrix.
