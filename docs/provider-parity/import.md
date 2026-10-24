# `rpi import pi`

`rpi import pi` copies eligible data from a Pi 1.3.0 agent directory into
rpi's own storage. It is an explicit command: nothing is imported at startup,
and nothing is imported without a reviewed plan and a confirmation.

```sh
rpi import pi --dry-run                 # review only; writes nothing
rpi import pi                           # review, then confirm on a terminal
rpi import pi --yes                     # apply without the prompt (required when not on a terminal)
rpi import pi --from DIR --only credentials,sessions
```

## Source and destination

- Source: `--from DIR`, else `$PI_CODING_AGENT_DIR`, else `~/.pi/agent`.
  Relative paths are resolved against the working directory.
- Credentials, settings, pool and ledger destination: rpi's agent directory
  (`RPI_CODING_AGENT_DIR`, else `~/.rpi/agent`).
- Session destination:
  - If an explicit session root is configured — existing rpi `sessionDir` in
    `settings.json`, or env `RPI_CODING_AGENT_SESSION_DIR` — archives land under
    `<that-root>/pi-import/`. Relative roots resolve against the launch working
    directory. Pi's `sessionDir` is never imported or used to choose this root.
  - Otherwise, archives land under the global agent directory:
    `<agent-dir>/sessions/pi-import/` (for example `~/.rpi/agent/sessions/pi-import/`),
    never a cwd-local `<cwd>/.rpi` path.
- The source and destination must differ.

## What is read

Only these four items are opened, listed or copied:

| Source item | Category | Destination |
| --- | --- | --- |
| `auth.json` | `credentials` | `auth.json` (through `CompatibilityCredentialStore`) |
| `settings.json` | `settings` | `settings.json` (merged) |
| `multiprovider-auth.json` | `multiprovider` | `multiprovider-auth.json` (byte copy) |
| `sessions/<dir>/*.jsonl` | `sessions` | `<archive-root>/<dir>/<stem>.<sha12>.jsonl` (`<configured-root>/pi-import/` or `<agent>/sessions/pi-import/`) |

Everything else in the source tree is never read: vault directories,
credential backups or stashes, SDK stores, lock files and any other file.
Symbolic links inside the allowlist are reported and not followed. The source
is never written to and never locked. Tests hash the whole source tree,
including lock files, before and after every run.

## Flow

1. Plan every selected category. Planning reads and writes nothing.
2. Print the plan: one line per item with its outcome, then a summary.
3. If any item is a conflict, refuse. Nothing is written.
4. `--dry-run` stops here.
5. Without `--yes`, ask `Apply these changes? [y/N]` on a terminal. Only `y` or
   `yes` proceeds. On a non-terminal, refuse and ask for `--yes`.
6. Apply each category in order, saving the import ledger after each one.

Exit status: 0 on success or nothing to do, 1 on refusal or a runtime error,
2 on bad usage.

## Outcomes

| Outcome | Meaning |
| --- | --- |
| `import` | Will be written (or was written, after confirmation). |
| `already imported` | The ledger holds this exact source content. Nothing changes. |
| `already present` | The destination already holds identical content. A ledger record is added. |
| `skip` | Not imported, for the reason shown. |
| `ignore` | Deliberately not imported (for example `sessionDir`). |
| `conflict` | The destination holds different content. The whole run is refused. |

A run where nothing is left to import prints `nothing to import` and exits 0.

## Credentials (`auth.json`)

Importable entries:

- `{"type":"oauth","access":<non-empty>,"refresh":<non-empty>,"expires":<integer ms>}`.
  Extra fields such as `projectId` are kept.
- `{"type":"api_key","key":<non-empty>}`, with an optional `env` object of strings.

Skipped, with the reason printed:

- `incomplete oauth entry: access|refresh is missing`
- `incomplete oauth entry: expires is not an integer millisecond timestamp`
- `oauth expiry is out of range`
- `incomplete api_key entry: key is missing`
- `api_key env values must be strings` / `api_key env must be an object`
- `unsupported credential type `<type>``, `credential has no type`,
  `entry is not a JSON object`

**Expiry units.** Pi writes `expires` in milliseconds. rpi's `auth.json` stores
seconds (the store multiplies by 1000 on read and divides by 1000, rounding
down, on write). The importer never edits `auth.json` by hand. It passes the
store a millisecond credential rounded down to whole seconds, and the store
performs the conversion under its lock. For example, Pi's `1800000000999` is
stored as `1800000000`. Writing the millisecond value verbatim would make
the token look valid for about 30,000 years.

**Conflicts.** If the destination already has a credential for the same
provider that differs from the source, the run is refused and nothing is
overwritten. Each write also re-checks the destination under the store lock, so
a credential that appears after planning is refused rather than replaced.

**OAuth rotation notice.** Copied OAuth refresh credentials may rotate. Pi and
rpi each refresh tokens on their own, so after either side refreshes, the other
copy can stop working. A fresh login may be required, and the source login may
stop working. The importer prints this notice whenever an OAuth credential is
to be written.

## Multiprovider pool (`multiprovider-auth.json`)

The file is never parsed or rewritten. It is copied byte for byte with mode
`0600`, and only if the destination is absent. A destination that is already
identical is `already present`. A different existing file is a conflict.

## Settings (`settings.json`)

