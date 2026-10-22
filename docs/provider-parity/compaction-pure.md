# Pure compaction preparation, auto-trigger and lifecycle order

This note covers the host-independent port of Pi 1.3.0's *pure* compaction
decisions (ledger H06/H10 partial): `prepareCompaction`, the automatic-compaction
trigger table, branch entry collection/preparation, and the ordered
`_runAutoCompaction` lifecycle.

The existing [compaction lifecycle checks](host-compaction-results.json) verify
reason/retry/source/failure event fields; this work fills the gap they record as
not tested — "complete compaction preparation, summary prompts and entry
representation equivalence" and automatic-trigger parity — for the decision
functions.

## Where the code lives

- `crates/rpi-harness/src/compaction/pure/` — new modules:
  - `model.rs` — Pi-shaped JSON helpers (UTF-16 lengths, `JSON.stringify`,
    truthiness, `LeafSelector`, `PureSettings`, `json_eq`).
  - `projection.rs` — `buildSessionPath`, `buildContextEntries`,
    `projectContextEntry`, `buildSessionProjection`, `sessionEntryToContextMessages`.
  - `tokens.rs` — `estimateTokens`, `calculateContextTokens`,
    `estimateContextTokens`, `estimateProjectedContextTokens`,
    `getCurrentSystemMessage`.
  - `file_ops.rs` — `extractFileOpsFromMessage`, `computeFileLists`,
    `formatFileOperations`.
  - `prepare.rs` — `findProjectedCutPoint`, `prepareCompaction`,
    `extractFileOperations`, `getSummarizationFailure`.
  - `branch.rs` — `collectEntriesForBranchSummary`, `prepareBranchEntries`.
  - `trigger.rs` — `isContextOverflow`, `isRecoverableLength`, `shouldCompact`,
    `checkCompaction`.
  - `lifecycle.rs` — `auto_compaction_order` (the `_runAutoCompaction` operation
    order and emitted events).
- `crates/rpi-harness/tests/compaction_pure.rs` — loads the fixtures with
  `include_str!` and compares complete values.

The module works on Pi-shaped JSON rather than the host `Entry` enum because the
host model predates Pi 1.3.0's `context_edit` entries and projection. Wiring the
projection/context-edit model into `session` and the harness loop is separate work.

## Reproducing

```sh
source /home/user/Projects/.parity-env.sh
node docs/provider-parity/check-compaction-pure-reference.mjs      # check mode
node docs/provider-parity/check-compaction-trigger-reference.mjs   # check mode
# add --write to regenerate; reruns are byte-identical
```

Both scripts import the unchanged Pi 1.3.0 modules from
`provider-references/integrated`, assert the sha256 of every imported upstream
file, inject stubs/IDs, and emit JSON. Check mode fails on any difference.

Rust side:

```sh
RUSTFLAGS="-D warnings" cargo +1.78.0 test -p rpi-harness --locked --test compaction_pure
```

## Coverage and limits

`compaction-pure-evidence.json` records the exact row counts, hashes and the
explicit limits. In short: the fixtures cover projection (14), token estimation
(11 + 3 + 6), preparation (16), branch collection/preparation (5 + 4), file
operation utils and failure classification (5 + 5 + 4 + 6), the overflow
classifier (41), recoverable length (5), `shouldCompact` (5), the
`_checkCompaction` table (19) and the auto-compaction lifecycle order (12).

Not covered here: ISO timestamp parsing, JS float exponent formatting outside the
fixture values, arbitrary overflow strings beyond the corpus, and the real
extension runner's `session_compact_failed` handler gate (the reference stubs
that method). Summary prompt text for compaction remains with the existing host
tests; branch-summary prompts are already covered by
[branch-summary-reference.json](branch-summary-reference.json).
