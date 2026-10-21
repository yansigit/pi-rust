# Native provider composition

The SDK appends credential-store, wrapper registration, delegated streaming and
private continuation slots. Earlier offsets and control structures are unchanged.
The `provider::composition::register` helper reports missing required capabilities
and owns one `ProviderContext` through lifecycle shutdown.

A wrapper manifest is `{version:1,id,providers:[providerIds]}`. An omitted/null
provider list matches all providers. Registrations are unique within their owning
plugin and session; first registration wins for a duplicate wrapper ID. The first
registered matching wrapper runs first. Fresh compositions start from unwrapped
providers, never a previously wrapped chain.

Each invocation receives `__rpiContinuation:{version:1,id,generation}`. This opaque
capability captures the next provider, original request and owning session. Calls
cannot change provider/model/API identity or look the wrapper up recursively.
Expired, cancelled and stale-generation capabilities fail. Every outstanding
invocation retains the wrapper, next provider, callback contexts and libraries.
Child requests share providers without acquiring their registration ownership.

The private continuation port accepts:

- `resolveAuth`: `storeId`, `accountId`, optional `options` (`apiKey`, `env`,
  `minimumValidityMs`). The store must belong to the wrapper. The registered
  provider resolves the account and returns an opaque receipt.
- `releaseAuth`: release a receipt from this invocation. Receipts otherwise live
  until the invocation ends and cannot be used by another continuation.

Both requests include `version:1`, `continuation`, and `action`. Native credential
stores register `{version:1,id}` with a request callback and a leaf lock-release
callback. Requests include `providerId`, `accountId`, `storeId`, `action`, `input`:

| Action | Successful response |
| --- | --- |
| `read` | `{version:1,kind:"credential",payload:credentialOrNull}` |
| `lock` | `{version:1,kind:"locked",payload:{lockId,credential:credentialOrNull}}` |
| `replace` | `{version:1,kind:"success",payload:{}}`, after durable commit |
| `remove` | `{version:1,kind:"success",payload:{}}`, after durable commit |

Replace/remove inputs include `lockId`; replace also includes `credential`.
Credential expiry is milliseconds. The store serializes access across processes.
A cancelled lock waiter consumes and releases a late acquisition. Once refresh
starts, cancellation cannot discard its rotated credential. Existing transactions
may commit after registry invalidation; new acquisitions cannot begin.

Delegated stream requests include `version:1`, `continuation`, optional
`authReceipt`, `model`, `context` and `options`. Credential replacement comes only
from a receipt. Raw authentication payloads, private mode overrides and unknown
options are rejected. The execution binding restores the trusted canonical
endpoint and applies the provider-derived credentials. Missing pooled credentials
never fall back to ambient lookup, including providers with external ADC lookup.

Delegation uses the existing 64-frame/4-MiB accounting, with an oversized frame
occupying the payload budget exclusively. Completion has a separate reserved
channel. Cancellation wakes blocked producers and preserves accepted partial
output. Private response frames precede ordinary events; wrappers can inspect
them and forward them to their caller. They never become agent events. Retaining
a response callback cannot prevent terminal completion.

**Allocation direction matters:** delegated frames, final results and private
continuation results are host allocations, freed with `PluginApi.free_string`.
Wrappers copy forwarded frames into their own allocations before handing them
to the host. The SDK adapter handles both directions and shares the event codec
with the host, including complete partial snapshots and compact text deltas.

This is an implemented composition foundation, not H04/H05 or package completion.
Dynamic catalog publication, generic runtime/auth discovery, both Pi stream entry
adapters and the full package differential suites remain separate requirements.
