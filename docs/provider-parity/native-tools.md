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

## Owned wake callbacks

The optional trailing `register_owned_tool_with_wake` slot adds a registration
context to tool `execute`. Its other callbacks and handle lifecycle are identical
to `register_tool_with_wake`. Both use the same host wake driver, captured
invocation scope, cancellation, result ownership and library keepalive. Plugins
own the registration context and release it from their lifecycle shutdown only
after their work has drained. They must support concurrent calls from different
handles. New packages that need owned contexts must reject hosts missing the slot.

All earlier C offsets/signatures and unsized controls remain unchanged. Rust
source users constructing `PluginApi` literals must supply the additional optional
field in the maintainer's coordinated SDK release; workspace versions are unchanged.
The SDK checks every earlier short-table boundary. A registration/driver regression
uses identical tool-call IDs in two concurrent registrations, cancels one, and
verifies independent completion and exactly one cleanup per handle. Results are
recorded in `host-owned-tools-results.json`; complete Cursor tool integration and
recorded execution counts remain separate acceptance work.

## Tool metadata

`tools.metadata` extends the existing `declare` JSON contract without changing
any C structure. After registering its tools, a plugin can declare:

```json
{"toolMetadata":{"question":{"label":"Question","executionMode":"sequential","promptSnippet":"Ask once","promptGuidelines":["Wait for the answer"]}}}
```

Names must already belong to the registration registry; unknown names, fields,
modes and malformed guidance reject the declaration before any metadata changes.
Metadata survives snapshots and first-registration-wins merges. Host adapters use
the label and execution mode, and tool inspection includes guidance and the host's
existing `direct` exposure. Source information remains host-stamped. This does
not add Pi's unrelated deferred/codemode exposure policies.

Default CLI prompts include active tools' snippets and deduplicated guidelines;
explicit/custom prompt sources retain control of their own text. Before-operation
hooks receive all registered `toolSnippets` and `toolGuidelines` alongside the
selected names. Image and child tool wrappers preserve these methods. Existing
plugins default to their tool name, parallel execution and no extra guidance.
Rust API additions require coordinated release review; versions are unchanged.
`host-tool-metadata-results.json` records the checks and their limits.

## Activation and registration filters

`tools.exactActivation` makes an empty active selection disable all tools for
inspection, requests, execution and inherited child permissions. The initial
empty constructor default still selects all registered tools, expanded once at
construction. Awaited before-operation hooks can update the live selection before
the first request is built. The native activation port ignores unknown names and
duplicates; changing a child's selection leaves its parent unchanged.

`tools.registrationPolicy` adds `toolRegistration` to native context snapshots:
`{"disabled":false,"allow":null,"deny":[]}`. These are the CLI's actual
registration filters, captured at startup/reload and inherited by children.
Worker-owned presentation declarations apply the same filters without becoming
executable host tools. A native package that relies on either capability must
require it. No C ABI layouts or workspace versions change.

`tools.selectionState` returns a committed `{tools,selection:{revision,state}}`
receipt from `tools.setActive`; native context snapshots include `toolSelection`
with the same selection object. Revisions are unsigned decimal strings to avoid
JavaScript integer rounding. Optional `state` is bounded to 16 KiB of JSON and
holds non-secret presentation metadata only; it never registers or enables an
executable tool. Filtering, selection and state commit under the same harness
lock. All legacy selection setters and registry replacement clear this state,
including a repeated selection with identical names. Child harnesses start with
fresh selection state. Receipts remain immutable after subsequent writes. Tool
requests, guidance and inherited selections retain the selected order, ignoring
unknown and duplicate names as Pi 1.3.0 does.

`tools.boundedProgress` uses the provider bridge's shared count/byte queue for both
legacy and wake-capable tool progress: 64 queued items and 4 MiB of incoming JSON
weight. One oversized item occupies the byte budget exclusively. This accounts
for queued payload, not total allocator usage or the producer's current frame.
Callbacks wait on capacity outside the async runtime; wake-driver pending progress
awaits capacity. Terminal delivery remains separate. Cancellation releases blocked producers
and retains accepted partials for terminal draining; consumer drop releases the
queued items. Both paths keep the invocation/library alive through the sole
destroy call.
