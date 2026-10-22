# Prebuilt sidecar assets

A prebuilt extension archive normally carries one native library. Some packages
need files beside that library at run time. The Cursor adapter, for example,
loads `runtime/main.mjs` relative to its library. The `rpi-extension.json`
manifest can declare those files so the installer places them beside the
library.

Related: [cursor-sdk-distribution.md](cursor-sdk-distribution.md) explains why
the proprietary `@cursor/sdk` is not part of any archive.

## Manifest

Each target may carry an optional `assets` array:

```json
"x86_64-unknown-linux-gnu": {
  "format": "tar.gz",
  "library": { "path": "libcursor.so", "sha256": "..." },
  "assets": [
    { "path": "runtime/main.mjs", "sha256": "..." },
    { "path": "runtime/package.json", "sha256": "..." }
  ]
}
```

- Absent `assets` is the legacy single-library layout. Those manifests install
  the same files, in the same layout, as before. Undeclared archive entries are
  ignored, as they always were.
- Present `assets`, even empty, makes the archive strict: it must contain exactly
  the library and the declared assets. Every entry is checked.

## Path rules

An asset `path` is a relative, forward-slash path of at most 512 bytes.

- Components must be non-empty, must not start with `.`, and may contain only
  ASCII letters, digits, `-`, `_` and `.`.
- `..`, `.`, absolute paths, backslashes, drive prefixes and whitespace are
  rejected.
- A component may not have a native library extension (`.so`, `.dylib`, `.dll`,
  `.pyd`), because the loader scans the store for libraries.
- Paths are compared case-insensitively. An asset may not equal, nest under, or
  contain the library or `rpi-extension.json`, and no two assets may overlap.
- `sha256` must be 64 lowercase hex characters, and each file must match it.

## Archive rules (strict mode)

- Symbolic links, hard links, devices and FIFOs are rejected.
- Every regular file must be the library or a declared asset. Undeclared files
  are rejected.
- Every declared asset must be present.
- Duplicate entry names are rejected. The zip central directory is read directly,
  because the zip reader silently keeps one entry per name.
- Zip64 archives are rejected.
- Entry-count and size caps are the existing constants: 128 entries, 256 MiB
  expanded archive, 128 MiB per library or asset. Asset bytes count toward the
  same expanded-size budget. Budgets use bytes actually produced, not the sizes
  declared in headers.

## Installation

1. Verify the archive size and SHA-256, then the manifest's declarations.
2. Extract and verify every entry before the store is touched.
3. Stage the library and assets. Sidecars are written inside a private
   `.rpi-assets-stage-*` directory in the store, which is removed on every path.
4. Activate as one transaction. Existing files are backed up first. Missing
   parent directories are created and recorded so a rollback removes them.
5. Activation failure restores every replaced file, removes created directories,
   and leaves `native-packages.json` unchanged.
6. Upgrades remove assets that the previous version recorded and the new one no
   longer declares.

The registry record stores each asset's `path` and `sha256` under
`prebuilt.assets`. Records without it, including all legacy records, are
unchanged on disk.

## Uninstall

`rpi uninstall <crate>` removes the recorded library and assets together. It
then removes parent directories that became empty, but never the store itself.
It refuses, without deleting anything, if a recorded path fails validation or if
a parent along the path is a symbolic link or not a directory.

## Local builds

`rpi install --path` and `rpi install --build` compile a crate locally and have
no published manifest to declare assets. If the crate's `rpi-extension.json`
declares any non-empty `assets` (at top level or under `targets`), the command
stops before building and asks for the published prebuilt release. Installing
assets from a local build is not implemented.

## Limits of this verification

- Installation from crates.io or a GitHub release requires the network, so it is
  exercised by unit tests on `install_archive_at` with crafted archives, not by
  an offline end-to-end run.
- The installed Cursor worker's runtime resolution (`runtime/main.mjs` beside the
  library) is not exercised here. It needs a built library and Node 24+.
- Windows-specific behavior (case folding and zip modes) is covered by reasoning
  and the zip fixtures, not by running on Windows.
