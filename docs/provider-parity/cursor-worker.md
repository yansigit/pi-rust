# Cursor retained adapter and worker transport

Status: C01 transport foundation implemented. Authoritative session snapshots,
awaited adapter writes and Rust journal IO have since been added; native provider
dispatch and packaged installation remain in progress. This is not a claim of
Cursor inference or package parity.

`rpi-package/packages/rpi-cursor-sdk/worker` retains all 461 tracked files from
`fitchmultz/pi-cursor-sdk@f337dc4bb7cea936c52e69a7fe08e7e376c8467b`. Each original
file's baseline Git blob hash is recorded in `upstream.json`. The initial transport
chunk retained those files unchanged; subsequent commits patch the host boundaries
and affected tests. The npm lockfile remains unchanged. New worker code and tests
live under `src/rpi` and `test/rpi`. The SDK remains an external exact
`@cursor/sdk@1.0.37` dependency, following the [distribution decision](cursor-sdk-distribution.md).

## Connection and ownership

The Rust owner starts Node with an absolute worker asset path and a private
stdin pipe. Bootstrap supplies a random 256-bit connection secret, a random
generation and an ephemeral loopback port. Neither argv nor environment carries
the secret. The dedicated connection authenticates before accepting operations;
stdout/stderr never carry protocol messages. Bootstrap has a ten-second deadline,
and malformed, stale-generation or unauthenticated traffic fails closed.

Frames contain version 1, generation, request identity, kind and a kind-specific
body. `h` identities belong to host calls and `w` identities to worker calls.
IDs are monotonic within a generation. Host ID allocation and enqueue are
serialized, including concurrent Rust callers. Worker-to-host calls retain their
admitting request identity. This low-level transport does not itself grant access
to tools, credentials or session storage; those require the owning native ports.

Node uses a separate transport thread. Synchronous SDK callbacks may wait on a
shared stream budget while that thread continues handling credits, replies and
cancellation. Per request, ordinary events have the fork's 64-item / 4 MiB
accounted payload limits. One oversized event exclusively consumes the byte
budget. Frames larger than 64 MiB fail explicitly. A generation admits at most
16 requests and 16 concurrent host callbacks. Control queues have independent
count/byte bounds and reserved progress for cancellation and completion.

Completion records the final event sequence. It may travel ahead of queued
events, but Rust exposes the result only after all preceding events have arrived.
Credits are returned after consumption, without a cancellable gap between
removing an event and acknowledging it. Dropped consumers cancel their request
and release queued credits; result-only consumption drains intermediate events.
These are queued-payload limits, not bounds on SDK state, total process memory,
one in-flight frame, final messages or consumer-held values.

The host reader stays active while provider work or nested host callbacks run.
Cancellation reaches both the producer's shared budget and admitted host work.
Repeated and late cancellation is idempotent. A worker crash preserves already
received partial events and returns an uncertain-completion error. Final teardown
cancels admission, waits for admitted host callbacks and explicitly reaps Node.
The adapter's MCP/SDK/journal shutdown must be awaited before that final teardown;
the worker now invokes and drains that lifecycle, while native-library ownership
and shutdown wiring remain pending.

## Verification and limits

[cursor-worker-results.json](cursor-worker-results.json) records the executed
initial transport checks and log hashes. Those Rust/Node checks use a fixture
worker. The retained 53 Pi native-loader checks exercise the original Pi
boundary, not native rpi loading. Windows cross-compilation does not establish
Windows execution; macOS execution is also pending.

The first retained-suite attempt omitted the previously established baseline
environment and six upstream CI/automation files. Those setup mistakes were
corrected without changing upstream source. A subsequent run passed 1,636 tests
and failed the artifact-race fixture because a safe file was observed under the
fixture's temporary `nested.safe` rename. Its checks for leaked outside data
passed. The unchanged test passed targeted checks in both the original repository
and retained fork. The subsequent full run passed 1,637 tests with two upstream
skips. The extra new control-order test is verified separately. The earlier
failure remains recorded as an upstream fixture timing limitation.

The package's `SESSION-PORT.md`, `tests/session-evidence.json` and
`tests/journal-evidence.json` describe the subsequent session/journal work. Native
cleanup uses committed durable receipts; cloud and usage writes await Rust IO.
Journal tests include actual IPC, concurrent processes, failed syncs, torn tails
and unchanged pinned Node writer comparisons. The retained suite now passes 1,663
tests with two upstream skips, plus 53 Pi loader tests at that stage. These checks
remain partial port evidence.

The [packaged dispatcher evidence](../../../rpi-package/packages/rpi-cursor-sdk/tests/dispatcher-evidence.json)
now executes the retained factory through `runtime/main.mjs` with scripted host
ports and integrated Pi 1.3.0. It verifies startup catalog reentry, command commit
acknowledgements, API-key login, both stream entries and origin release. Its latest
retained suite passes 1,698 tests with two existing skips; the factory/process
checks pass six, Node transport checks seven, and Rust worker checks eleven.
The [host context boundary](host-native-context-results.json) supplies owned paths,
flags, mode/trust, execution metadata and independent child status. Native DLL
registration, session/journal routing and complete event/tool integration remain
required; these counts do not qualify local or cloud SDK behavior.

Still required: installed native-library asset resolution and loading; actual
adapter registration and request ownership; background session leases and
history-authorized inherited journal reads; stable request headers/signals; tool execution/replay counts;
all adapter feature groups; imports; SDK resource shutdown; packaged execution on
Linux, macOS and Windows; and controlled live Cursor acceptance. No Cursor live
usage or cloud mutation was performed for this transport work.
