# Offline provider pipeline benchmark

`scripts/bench-provider.py --quick` builds the release test fixture, then runs it
alone in a fresh process for each sample. Omit `--quick` for representative
4/32/128 KiB outputs, 64/1024-byte chunks, a 4 KiB one-byte stress case, 1/8/32
streams, delayed readers, and result-only consumption. No network or credentials
are used. Fixed model, timestamp, text, usage and identifiers make results
repeatable through the real SDK driver and host provider adapter.

The benchmark-only commit can be applied to baseline `534d316`. Use the same
harness on subsequent revisions; after the async stream API migration, pass
`--features provider-performance,bounded-provider-stream`. The conditional push
adapter accommodates the intentional Rust API break without changing workloads.

JSONL includes environment/revision, elapsed time, global allocation count and
bytes, peak live allocator bytes, and transmitted JSON bytes. Linux additionally
reports process user/system CPU and peak RSS via `/usr/bin/time`. Allocation
instrumentation includes runtime and host overhead and may perturb execution;
peak live bytes are whole-process allocations, not queue payload accounting.
Use repeated samples and compare distributions; CI must not assert speedups.

Run correctness tests separately. This harness initially measures conversion
and streaming work; tool polls, resource reuse and bounded queue diagnostics
need the later features and their targeted tests.
