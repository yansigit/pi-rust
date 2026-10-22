# R01 install matrix (host half)

R01 asks for independent install, upgrade, uninstall, offline and disabled
discovery for the three provider extension packages without changing existing
providers. This note records what the host verifies end to end from a loopback
package repository, and what it cannot verify yet.

Related: [install-assets.md](install-assets.md) describes the prebuilt manifest
`assets` declaration and its transactional installer.
[cursor-sdk-distribution.md](cursor-sdk-distribution.md) covers the retained
licenses and SDK redistribution review; neither is repeated here.

## Harness

`crates/rpi-cli/tests/install_matrix.rs` starts a crates.io-compatible sparse
registry on `127.0.0.1` and drives the real `rpi` binary against it:

- `GET /index/config.json` returns `{"dl": "<base>/dl"}`.
- `GET /index/<prefix>/<name>` returns one NDJSON record per published version,
  with the SHA-256 of the served `.crate`.
- `GET /dl/<name>/<version>/download` returns the `.crate` built by the test.

Every test process gets its own temporary `CARGO_HOME` whose `config.toml`
replaces `crates-io` with `sparse+http://127.0.0.1:<port>/index/`, its own
`RPI_CODING_AGENT_DIR`, and an empty working directory. No request leaves the
loopback interface, `CARGO_TARGET_DIR` is removed so the nested Cargo build
cannot contend with the test run's target lock, and ambient provider
credentials are removed.

The registry publishes three stand-in `cdylib` packages under the real provider
package names (`rpi-multiprovider`, `rpi-cursor-sdk`,
`rpi-antigravity-auth`). Each exports the unified `rpi_plugin_register`
entrypoint and registers nothing, so any catalog or tool change observed while
one is installed would be a host bug, not fixture behavior. The stand-ins are
not the upstream provider implementations; see the limits below.

## Matrix

`install_upgrade_downgrade_and_uninstall_leave_existing_providers_unchanged`
covers, with a user `models.json`/`auth.json` provider present throughout:

| Step | Verified |
| --- | --- |
| Fresh install | The three packages install from repository resolution, in non-sorted order; `native-packages.json` holds exactly the three records, sorted by name, each with `version: 0.1.0`, no `source`/`prebuilt` override, and its own artifact in `extensions/`; the server actually served each index and archive. |
| Existing providers | `--list-models` is byte-identical to the pre-install baseline, including the user's `existing-core/existing-model`; `models.json` and `auth.json` are byte-identical; the agent directory gains only `extensions/`, `native-packages.json` and `native-packages.lock`. |
| Reinstall | Reinstalling the installed version prints the already-installed notice and changes neither the registry nor the artifact bytes. |
| Upgrade | Publishing `0.2.0` and reinstalling upgrades `rpi-multiprovider` and `rpi-antigravity-auth`; the Cursor record keeps `0.1.0`. |
| Downgrade policy | Requiring `rpi-multiprovider` `0.1.0` without `--force` fails with `rpi-multiprovider 0.2.0 is installed; use --force to downgrade to 0.1.0` and changes nothing; the same command with `--force` installs `0.1.0`. |
| Uninstall | Removing one package deletes only its artifact and record; the other two remain and still load (`extensions: loaded 2 plugin(s)`), and an unrelated `extensions/user-note.txt` survives. Removing the rest deletes the registry file and leaves only that note. |
| Absent package | Uninstalling an absent package reports `Rust extension is not installed: …` and exits successfully, preserving the host quirk. |
| Final catalog | `--list-models` again matches the pre-install baseline. |

`offline_operation_and_disabled_discovery_use_only_installed_packages` installs
all three, then stops the loopback server:

| Step | Verified |
| --- | --- |
| Offline discovery | `RPI_OFFLINE=1 rpi --verbose --list-models` loads all three packages (`extensions: loaded 3 plugin(s) (0 tool(s))`) and matches the baseline catalog. |
| Disabled discovery | Adding `--no-extensions` suppresses the extension summary entirely, leaves the catalog identical and the store identical. |
| Offline reinstall | `RPI_OFFLINE=1 rpi install --build <package> --force` rebuilds each package from the warm Cargo cache, keeps `0.1.0`, and never contacts the stopped repository. |
| Offline prebuilt refusal | `RPI_OFFLINE=1 rpi install rpi-multiprovider` fails with `prebuilt installation requires network access; use --build for an explicit Cargo build` before touching the registry or store. |

`repository_failures_leave_no_package_state_behind` covers repository and
package-integrity failures:

| Step | Verified |
| --- | --- |
| Unknown crate | A name the repository does not publish fails during resolution (`could not resolve`) with no registry and no store entry. |
| Corrupt archive | A published checksum that disagrees with the archive fails (`failed to verify the checksum`) before activation, leaving no state. |
| Asset declaring package | A package shipping `rpi-extension.json` with non-empty `assets` is refused for `--build` (`declares sidecar assets` … `install the published prebuilt release instead`); assets require the prebuilt release that records them. |
| Healthy package | A valid package still installs afterwards, showing the failures did not damage the environment. |

Counts: 3 test functions, 31 `rpi` invocations (19 install: 14 successful and 5
refused; 4 uninstall; 8 catalog listings), all offline.

## Limits of this verification

- **Prebuilt distribution is not loopback-testable.** The host hard-codes
  `https://crates.io/api/v1/crates` for discovery
  (`prebuilt_extensions.rs`, `CRATES_API`), requires `https` and host
  `github.com` in `canonical_repository`, derives release URLs as
  `<repository>/releases/download/<name>-v<version>/<file>`, and uses
  `https://api.github.com` for tag lookups. `updates.rs` likewise hard-codes
  the crates.io URL. There is no environment variable, flag or injected
  registry base, so a loopback `rpi-extension.json` plus archive cannot be
  fetched. Closing this needs a host seam: either a resolved
  `{ api_base, release_base }` threaded through `prebuilt_extensions::install`
  and `latest_compatible_version`, or a single explicit override (for example
  `RPI_PREBUILT_API_BASE`/`RPI_PREBUILT_RELEASE_BASE`) that is refused outside
  tests and never becomes the default. Until then, manifest validation,
  archive extraction (including `assets`), and install transactions remain
  covered only by the in-module unit tests, and the prebuilt upgrade path is
  not exercised end to end.
- **Stand-in packages.** The three published crates are protocol-faithful but
  empty extensions published under the provider package names; they are not
  the upstream Antigravity, Cursor or multiprovider implementations, which
  live in `rpi-package`. Runtime behavior of those packages is other ledger
  rows' evidence.
- **Update discovery is not exercised.** `latest_compatible_version` and the
  startup update notice need the same network seam as above; only the offline
  suppression path (`RPI_OFFLINE`/`RPI_DISABLE_UPDATE_CHECK`) is observable
  here.
- **Disabled means `--no-extensions`.** The host has no per-package disabled
  state, so "disabled discovery" is verified as extension discovery being
  suppressed while the store and registry stay intact.
- **Licenses and redistribution** are reviewed in
  [cursor-sdk-distribution.md](cursor-sdk-distribution.md); the installer only
  places declared files and does not decide licensing.

## Reproduction

```sh
RUSTFLAGS="-D warnings" cargo +1.78.0 check --workspace --all-targets --locked
RUSTFLAGS="-D warnings" cargo +1.78.0 test -p rpi-cli --test install_matrix --locked
```

The matrix runs in a few seconds on a warm target directory; each nested Cargo
build compiles a dependency-free fixture crate.
