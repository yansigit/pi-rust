# Streaming message copy measurements (2026-10-24)

The host now combines adjacent, already-queued pure text/thinking deltas before
constructing an immutable assistant-message snapshot. Each batch contains at
most 4 KiB of delta text; an individual oversized delta is delivered unchanged.
There is no fill timer: a lone available delta is forwarded immediately.
Full snapshots, metadata, unknown fields, tool calls and content-block changes
are boundaries. Consumers must not assume one host delta per plugin callback.
The concatenated delta text and ordered structural events are preserved.

The public event types, C ABI and producer-side deallocation remain unchanged.
`OwnedString::as_str_lossy` borrows valid UTF-8 during JSON parsing and allocates
only for the existing malformed-UTF-8 replacement behavior. Parsed messages own
their data before the plugin allocation is released. The mapper moves delta
strings out of JSON values, and the agent loop moves streaming events into its
broadcast envelope instead of cloning their delta strings again.

## Method

Windows x86-64, release profile, the existing offline `provider-performance`
fixture and allocation counters. The pre-change executable was preserved before
rebuilding; the fixture itself was not changed. Before/after executables were
alternated in three fresh-process runs per workload, reversing their order in
the middle repetition. Both include the uncommitted runtime extraction and
release-policy changes on base commit `87c29e9`; executable hashes, environment
and all 54 samples are recorded in the [raw results](results/message-copy-20261024.jsonl).

Use `python scripts/bench-provider.py --repeats 3` to build and run the fixture
on either source revision. Its quick subset uses `--quick`. Run correctness
tests separately; timings are diagnostic on a shared machine, not CI thresholds.
The benchmark exercises the SDK and plugin host bridge. It does not measure
the agent-loop move optimization, live network latency or UI rendering.

## Results

Medians; allocation bytes are cumulative allocations, not resident memory.

| Output / chunks / consumers | Allocated MiB before → after | Change | Elapsed ms before → after |
|---|---:|---:|---:|
| 4 KiB / 64 B / one stream | 0.78 → 0.69 | −11.3% | 1.009 → 1.023 |
| 32 KiB / 64 B / one stream | 18.84 → 14.46 | −23.2% | 6.783 → 6.496 |
| 128 KiB / 64 B / one stream | 266.62 → 181.44 | −31.9% | 66.693 → 57.729 |
| 4 KiB / 1 B / one stream | 25.91 → 22.78 | −12.1% | 13.683 → 13.621 |
| 32 KiB / 1 KiB / eight streams | 21.84 → 18.07 | −17.3% | 5.563 → 5.509 |
| 32 KiB / 1 KiB / 32 streams | 87.49 → 71.55 | −18.2% | 18.574 → 17.379 |
| 32 KiB / 64 B / result only | 18.84 → 14.13 | −25.0% | 6.542 → 6.909 |
| 32 KiB / 64 B / SDK only (control) | 8.97 → 8.97 | 0% | 4.053 → 4.146 |

For the 128 KiB case, median process-wide peak live allocator bytes decreased
from 7.88 to 4.85 MiB. That is not a universal peak-memory guarantee: the 32-stream
case increased from 17.63 to 19.88 MiB. Queue limits still apply to accounted
queued payload, excluding producer state, snapshots held by consumers and total
process memory. Batching depends on scheduling and how much data is ready.

The delayed-reader case slept 1 ms per emitted event and measured 559.275 →
289.766 ms. Batching reduces the number of those sleeps, so this is not a measure
of CPU speedup. Small/SDK-only workloads show no consistent timing improvement;
the result-only sample was slower despite allocating less.

This is a reduction in copies, not end-to-end zero-copy or proof of linear
complexity. Full snapshots are still created for each published batch, and the
SDK still checks snapshot prefixes before compacting text. A future delta-only
event contract would need separate consumer, recovery and compatibility work.

Correctness coverage includes batching boundaries and accounting, oversized
frames, immediate delivery, Unicode/empty deltas, equivalence after every input
prefix (including authoritative replacement and tool calls), cancellation under
saturation, producer-side frees, and the borrowed-view lifetime compile-fail test.
