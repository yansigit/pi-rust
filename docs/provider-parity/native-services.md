# Native services and invocation contexts

Status: H07 host infrastructure. Multiprovider's account and Fabric contracts and
its differential fixture extensions remain pending; these APIs alone do not
establish package parity.

Two trailing `PluginApi` fields are appended without changing any earlier offset:
`register_service` and `native_context`. Existing compiled plugins and short
vtables retain their previous layout. New packages must require these slots.
The Rust `RegisteredProvider` and `RegisteredWrapper` records now carry their
owning action bridge; downstream Rust initializers need the additional field.
Coordinate that source change with the maintainer's release; versions are unchanged.

## Registration and private calls

`register_service` has the same callback/deallocator signature as native auth.
A manifest is `{version:1,id,serviceVersion,methods,topics}`. The wire version is
separate from the service's version. Names must be nonempty ASCII identifiers;
duplicate methods/topics and unknown manifest fields are refused. The first
registration of an `(id,serviceVersion)` pair wins. Registration becomes callable
only when the successful library registrations are assembled into their session.
Failed registration never publishes a callable service.

Use `native_request` with `port:"services"` and these actions:

| Action | Other fields | Result |
| --- | --- | --- |
| `list` | none | Registered manifests |
| `call` | `service`, `serviceVersion`, `method`, `input` | Private callback result |
| `subscribe` | `service`, `serviceVersion`, `topic` | Opaque `subscription` |
| `next` | `subscription` | Next `{sequence,topic,value}`; waits if empty |
| `unsubscribe` | `subscription` | `null`; wakes a waiting reader |
| `publish` | `service`, `serviceVersion`, `topic`, `value` | Committed `sequence` |

The method callback receives `{version:1,service,serviceVersion,method,input,caller,
identity}` and returns `{version:1,result}` or `{version:1,error}`. The host supplies
`caller` and the frozen request identity. A request cannot select its own owner by
adding JSON fields. Results, including credential-to-request resolution, are not
sent to ordinary extension observers or notification subscribers. Publishers must
keep notification values non-secret; the generic registry cannot determine whether
an arbitrary application value contains credentials.

Calls retain the callback's library and context until foreign code returns.
Cancellation or unload can stop waiting earlier, but cannot unload an executing
callback. A service callback may call another native port without holding the
registry lock. Publication is restricted to the service's registration owner.
A child can report through its own plugin's service without acquiring ownership
of the parent's registration. Child termination does not unregister that service.

## Bounded subscriptions

Each subscription holds at most 64 events and 4 MiB of serialized notification
envelopes. Oversized notifications fail explicitly. A session permits at most
1,024 subscriptions and a registration/scope class at most 128. Notification
sequence numbers increase only for accepted publications.

Publication waits until every current matching subscriber can accept the event,
then enqueues it atomically. Cancellation during backpressure does not partially
deliver an event. Unsubscribe, caller cancellation and session closure progress
independently of queued data. Only one `next` may wait per subscription. Consumers
must cancel or unsubscribe when stopping. No service callback runs on a notification
reader's behalf. Subscribers that need awaited application handling expose a
private method for that acknowledgement.

Subscription handles are bound to registration, generation and the actual child
execution scope. Sibling children cannot consume each other's handles. A child
scope's end wakes blocked publishers and discards that child's subscriptions.
Reload/session shutdown closes the old registry and wakes all its waiters.
The state contains weak library references, avoiding a library/bridge/registry cycle.

## Moving calls to foreign workers

`native_context(0,handle)` acquires a new opaque lease on the current invocation;
`native_context(1,lease)` releases it. Handles are never reused. Release cannot
remove a registration's ordinary runtime handle, and releasing an old lease cannot
revoke a later lease. Host shutdown retires the owner's outstanding leases.
The native entry point resolves the lease before dispatch and restores the frozen
request identity, cancellation and child execution scope on the host runtime.

The SDK's `provider::native::Client::scoped()` captures this context synchronously.
Call it on the foreign callback thread before moving work to a Node/SDK worker or
another runtime. `request()` also captures before its own blocking dispatch.
Keep a scoped client for the invocation, not as the session's default client.
A new session-level client can perform session management outside an invocation;
it must not be used to erase a child or provider invocation's restrictions.

Both native provider entries and nested wrapper/delegation paths establish and
retain this invocation context. This is required for provider-originated tool
bridges and account services as well as event callbacks.
