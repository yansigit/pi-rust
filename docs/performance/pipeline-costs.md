# Internal pipeline costs (2026-10-24)

These measurements describe the initial optimization pass, before the later
single-dispatch append writer and Anthropic history/preparation changes. For the
subsequent Linux end-to-end comparison and validation, see the
[runtime follow-up](runtime-20261024/README.md). The raw hashes below identify the
earlier measured sources; their microbenchmark reductions are not application
speedup claims.

This pass reduces redundant copies, serialization and append overhead across six
paths. It preserves model history, state transitions, native RPC fields and
acknowledged JSONL recovery boundaries.

| Path | Change | Remaining work/constraint |
|---|---|---|
| History | The default context builder borrows the slice after the compaction boundary. Branch queries walk references and copy only selected results. Read-only provider hooks borrow Context; an actual patch gets a private copy. | Public owned-message converters and asynchronous providers still need owned history. Stateful transforms/projectors run on every call; their results are not cached across calls. |
| HTTP | All three built-in HTTP protocols prepare encoded requests once and share buffered body bytes across retries. Normal Anthropic requests serialize typed parameters directly; its non-stream compatibility mode keeps the necessary mutable Value transformation. | New requests are encoded again with their current messages, options and credentials. Previously built provider DLLs need a rebuild against the updated AI implementation to benefit. |
| State | Entry/record storage keeps the canonical payloads. The ID map and unified mutation log hold indexes instead of additional payload copies; query results remain defensive copies. | Active-operation tracking still owns its descriptors. SQLite snapshot serialization retains the legacy shape, including its redundant serialized fields, for compatibility. |
| Recovery | CLI `--no-session` uses typed in-memory frames, with the same encoder/reducer, entry bindings and clear boundaries. It skips JSON frame records, generated record IDs and their reducer/log entries. The recorder borrows streaming events instead of cloning them first. | Persistent sessions keep durable frame records. In-process failure recovery remains enabled in volatile mode. |
| RPC | Native RPC delta updates normalize and encode a shared snapshot once, reuse its bytes for the required fields, and enqueue JSON bytes directly. Different message/event snapshots are handled separately. | The existing protocol still transmits both snapshots and delta mirrors. JSON print mode and remote WebSocket projection are not changed by this optimization. |
| Files | JSONL lazily acquires an optional reusable append handle. Unix handles check the device/inode before each append and reopen when the path changes. Windows and platforms without safe stable file IDs reopen the current path on every append, including after native or external replacement. Each acknowledged append still completes and flushes before the reducer applies it. Unsupported filesystems use the existing append API. | This is the existing write-visibility boundary, not an added power-loss fsync guarantee. Host session locking still provides single-writer ownership. |

`Session::with_volatile_progress()` is opt-in for embedded hosts. Clones share
the typed recovery cache. `AgentHarness::set_session` adopts the target's storage,
recovery mode and cache together; existing facade views follow the switch. Raw
`Session::set_storage` replacement clears volatile mode and its cache.
Normal session and SQLite storage formats are unchanged. Recovery tests cover
bindings before/after entry commit so a failed commit cannot hide progress and
a successful one cannot be replayed twice.

## Reproduction

```sh
python scripts/bench-pipeline.py --offline --baseline-ref 87c29e9
```

Omit `--offline` when dependencies are not cached. The runner stages an isolated
temporary benchmark crate and leaves the checkout unchanged. It reads the
baseline context builder and reducer with `git show`, and copies the current
RPC projection and prepared-request implementation for read-only instrumentation.
The fixture is [scripts/bench-pipeline/main.rs](../../scripts/bench-pipeline/main.rs).

Windows x86-64, Rust release profile with thin LTO and 16 codegen units. Three
fresh-process runs per variant/workload, alternating their order. One warmup
call is excluded from each sample. The counting allocator forwards alloc,
alloc_zeroed, realloc and dealloc to System; cumulative requested bytes include
buffer growth and are not live memory or RSS. Counter overhead and the shared
machine affect timings; these are diagnostic microbenchmarks, not CI thresholds
or live model/network/UI speedups.

The [raw results](results/pipeline-costs-20261024.jsonl) include all 36 samples,
baseline revision, source/executable hashes, OS and toolchain. HTTP does not send
requests; its fixture compares four encodings of a 512 KiB body with one prepared
encoding plus four replayable attempts. The progress baseline uses JSON records
in the current in-memory backend; this isolates the new volatile mode rather
than comparing the complete old and new application. The file workload runs on
Windows, where the corrected writer reopens each append; it measures the remaining
API/dispatch savings rather than handle reuse. Unix handle reuse is not timed here.

## Results

Medians for each complete workload, not per event:

| Workload | Iterations | Allocated MiB before → after | Change | Elapsed ms before → after |
|---|---:|---:|---:|---:|
| Context: 96 messages × 8 KiB | 50 builds | 78.684 → 39.624 | −49.6% | 25.694 → 7.925 |
| State: 96 entries, query oldest one on branch | 25 sessions | 121.218 → 40.413 | −66.7% | 31.181 → 9.356 |
| HTTP: 512 KiB body, four attempts | 25 requests | 149.942 → 24.907 | −83.4% | 43.757 → 5.594 |
| Native RPC: 64 KiB snapshot | 100 updates | 70.145 → 37.926 | −45.9% | 20.399 → 8.549 |
| Recovery: 128 deltas × 128 bytes | 20 runs | 30.934 → 18.347 | −40.7% | 11.954 → 3.243 |
| OS file: 128 acknowledged appends | 3 batches | 0.593 → 0.322 | −45.7% | 53.493 → 34.895 |

Correctness coverage includes read-only/empty/mutating context hooks, old SQLite
snapshot round trips and defensive reads, body-byte sharing and custom headers,
volatile/durable recovery equivalence after every input prefix, committed-entry
bindings, storage switching, escaped/Unicode RPC deltas and distinct snapshots,
write failure before reducer commit, visibility after each acknowledged append,
and append writers surviving both environment-mediated and native atomic file
replacement. Regression coverage also checks missing-file recreation, failed
reopen without acknowledging a write to the old file, and transfer of recovery
cache/commit bindings to existing views during session switches.

Initial-pass verification: workspace tests with `rpi-extensions/provider-performance`
passed 2,098 tests, with 9 existing ignored tests. Real provider-package and
plugin-stub DLL integration tests ran. Workspace/all-target compilation, Rust
formatting and diff whitespace checks passed. Clippy completed with the existing
raw-pointer lint explicitly allowed for the scan; no diagnostics overlapped the
changed lines or new Rust modules. Strict Rustdoc remains blocked by existing
documentation issues, including the private `CaptureState` link in
`rpi-tools/src/shell_output.rs` and the redundant `WatchHandle` link in
`rpi-harness/src/watcher.rs`; these files were not changed in this pass.
