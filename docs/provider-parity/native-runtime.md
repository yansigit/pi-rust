# Native provider runtime ports

Status: host implementation passes the recorded [regression checks](host-runtime-results.json);
this does not qualify any of the three provider packages as complete.

`PluginApi.native_request` and `register_provider_runtime` follow the composition
slots. Both use the separately sized `AuthRequestControl`; existing controls and
all existing vtable offsets remain unchanged. The SDK's optional provider feature
exposes `provider::native::Client`, which rejects hosts lacking the runtime port.
Host results use the host's string deallocator.

The private request envelope is `{version:1,port,action,...}` and returns
`{version:1,result}` or `{version:1,error}`. Ownership comes from the host's opaque
plugin/invocation bridge. JSON cannot choose another owner. A request retains the
runtime it invokes; invalidated generations fail explicitly. Credentials and login
continuations never enter ordinary plugin events.

`{version:1,port:"capabilities",action:"inspect"}` returns the supported native
data-contract capabilities. `provider::native::Client::require_capabilities`
rejects an unsupported probe or missing names explicitly. New compatibility
providers must require `assistant.durationMs` and `json.exactNumbers` before
registration; an older host must not silently drop timing or round usage values.
This probe uses the existing private port and leaves all C ABI layouts unchanged.

The private `context.inspect` capability adds `{port:"context",action:"inspect"}`.
It returns the host-captured `owner` (including generation and child status) and
operation `identity`, or null when there is no active operation. Supplied owner
fields are ignored. Captured context leases retain the invocation's scope;
cancelled or invalidated invocations are refused. Like capability discovery,
this query is safe during synchronous registration and schedules no host work.
It lets inherited handlers avoid mutating their parent's provider state.

## Catalog operations

The `providers` port supports `inspect`, `publish`, `register`, `unregister`, and
`refresh`. `inspect` includes offline discovery records as well as executable
providers. Mutations return the committed catalog revision. Only a provider's
owning registration can publish or unregister it; child invocations cannot mutate
a parent's registration. A native implementation can register additional identities
using its own `implementationId`, including virtual providers.

Publication prepares a complete candidate catalog and rebuilds wrapper chains from
unwrapped providers before installing it. Failed installation leaves the previous
catalog and revision intact. Readers wait for installation to commit. Outstanding
invocations retain the previous chain; subsequent requests use the current chain.
The session runtime stays alive even if its last provider is unregistered.

An optional `expectedRevision` rejects a stale mutation. Catalog refresh captures
its starting revision, awaits the owning provider outside the publication lock,
and refuses to overwrite a newer catalog. Public descriptors cannot carry headers
or change a provider's registered protocol. Credential-bearing execution bindings
retain their private endpoint/header configuration across empty catalogs and
republishing. Existing source-level `ProviderManifest` initializers must include
`replace_catalog`; its JSON default is false. No workspace version has been bumped.

## Provider callbacks

A runtime manifest declares `{version:1,id,actions}` with any of `preflight`,
`refreshCatalog`, `authMethods`, `credentialStore`, `initialize`, and `requestAuth`. Each callback receives
`{version:1,providerId,action,input}` and returns `{version:1,result}` or a public
`{version:1,error}`. The callback and context remain pinned until foreign code
returns, including after cancellation.

`preflight` is a side-effect-free admission check supplied only model identity and
protocol. It runs before credential reads or network activity. `refreshCatalog`
receives `allowNetwork` (default false) and `force` (default false), and returns
`{models}`. `authMethods` returns the provider's currently available auth methods.
Catalog replacement is explicit via `ProviderManifest.replaceCatalog`.

With `auth.dynamicMethods`, both the native auth port and standalone `rpi auth`
discover current methods before login; an empty list disables new login flows.
The host validates unique nonblank IDs/names and kinds supported by the registered
auth manifest. Dynamic methods cannot expand its credential capabilities. Existing
registrations without this callback retain their static methods. The callback is
private, cancellation-aware and generation-scoped.

Hosts advertising `providers.initialize` await each owning provider's `initialize`
callback, in registration order, before model resolution or standalone `rpi auth`
reads credentials. The input contains the host-selected `agentDir`. The private
credential port is already bound; tools and session operations require the later
harness startup. Initialization does not publish an executable catalog. A callback
error aborts preparation and drains admitted credential writes. Providers whose
upstream behavior keeps recovery menus available must retain their refusal state
and acknowledge initialization, then reject authentication and streaming from that
state until the user completes recovery.

