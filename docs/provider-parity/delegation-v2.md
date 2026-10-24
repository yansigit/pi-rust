# Secure delegation v2 host protocol

The v1 continuation descriptor remains `{version:1,id,generation}`. Both operations
reuse the existing `provider_continuation` and `delegate_provider_stream` callbacks;
there are no new C slots, runtime action IDs, or enum discriminants.

Continuation envelopes accept these actions:

- v1 `capabilities`: `{version:1,delegationVersions:[1,2],opaqueTargets:true,ambientReceipts:true,epochRevocation:true}`.
- v2 `resolveTarget {provider,model}`: `{version:2,target,model}` (public metadata only).
- v2 `auth.resolveAmbient {provider,model,target}`: `{version:2,receipt}`.
- v2 `resolveAuth {target,storeId,accountId,options}`: `{version:2,receipt}`.
- v2 `releaseTarget {target}` and `releaseAuth {receipt}`: `{version:2}`.

Every envelope also contains `continuation`. Old hosts may reject `capabilities`;
SDK users must not downgrade cross-provider requests in response.

Delegated streams accept `{version:2,continuation,target,authReceipt?,context?,options?}`.
A model descriptor is not authority. The target selects the full composed chain.
Targets and receipts are bearer capabilities stored in separate, bounded tables
owned by a single live continuation (64 targets and 64 receipts). Releasing a
target removes its dependent receipts. No capability contains serialized secrets.

Host-only scope/epoch/provider-instance/wrapper-instance IDs are distinct types.
Ancestry is immutable per invocation branch, not a concurrent-active registry.
Provider ID or generation recurrence fails with `provider delegation cycle`;
wrapper recurrence fails with `provider wrapper instance reentry`. Independent
calls and retries from the original ancestry remain legal. Scope-wide committed
publication revokes old epochs; failed publication leaves the current epoch valid.

Stable security errors:

- `delegation target belongs to a different continuation`
- `delegation epoch revoked`
- `provider delegation cycle`
- `provider wrapper instance reentry`
- `auth receipt target mismatch`
- `delegation target capacity exhausted`
- `auth receipt capacity exhausted`

These are host errors, not TypeScript parity claims. The two lookup errors retain
`multiprovider/src/virtual.ts:165,169` wording; verify against pinned sources when
changing them. This protocol document is not acceptance evidence: runtime/SDK and
loaded-library coverage must be verified independently.
