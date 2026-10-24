use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};
pub(crate) fn library() -> &'static Path {
    static FIXTURE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    fixture(&FIXTURE, "delegation_v2")
}
// Also compiled by runtime unit tests, which only need the handwritten fixture.
#[allow(dead_code)]
pub(crate) fn sdk_library() -> &'static Path {
    static FIXTURE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    fixture(&FIXTURE, "delegation_sdk")
}
fn fixture(lock: &'static OnceLock<(tempfile::TempDir, PathBuf)>, name: &str) -> &'static Path {
    &lock
        .get_or_init(|| {
            let directory = tempfile::tempdir().unwrap();
            let filename = format!(
                "{}{name}{}",
                std::env::consts::DLL_PREFIX,
                std::env::consts::DLL_SUFFIX
            );
            let library = directory.path().join(&filename);
            let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
            let workspace = manifest_dir.parent().unwrap().parent().unwrap();
            let executable = std::env::current_exe().unwrap();
            // Separate caches across test binaries (and toolchain target dirs), but
            // stable across repeated runs. Never inspect the host's dependency rlibs:
            // their feature variants need not belong to a consistent Cargo graph.
            let cache = executable
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("delegation-fixtures")
                .join(executable.file_stem().unwrap());
            std::fs::create_dir_all(&cache).unwrap();
            let build_lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(cache.join("build.lock"))
                .unwrap();
            // Protect manifest/lockfile writes and artifact copying too, including
            // simultaneous invocations of the same test binary in other processes.
            fs2::FileExt::lock_exclusive(&build_lock).unwrap();
            let package = cache.join(name);
            std::fs::create_dir_all(&package).unwrap();
            let manifest = format!(
                r#"[package]
name = "{name}"
version = "0.0.0"
edition = "2021"

[workspace]

[lib]
path = "fixture.rs"
crate-type = ["cdylib"]

[dependencies]
rpi-plugin-sdk = {{ path = {sdk:?}, features = ["provider"] }}
rpi-ai = {{ path = {ai:?} }}
serde_json = "1"
async-trait = "0.1"
tokio = {{ version = "1", features = ["rt-multi-thread", "macros", "time", "sync"] }}

[profile.dev]
debug = 0
"#,
                sdk = workspace.join("crates/rpi-plugin-sdk"),
                ai = workspace.join("crates/rpi-ai"),
            );
            write_if_changed(&package.join("Cargo.toml"), manifest.as_bytes());
            write_if_changed(
                &package.join("fixture.rs"),
                &std::fs::read(manifest_dir.join(format!("tests/fixtures/{name}.rs"))).unwrap(),
            );
            // Seed resolution with the workspace pins and the new local root.
            // Cargo must prune unused workspace packages before --locked works.
            let mut cargo_lock = std::fs::read_to_string(workspace.join("Cargo.lock")).unwrap();
            cargo_lock.push_str(&format!(
                "\n[[package]]\nname = {name:?}\nversion = \"0.0.0\"\ndependencies = [\n \"async-trait\",\n \"rpi-ai\",\n \"rpi-plugin-sdk\",\n \"serde_json\",\n \"tokio\",\n]\n"
            ));
            write_if_changed(&package.join("Cargo.lock"), cargo_lock.as_bytes());
            let target = cache.join("target");
            // The Cargo executable and inherited RUSTUP_TOOLCHAIN select the same
            // toolchain as the test runner, including on the Rust 1.78 gate.
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| env!("CARGO").into());
            let output = std::process::Command::new(&cargo)
                .current_dir(&package)
                .args(["metadata", "--offline", "--format-version", "1"])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "fixture lockfile normalization failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            // Normalization may remove packages and redundant version qualifiers,
            // but must never introduce a version/source/checksum absent from the
            // copied lockfile. Check before compiling any code with --locked.
            let normalized = std::fs::read_to_string(package.join("Cargo.lock")).unwrap();
            for identity in package_identities(&normalized) {
                assert!(
                    package_identities(&cargo_lock).any(|pinned| pinned == identity),
                    "fixture resolution changed a workspace pin: {identity}"
                );
            }
            let output = std::process::Command::new(cargo)
                .current_dir(&package)
                .args(["build", "--offline", "--locked", "--lib", "--target-dir"])
                .arg(&target)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "independent delegation DLL must build: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            // Load a private copy, so a subsequent Cargo build cannot overwrite a
            // mapped DLL. Dropping build_lock releases the cross-process lock.
            std::fs::copy(target.join("debug").join(filename), &library).unwrap();
            (directory, library)
        })
        .1
}
fn package_identities(lockfile: &str) -> impl Iterator<Item = &str> {
    lockfile
        .split("[[package]]\n")
        .skip(1)
        .map(|package| package.split("dependencies = [").next().unwrap().trim())
}
fn write_if_changed(path: &Path, bytes: &[u8]) {
    if std::fs::read(path).ok().as_deref() != Some(bytes) {
        std::fs::write(path, bytes).unwrap();
    }
}
