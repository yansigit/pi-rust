# Scope-service grants (`services.scopeLease.v1`)

M09 adds time-bounded, host-private service authority over the existing version-1
`native_request` JSON transport. No ABI layout, function slot or action ID changes.
Tool names confer no authority. `scope_lifetime()` remains inspection-only.

## Protocol

Negotiate `services.scopeLease.v1` in the host's `capabilities` response, then send:

```json
{"version":1,"port":"serviceLeases","action":"acquire","service":"pi-fabric","serviceVersion":1,"methods":["isIdle","compact"],"capability":"services.scopeLease.v1"}
```

The result is `{"lease":"opaque-host-id"}`. Calls and release:

```json
{"version":1,"port":"serviceLeases","action":"call","lease":"opaque-host-id","method":"compact","input":{}}
{"version":1,"port":"serviceLeases","action":"release","lease":"opaque-host-id"}
```

Call returns the underlying service result; release returns `null`. The usual
`{"version":1,"result":...}` / `{"version":1,"error":"..."}` envelope applies.
Release is synchronous at the host boundary, allowing best-effort RAII teardown
without a worker outliving its plugin library.

Acquire requires a live **provider** invocation, not a command, service callback,
inspection lease or registration handle. Authority is captured by the invocation
bridge before entering the DLL: owner registration/generation, weak selected
main/child execution, epoch revocation token, and operation identity. Methods
must be a nonempty subset of the exact service registration's manifest. Grants
never become ActionBridge handles, and call/release resolve the owner registration
rather than falling back to an ambient invocation. They confer no publication,
registration, other port or unrestricted runtime-action authority.

A service callback has its existing service authority; host dispatch restores the
captured selected execution before constructing its invocation bridge. Fabric's
`isIdle` / `compact` implementation must forward to host `IsIdle` / `Compact`.
There is no built-in `pi-fabric` service in the host. The read-only consumer in
`packages/rpi-multiprovider/src/native/fabric.rs` currently uses ordinary service
requests for both methods; it must adopt the lease calls below. No package files
are changed by this host lane.

## Lifetime, limits and errors

The host registry serializes acquire and removal. Callback return **does not**
revoke a grant. Release, selected scope CLOSING, epoch revocation, caller unload,
session invalidation, or target service unload/replacement revoke it. Closing
entries are reclaimed (including otherwise unused grants) by a bounded monitor.
Each call has a child cancellation token and is cancelled on caller cancellation
or grant revocation. Calls do not keep a closed child alive or select main instead.

Limits: 1024 live grants/registry, 32/owner, 8 simultaneous calls/grant, 256 calls
process-wide; no waiting queue. Admission is retained until the foreign callback
and output deallocation finish, even if the caller has already returned on
cancellation. IDs are unguessable and every use checks owner.

Stable failures:

- `scope service lease requires a live provider invocation`
- `scope service lease capability not negotiated`
- `scope service lease capacity exhausted`
- `unknown scope service lease`
- `scope service lease owner mismatch`
- `scope service lease revoked`
- `scope service lease method not permitted`
- `unsupported scope service lease action`
- `service request cancelled`
- `selected execution scope is busy` (selected compaction)

Acquisition/parsing also preserves `missing service service`, `missing service lease`,
`invalid service version`, and `service unavailable`. Service dispatch preserves
service-owner, callback and response errors, including arbitrary service-provided
error strings. Cancellation may surface through the outer transport as
`native runtime request cancelled` instead of `service request cancelled`.
Removed grants can report `unknown scope service lease` rather than retaining
unbounded revocation tombstones.

SDK-specific errors include `invalid scope service lease response` and
`host is missing required native capabilities: services.scopeLease.v1` (old-host
capability refusal). Capability discovery transport/parsing failures are wrapped
as `native capability discovery failed; upgrade rpi: {error}` or reported as
`invalid native capability response`.

## Idle-only compaction

`IsIdle` is an observation, never a reservation. `Compact` selects
`for_current_execution()`, then the harness atomically installs its compaction
ActiveRun under the same mutex as prompts. If another run won admission it returns
`selected execution scope is busy`. It never aborts a newer retry. A missing/closed
child binding fails closed, never falls back to the main transcript. Existing
compaction signals and per-call cancellation still apply after admission.

## SDK adoption

Capture `Client::scoped()` on the provider callback thread, then acquire **before**
returning the failed stream. Keep only the narrow lease in the delayed worker:

```rust,ignore
let invocation = Client::new(api)?.scoped()?;
let lease = invocation.scope_service_lease(
    "pi-fabric", 1, &["isIdle", "compact"], invocation_signal.clone(),
).await?;
// The failed stream may now return. Use a worker/shutdown signal, NOT the
// completed invocation signal, for delayed calls.
let idle = lease.call("isIdle", json!({}), worker_signal.clone()).await?;
if idle["isIdle"] == true {
    // May still return busy if a newer run won admission.
    lease.call("compact", json!({}), worker_signal.clone()).await?;
}
lease.release().await?;
```

`ScopeServiceLease` is Clone/RAII: clones share release state; final Drop sends a
best-effort release. Explicit async release is preferable. Calls use a dedicated
raw-envelope transport and do not re-enter `Client::scoped()` after callback expiry.
Old hosts fail closed at capability negotiation. Retain ordinary scoped clients
only for invocation-time discovery/receipts; a lease cannot send UI notifications.
The consumer should retain its existing 50ms idle polling, 15s deadline and 30s
compaction debounce; quota/auth rotation remains inline, not deferred compaction.

See [verification and mutation results](scope-service-lease-verification.md) for
loaded-library transcript proofs, gate totals and the two restored dispatch boundaries.