Only keys rpi understands are candidates. Each candidate is also deserialized
into rpi's settings type on its own, so a value of the wrong type is a `skip`
(`rejected by rpi: ...`) rather than a broken settings file. Legacy aliases are
written under their canonical names (`skills` as `skillDirs`, `prompts` as
`promptDirs`, `enabledModels` as `scopedModels`, `httpIdleTimeoutMs` as
`httpIdleTimeout`).

Deliberately ignored keys:

- `sessionDir`: would point rpi's session storage at Pi's directory.
- `extensions`, `extensionDirs`: Pi extension paths are not rpi cdylib
  directories. Importing them would activate native code nobody asked for.
- `lastRpiChangelogVersion`: rpi's own release-note baseline.

Keys rpi does not understand are `ignore`d and listed. Nested objects keep only
the sub-keys rpi reads (for example `retry.bogus` is listed as ignored). A
`null` value is a `skip`. A destination key with a different value is a
conflict. Unknown keys already in the destination are preserved on merge.

## Sessions (`sessions/`)

Each `sessions/<dir>/*.jsonl` file is copied to
`<resolved-session-root>/pi-import/<dir>/<stem>.<sha12>.jsonl`, where `sha12` is the first 12
hex digits of the source file's SHA-256.

- The first line gains an `imported` object:

  ```json
  "imported": {"source": "pi", "sourcePath": "...", "sha256": "...", "sessionVersion": 3}
  ```

  `sha256` is the digest of the Pi file as read. `sessionVersion` is the Pi
  header version (1 to 3 are accepted).
- Every later line is copied byte for byte. Entries, custom entries and their
  markers are preserved and never interpreted.
- A file is skipped, with the reason, if it is empty, is not a Pi session
  header, has an unsupported version or no `id`, already has an `imported`
  field, or has a line that is not a JSON object (a torn file). A symbolic link
  is skipped as `symbolic link; not followed`.

**Session-root decision (reconciled 0.8.4 host).** When an explicit session root
is configured (via saved rpi `sessionDir` or `RPI_CODING_AGENT_SESSION_DIR`),
archives belong under `<that root>/pi-import`: explicit overrides select the
same storage tree as resume and session listing. When no explicit session root
is configured, archives default to the global agent directory
(`<agent>/sessions/pi-import/`) rather than `<cwd>/.rpi/sessions`. Pi stores
sessions globally across projects; bulk-importing global archives must never
dump foreign session logs into whatever project directory happens to be current.
A dedicated `pi-import` subtree keeps provenance and snapshot names intact
without colliding with native session paths.

**Discovery is not format conversion.** The current rpi reader accepts only
its v4 format, whereas these are byte-preserving Pi v1–v3 archives. They live
under the selected root for future discovery, but are not currently listed as
native resumable sessions or opened by resume. Upstream's `/import <path>`
helper validates v4 logs, chooses native repository paths, and returns a live
session; it cannot be reused for these archives without rejecting or changing
them. Bulk import instead reuses the existing private atomic-write helper
(`config::atomic_write` via `write_private`, mode `0600`). The
`imported.source` field remains the contract for any later reader or native
port. Adding Pi-to-v4 conversion or archive entries to the native session
picker is outside this command's archival contract.

**Origin contract for native ports.** A native session port must check
`imported.source` before it grants resume, cleanup or cloud authority to a
session. An imported Pi session with `source: "pi"` is history only. The Cursor
package in particular must not activate its resume, cleanup or cloud authority
for a session whose header carries `imported`.

**Changed sources.** Snapshot names carry the source digest. A session that
grew after an earlier import gets a new snapshot next to the old one, which is
never overwritten.

## Import ledger

`import-ledger.json` in the rpi agent directory (mode `0600`, version 1) holds
one record per accounted-for item:

```json
{"category":"credentials","source":"/abs/source/auth.json","unit":"anthropic","sha256":"...","destination":"auth.json"}
```

The key is `(category, source path, unit)`. The unit is the provider for
credentials, the canonical key for settings, and empty for whole-file
categories. The digest is the canonical JSON digest for credentials and
settings, the byte digest for multiprovider and sessions.

- A repeat run whose digest matches reports `already imported` and writes
  nothing. Session records store the absolute resolved snapshot destination.
  Changing the configured root later does not relocate previously recorded
  snapshots or bypass source-based idempotency; use the recorded destination
  to find them.
- A record whose digest differs is reported as `source changed since the last
  import`.
- A corrupt or unknown-version ledger is an error. Fix it or move it aside.

Records are written only for items that were imported or found already
present. Skipped and ignored items get no record, so they are reported again on
each run.

## Verification

Tests use only temp directories and the real `rpi` binary:

```sh
cargo test -p rpi-cli --locked import      # unit tests in src/import*
cargo test -p rpi-cli --locked --test import_pi
```

The integration tests cover: dry run, full import with the mode and
expiry-unit checks, each skip reason, conflict refusal for each category, the
idempotent re-run, changed-source detection, the source tree hash before and
after each run, `--only`, the non-terminal refusal and usage errors. Session
coverage includes the default agent root (confirming nothing is created under
the cwd), absolute and relative environment overrides (including dry run and
idempotent re-run), saved `sessionDir` precedence, private HOME (`0700`), and
snapshot permissions (`0600`).

## Not covered

- Real Pi credentials and real provider logins are not used.
- Sessions are archived, not resumed. rpi cannot open Pi v3 files yet.
- Pi's `!command` API keys are copied verbatim, unresolved, as Pi stores them.
  Resolution happens when rpi reads the credential, as for any rpi key.
- No vendor-specific handling. Provider names are opaque strings.
