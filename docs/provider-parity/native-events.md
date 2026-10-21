# Native event contract (version 1)

This is host infrastructure for H06, not completed extension parity. The trailing
`PluginApi.register_event_result_handler` slot accepts an event tag, callback,
plugin deallocator and plugin-owned context. A package requiring awaited results
must reject a host with no slot. Existing observer registration and field offsets
are unchanged. `SessionCompactFailed` is appended at discriminant 37.

Callbacks borrow an independently sized `AuthRequestControl` and receive
`{"version":1,"tag":15,"event":{...},"identity":{...}}`. They return
`{"version":1,"result":{...}}`; missing `result` means undefined, while explicit
`null` is a value. A callback may return an explicit `error`. The host does not
log callback payloads or authentication material. Output is reclaimed through the
registered deallocator even after cancellation. Each outstanding foreign call
retains its callback context, library and invocation binding until it returns.

Observers and result callbacks share priority and registration order, including
merged registries. Later listeners see prior modifications. Context handlers
replace messages without replacing the system prompt or tools. Before-operation
handlers can replace the prompt and return custom messages; the harness commits
those messages before the first provider request. Tool approvals can block;
errors fail closed. Tool results chain content, details, structured content,
error state and usage. Replacing content removes stale structured content unless
an explicit replacement accompanies it. Message-end changes run before settlement
and reach both persisted history and the returned assistant message.

Prompt operations use the session's admitted run ID. A turn gets an explicit ID,
and each provider invocation gets a request ID. Repeated prompt timestamps do not
identify operations. The identity is private native request metadata and cannot
be patched through a delegated continuation. Parallel tools inherit their
operation scope. Summary requests retain a distinct compaction purpose and
identity; before/complete/failure lifecycle callbacks share it. Success events
carry the committed entry, and failure callbacks still run when the request was
cancelled. Session startup and shutdown await result listeners. Reload rebuilds
tool subscriptions and waits for native shutdown callbacks.

The legacy redacted before-provider observations remain separate from private
provider request ownership. Cursor's stable header-object receipt and abort-signal
mapping must be supplied by its owning worker port; those worker boundaries are
not implemented yet. Tree, session-info and model-selection result dispatch still
need to be connected to their authoritative session/UI operations. Synchronous
progress callbacks retain the observer path. Complete differential event traces,
platform loading and package acceptance remain pending.

Rust source changes require maintainer release coordination: ProviderHooks has
awaited operation/request/lifecycle methods, AgentEmitter has preparation and
prepared-publication methods, stream options carry explicit identity, and tool
results preserve structured content. C ABI changes are trailing slots and an
appended tag; workspace versions and declared Rust support are unchanged.
