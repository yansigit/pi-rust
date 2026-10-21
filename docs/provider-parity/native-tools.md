# Native tool invocation and recorded activity

This implements the generic H08 host boundary. It does not establish Cursor
adapter parity, MCP parity or a passing live Cursor test.

Use the versioned `native_request` slot through `provider::native::Client`.
Capture `Client::scoped()` inside a provider, event or tool callback before moving
work to another runtime or a worker. The captured context selects the owning
child harness, cancellation and request identity. An unavailable child fails;
its request cannot use the parent's tools. A request carrying another session's
identity is rejected.

## Tool inspection and execution

`{"port":"tools","action":"inspect"}` returns registered tool schemas with
`label`, `active` and `executionMode`. It includes disabled tools, marked inactive;
invocation resolves only against the active set. The child view is already
restricted to the tools granted to that child. Side questions have no tools.

```json
{
  "port": "tools",
  "action": "invoke",
  "call": {
    "type": "toolCall",
    "id": "bridge-call-1",
    "name": "read",
    "arguments": {"path": "README.md"}
  }
}
```

A call uses the same argument preparation, schema validation, before/after hooks,
image limits, tool context and cancellation as an agent-loop call. Hook argument
rewrites retain the existing trusted, post-validation semantics. The result is
`{"result": <AgentToolResult>, "isError": false}`; ordinary tool errors also use
this result shape with `isError: true`. Dispatch failures use the native error
response. Direct invocation emits tool progress but neither starts a provider
turn nor appends invented assistant/tool messages. The adapter owns the history
for its corresponding provider request.

Cancellation/drop suppresses late progress callbacks. The caller's cancellation
and the owning active run's cancellation reach the tool. Existing foreign-tool
lifetime tracking remains in effect until its callback returns.

## Recorded activity

A completed external operation is represented by the usual tool-call stream
blocks, with an additional `recorded` member on the call:

```json
{
  "type": "toolCall",
  "id": "external-call-1",
  "name": "bash",
  "arguments": {"command": "external operation already completed"},
  "recorded": {
    "version": 1,
    "isError": false,
    "result": {
      "content": [{"type": "text", "text": "Saved external output"}],
      "details": {},
      "terminate": true
    }
  }
}
```

Presence of `recorded` forbids executor dispatch. The host does not look up or
validate against a same-named executable tool. A missing result, explicit null,
malformed payload or unsupported recording version produces a replay error;
none falls through to execution. Ordinary before/after tool hooks still apply
to valid recordings, including blocking, argument edits and result changes.

The agent emits distinct `RecordedToolStart`/`RecordedToolEnd` events. Native
presentation uses the existing tool components; JSON/RPC and result-bearing
extension events identify these with `recorded: true`. Existing observer event
tags and ABI payload layouts remain unchanged. The surrounding tool-call stream
remains observable output to wrappers, and normal assistant/tool-result history
preserves IDs, result content, structured content, usage and termination.

`{"port":"tools","action":"present","call":...}` presents such a recorded
call outside the loop. This action requires the `recorded` member. It never adds
conversation entries implicitly. `invoke` also recognizes the marker and can
never execute a marked call.

Recorded results survive native stream encoding, persisted incremental frames
and ordinary message JSON. Recovery obtains an interrupted recording's result
from the saved call, without invoking a tool, even if a same-named tool and an
old start record both declare safe reexecution. New recorded start records use
`ToolReplay::Never`. Malformed recovered recordings produce an error result.

The Cursor adapter must attach the marker to every native replay call, including
an explicit null when its upstream display state has no result. Bridge calls
that actually need host execution use unmarked calls. This boundary is generic;
there are no Cursor tool-name or ID checks in the host.

## Compatibility and evidence

No C ABI fields or signatures change in this chunk. Old native libraries remain
loadable. Rust source users constructing `ToolCall`/tool-call frame literals must
supply `recorded: None`; exhaustive `AgentEvent` matches must handle the two new
presentation variants. Coordinate those source changes with the maintainer's
release process. Workspace versions are unchanged.

Regression coverage includes direct-versus-loop hook results, validation,
cancellation, late updates, child isolation, native loaded-library execution
counts, sequential/parallel recording history, malformed recordings, wire/frame
round trips and interrupted recovery. See `host-tools-results.json` for executed
checks and their limits. Adapter-specific tool metadata and complete retained
Cursor acceptance remain part of the Cursor implementation ledger.
