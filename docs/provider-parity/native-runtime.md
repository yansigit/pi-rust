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
`refreshCatalog`, and `authMethods`. Each callback receives
`{version:1,providerId,action,input}` and returns `{version:1,result}` or a public
`{version:1,error}`. The callback and context remain pinned until foreign code
returns, including after cancellation.

`preflight` is a side-effect-free admission check supplied only model identity and
protocol. It runs before credential reads or network activity. `refreshCatalog`
receives `allowNetwork` (default false) and `force` (default false), and returns
`{models}`. `authMethods` returns the provider's currently available auth methods.
Catalog replacement is explicit via `ProviderManifest.replaceCatalog`.

## Authentication operations

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