Reload initializes a fresh generation before resolving its credentials. Runtime
bindings are weak and keyed by generation, so this preparation cannot redirect
calls from the existing generation. Failed preparation leaves its binding intact.
The source-level `prepare_extensions` function is now async; callers await it.
This source change requires maintainer release coordination, without a contributor
version bump or changes to existing C ABI layouts.

## Authentication operations

Hosts advertising `auth.providerRequest` support an owning provider's private
`requestAuth` callback before ambient credential resolution. Its input contains
`modelId`, `api`, the request's `apiKey`, and merged request/environment `env`.
Returning `null` delegates to the existing credential resolver. Returning a
`RequestAuth` object supplies the complete request authentication without reading
or refreshing the dormant credential store. Errors refuse the request. Headers,
endpoint and environment changes apply only to that request; they never enter
public model metadata or observer events. Explicit replacement-account attempts
bypass this ambient callback and retain their independently resolved credentials.

A provider opting into this callback can start with a malformed credential file
so its recovery controls remain available. Other providers retain store errors
and cannot run from that session's catalog while the file is unreadable. Unrelated
credential commands are not evaluated on this path. Cancellation and shutdown
retain the callback and library until foreign work returns. This capability uses
the existing sized runtime port and adds no C ABI fields.

The `auth` port supports `methods`, `login`, `continue`, `cancelLogin`, `status`,
`refresh`, and `logout`. The default destination is the host credential store.
An explicit `storeId` and `accountId` select a store owned by the calling plugin.
Host credentials and pooled accounts remain separate.

Login binds its destination before presenting an interaction. The caller receives
an opaque `loginId` and presentation data; the provider continuation stays private
inside the host. Continuing consumes that ticket, verifies its owner, and issues a
new ticket when another interaction is needed. Success is returned only after the
credential-store transaction acknowledges persistence. Status exposes credential
kind and millisecond expiry, never tokens. Legacy second-based stores convert at
the boundary. Refresh uses the existing cancellation-independent refresh/persist
runtime; pooled refresh cannot fall back to ambient credentials.

The explicit native management/pooling path also adapts legacy OAuth `begin`,
`exchange`, `refresh`, and `revoke` callbacks, preserving their presentation and
second-based credential contract. Registered protocol providers without a custom
auth implementation receive a host API-key method. Unknown providers are refused.
This does not opt legacy providers into new ambient fallback behavior. Full UI
interaction parity remains a separate pending ledger requirement.

Print and JSON modes now deliver session start/shutdown once, as interactive/RPC
modes already did, including the shutdown authentication drain. Full event identity
and result-bearing lifecycle work remains pending.

## Private credential-file transactions

Hosts advertising `auth.credentialStoreLock` accept
`{port:"auth",action:"credentialStore",providerId,input,lockTimeoutMs?}`.
The registered provider must own the calling session and plugin; child callers,
foreign providers, explicit paths, and pooled-store redirection are refused.
The optional timeout is 0–30000 milliseconds (default 30000); zero still attempts
an uncontended acquisition once.

The host acquires the same file lock used by native login, logout and refresh,
then invokes the provider's `credentialStore` runtime callback with
`{authPath,expiryUnit:"seconds",input}`. The host selects this private absolute
path. The callback owns raw-format validation, byte-preserving slot updates,
backup/recovery ordering, and awaited file/directory durability. It must leave
other providers unchanged and must not recursively request a host credential
operation while holding this lock. A malformed file is passed to the callback
without parsing or rewriting, so the provider can preserve its redacted errors.
No credential or path is broadcast through observer events or model metadata.

Admission and lock waits are cancellable. Once the callback begins, cancellation
can stop waiting but the host retains the lock, callback and library through
completion. Authentication shutdown drains this work before plugin teardown.
The provider must not acknowledge queued writes as committed. A failed callback
may have already published a file; its durable recovery records remain authoritative.
This extends the private JSON contract without changing C vtable offsets,
control structures, or workspace versions. Packages must negotiate the capability.
