//! R01 host matrix: fresh install, upgrade, downgrade policy, uninstall,
//! offline operation and disabled discovery for the three provider extension
//! packages, driven through the real `rpi` binary against a loopback package
//! repository.
//!
//! The repository is a crates.io-compatible sparse registry served on
//! `127.0.0.1` by this test. Each provider package is a small stand-in
//! `cdylib` published under the package's real name and version series. The
//! stand-ins register no tools or providers, so the built-in provider catalog
//! must stay byte-identical before and after their installation.
//!
//! The prebuilt distribution protocol (`rpi-extension.json` plus a GitHub
//! release archive) is not reachable from a loopback host: discovery and
//! release URLs are hard-coded to `https://crates.io` and `https://github.com`
//! in `prebuilt_extensions.rs`, with no override. Those flows are covered by
//! the in-module unit tests beside the installer instead; see
//! `docs/provider-parity/install-matrix.md` for the exact gap. This file
//! exercises the package lifecycle that the host *does* expose end to end:
//! registry resolution, Cargo builds, the transactional store, offline
//! behavior and `--no-extensions` discovery suppression.
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const PROVIDERS: [&str; 3] = [
    "rpi-antigravity-auth",
    "rpi-cursor-sdk",
    "rpi-multiprovider",
];
const INITIAL_VERSION: &str = "0.1.0";
const UPGRADE_VERSION: &str = "0.2.0";

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

// ---------------------------------------------------------------------------
// Loopback package repository
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Release {
    library_source: String,
    /// A published `rpi-extension.json` beside the crate, as the real Cursor
    /// package ships to declare its worker sidecars.
    extension_manifest: Option<String>,
    /// Serve an archive that does not match the published checksum.
    corrupt_checksum: bool,
}

#[derive(Default)]
struct Repository {
    releases: BTreeMap<String, BTreeMap<String, Release>>,
}

impl Repository {
    fn with_providers() -> Self {
        let mut repository = Self::default();
        for package in PROVIDERS {
            repository.publish(package, INITIAL_VERSION);
        }
        repository
    }

    fn publish(&mut self, package: &str, version: &str) {
        self.releases
            .entry(package.to_string())
            .or_default()
            .insert(version.to_string(), Release::new(package, version));
    }

    fn publish_with_extension_manifest(&mut self, package: &str, version: &str) {
        let mut release = Release::new(package, version);
        release.extension_manifest = Some(json!({
            "schema_version": 1,
            "name": package,
            "version": version,
            "repository": "https://github.com/pi-rust/rpi-package",
            "tag": format!("{package}-v{version}"),
            "source_commit": "0123456789abcdef0123456789abcdef01234567",
            "sdk": {"version": "0.3.23", "plugin_abi": 4},
            "targets": {
                "x86_64-unknown-linux-gnu": {
                    "format": "tar.gz",
                    "url": format!("https://github.com/pi-rust/rpi-package/releases/download/{package}-v{version}/package.tar.gz"),
                    "size": 1024,
                    "sha256": "a".repeat(64),
                    "library": {"path": "librpi_cursor_sdk.so", "sha256": "b".repeat(64)},
                    "assets": [{"path": "runtime/main.mjs", "sha256": "c".repeat(64)}],
                }
            }
        })
        .to_string());
        self.releases
            .entry(package.to_string())
            .or_default()
            .insert(version.to_string(), release);
    }

    fn publish_corrupt_checksum(&mut self, package: &str, version: &str) {
        let mut release = Release::new(package, version);
        release.corrupt_checksum = true;
        self.releases
            .entry(package.to_string())
            .or_default()
            .insert(version.to_string(), release);
    }

    /// Sparse-index document: one JSON line per published version.
    fn index(&self, package: &str) -> Option<String> {
        let versions = self.releases.get(package)?;
        let mut body = String::new();
        for (version, release) in versions {
            let archive = self.archive(package, version)?;
            let checksum = if release.corrupt_checksum {
                "0".repeat(64)
            } else {
                sha256_hex(&archive)
            };
            let line = json!({
                "name": package,
                "vers": version,
                "deps": Vec::<Value>::new(),
                "cksum": checksum,
                "features": Value::Object(Default::default()),
                "yanked": false,
            });
            body.push_str(&line.to_string());
            body.push('\n');
        }
        Some(body)
    }

    /// A registry crate: `<package>-<version>/{Cargo.toml,src/lib.rs}`.
    fn archive(&self, package: &str, version: &str) -> Option<Vec<u8>> {
        let release = self.releases.get(package)?.get(version)?;
        let prefix = format!("{package}-{version}");
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        append_tar(
            &mut builder,
            &format!("{prefix}/Cargo.toml"),
            crate_manifest(package, version).as_bytes(),
        );
        append_tar(
            &mut builder,
            &format!("{prefix}/src/lib.rs"),
            release.library_source.as_bytes(),
        );
        if let Some(manifest) = &release.extension_manifest {
            append_tar(
                &mut builder,
                &format!("{prefix}/rpi-extension.json"),
                manifest.as_bytes(),
            );
        }
        builder.finish().expect("finish crate archive");
        Some(
            builder
                .into_inner()
                .expect("crate archive writer")
                .finish()
                .expect("finish gzip stream"),
        )
    }
}

impl Release {
    fn new(package: &str, version: &str) -> Self {
        Self {
            library_source: format!(
                "//! Loopback install-matrix stand-in for `{package}` {version}.\n\
                 //!\n\
                 //! It exports the unified ABI entrypoint and registers nothing, so the\n\
                 //! host catalog and tool set must be unchanged while it is installed.\n\
                 #[no_mangle]\n\
                 pub extern \"C\" fn rpi_plugin_register(_: *const std::ffi::c_void) -> i32 {{\n\
                 0\n\
                 }}\n"
            ),
            extension_manifest: None,
            corrupt_checksum: false,
        }
    }
}

fn crate_manifest(package: &str, version: &str) -> String {
    format!(
        "[package]\n\
         name = \"{package}\"\n\
         version = \"{version}\"\n\
         edition = \"2021\"\n\
         description = \"rpi install-matrix fixture standing in for a prebuilt provider extension\"\n\
         \n\
         [lib]\n\
         crate-type = [\"cdylib\"]\n"
    )
}

fn append_tar<W: Write>(builder: &mut tar::Builder<W>, path: &str, bytes: &[u8]) {
    let mut header = tar::Header::new_ustar();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    builder
        .append_data(&mut header, path, bytes)
        .expect("append crate entry");
}

struct Loopback {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Loopback {
    fn serve(repository: Arc<Mutex<Repository>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback repository");
        let address = listener.local_addr().expect("loopback address");
        let base = format!("http://{address}");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        listener
            .set_nonblocking(true)
            .expect("non-blocking loopback listener");
        let thread = {
            let repository = Arc::clone(&repository);
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            let base = base.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => respond(stream, &repository, &requests, &base),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            base,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn registry(&self) -> String {
        format!("sparse+{}/index/", self.base)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("stop loopback repository");
        }
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.stop();
    }
}

fn respond(
    mut stream: std::net::TcpStream,
    repository: &Arc<Mutex<Repository>>,
    requests: &Arc<Mutex<Vec<String>>>,
    base: &str,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut request = Vec::new();
    let mut buffer = [0u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => request.extend_from_slice(&buffer[..count]),
            Err(_) => return,
        }
        if request.len() > 64 * 1024 {
            return;
        }
    }
    let text = String::from_utf8_lossy(&request);
    let path = text
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();
    requests.lock().unwrap().push(path.clone());
    // Responses deliberately carry no validators, so Cargo re-reads the index
    // after the repository publishes a new version.
    let (status, body) = route(repository, &path, base);
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
}

fn route(repository: &Arc<Mutex<Repository>>, path: &str, base: &str) -> (&'static str, Vec<u8>) {
    let repository = repository.lock().unwrap();
    if let Some(rest) = path.strip_prefix("/index/") {
        if rest == "config.json" {
            return (
                "200 OK",
                json!({"dl": format!("{base}/dl")}).to_string().into_bytes(),
            );
        }
        let package = rest.rsplit('/').next().unwrap_or_default();
        return match repository.index(package) {
            Some(index) => ("200 OK", index.into_bytes()),
            None => ("404 Not Found", b"unknown crate".to_vec()),
        };
    }
    if let Some(rest) = path.strip_prefix("/dl/") {
        let mut parts = rest.split('/');
        let package = parts.next().unwrap_or_default();
        let version = parts.next().unwrap_or_default();
        let download = parts.next().unwrap_or_default();
        if download == "download" {
            if let Some(archive) = repository.archive(package, version) {
                return ("200 OK", archive);
            }
        }
    }
    ("404 Not Found", b"not found".to_vec())
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn write_cargo_home(root: &Path, registry: &str) -> PathBuf {
    let home = root.join("cargo-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[source.crates-io]\n\
             replace-with = \"loopback\"\n\
             \n\
             [source.loopback]\n\
             registry = \"{registry}\"\n"
        ),
    )
    .unwrap();
    home
}

fn agent_dir(root: &Path) -> PathBuf {
    let agent = root.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    agent
}

fn work_dir(root: &Path) -> PathBuf {
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    work
}

fn command(agent: &Path, work: &Path, cargo_home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rpi"));
    command
        .current_dir(work)
        .env("RPI_CODING_AGENT_DIR", agent)
        .env("CARGO_HOME", cargo_home)
        .env("RPI_OFFLINE", "1")
        // A nested Cargo must never contend with the test run's target lock or
        // reach the network for anything but the loopback registry.
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_NET_OFFLINE")
        .env_remove("RPI_EXTENSIONS_DIR")
        .env_remove("RPI_DISABLE_UPDATE_CHECK")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("ANTHROPIC_BASE_URL")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("LLAMA_CPP_API_KEY")
        .env_remove("PROXY_API_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("RPI_FAKE_PROVIDER_API_KEY")
        .stdin(Stdio::null());
    command
}

fn install(agent: &Path, work: &Path, cargo_home: &Path, args: &[&str], offline: bool) -> Output {
    let mut command = command(agent, work, cargo_home);
    command.env("RPI_OFFLINE", if offline { "1" } else { "0" });
    command.arg("install").args(args);
    command.output().unwrap()
}

fn uninstall(agent: &Path, work: &Path, cargo_home: &Path, package: &str) -> Output {
    let mut command = command(agent, work, cargo_home);
    command.args(["uninstall", package]);
    command.output().unwrap()
}

fn list_models(agent: &Path, work: &Path, cargo_home: &Path, extra: &[&str]) -> Output {
    let mut command = command(agent, work, cargo_home);
    command.args(extra).arg("--list-models");
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

fn assert_failure(output: &Output, context: &str, expected: &str) {
    assert_eq!(
        output.status.code(),
        Some(1),
        "{context} should fail\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
    let combined = format!("{}{}", stdout(output), stderr(output));
    assert!(
        combined.contains(expected),
        "{context} should report `{expected}`\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

fn registry(agent: &Path) -> Vec<Value> {
    match std::fs::read(agent.join("native-packages.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).expect("native-packages.json is valid JSON"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("read native package registry: {error}"),
    }
}

fn versions(agent: &Path) -> BTreeMap<String, String> {
    registry(agent)
        .into_iter()
        .map(|record| {
            (
                record["name"].as_str().unwrap().to_string(),
                record["version"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn artifact(agent: &Path, package: &str) -> PathBuf {
    let records = registry(agent);
    let record = records
        .iter()
        .find(|record| record["name"] == package)
        .unwrap_or_else(|| panic!("{package} has no registry record"));
    let name = record["artifacts"][0]
        .as_str()
        .expect("recorded artifact name");
    agent.join("extensions").join(name)
}

fn file_bytes(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

fn top_level_entries(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

fn store_snapshot(agent: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn collect(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect(root, &path, out);
            } else {
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                out.insert(relative, std::fs::read(&path).unwrap());
            }
        }
    }
    let store = agent.join("extensions");
    let mut snapshot = BTreeMap::new();
    collect(&store, &store, &mut snapshot);
    snapshot
}

fn write_existing_provider(agent: &Path, models: &[u8], auth: &[u8]) {
    std::fs::write(agent.join("models.json"), models).unwrap();
    std::fs::write(agent.join("auth.json"), auth).unwrap();
}

const EXISTING_MODELS: &[u8] = br#"{"providers":{"existing-core":{"api":"openai-completions","baseUrl":"http://127.0.0.1:9/v1","models":[{"id":"existing-model"}]}}}"#;
const EXISTING_AUTH: &[u8] = br#"{"existing-core":{"type":"api_key","key":"fixture-key"}}"#;

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

#[test]
fn install_upgrade_downgrade_and_uninstall_leave_existing_providers_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let repository = Arc::new(Mutex::new(Repository::with_providers()));
    let loopback = Loopback::serve(Arc::clone(&repository));
    let cargo_home = write_cargo_home(temp.path(), &loopback.registry());
    let work = work_dir(temp.path());
    let agent = agent_dir(temp.path());
    write_existing_provider(&agent, EXISTING_MODELS, EXISTING_AUTH);

    // Baseline: the same user configuration with no installed extensions.
    let baseline_agent = agent_dir(&temp.path().join("baseline"));
    write_existing_provider(&baseline_agent, EXISTING_MODELS, EXISTING_AUTH);
    let baseline = list_models(&baseline_agent, &work, &cargo_home, &[]);
    assert_success(&baseline, "baseline catalog");
    assert!(stdout(&baseline).contains("existing-core"));
    let baseline_catalog = stdout(&baseline);

    // Fresh installs, deliberately not in sorted order.
    for package in [
        "rpi-multiprovider",
        "rpi-cursor-sdk",
        "rpi-antigravity-auth",
    ] {
        let output = install(&agent, &work, &cargo_home, &["--build", package], false);
        assert_success(&output, &format!("fresh install of {package}"));
        assert!(
            stdout(&output).contains("installed "),
            "install should report the activated artifact: {}",
            stdout(&output)
        );
        assert_eq!(versions(&agent)[package], INITIAL_VERSION);
        assert!(
            artifact(&agent, package).is_file(),
            "{package} artifact should exist"
        );
    }
    // Records are sorted by package name, and each package owns its artifact.
    let records = registry(&agent);
    let names: Vec<&str> = records
        .iter()
        .map(|record| record["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, PROVIDERS);
    let mut artifacts = BTreeSet::new();
    for record in &records {
        artifacts.insert(record["artifacts"][0].as_str().unwrap().to_string());
        assert_eq!(record["version"], INITIAL_VERSION);
        assert!(
            record.get("prebuilt").is_none(),
            "a registry build records no prebuilt provenance"
        );
        assert!(
            record.get("source").is_none(),
            "a registry build keeps the update-eligible source"
        );
    }
    assert_eq!(artifacts.len(), 3, "each package installs its own library");
    let requests = loopback.requests();
    for package in PROVIDERS {
        assert!(
            requests
                .iter()
                .any(|path| path.ends_with(&format!("/{package}"))),
            "the loopback repository served the {package} index: {requests:?}"
        );
        assert!(
            requests
                .iter()
                .any(|path| path.starts_with(&format!("/dl/{package}/"))),
            "the loopback repository served the {package} archive: {requests:?}"
        );
    }

    // Discovery of the installed packages does not change the built-in catalog
    // or the user's own providers/credentials.
    let catalog = list_models(&agent, &work, &cargo_home, &["--verbose"]);
    assert_success(&catalog, "catalog with installed providers");
    assert_eq!(
        stdout(&catalog),
        baseline_catalog,
        "installing provider extensions must not change existing providers"
    );
    assert!(
        stderr(&catalog).contains("extensions: loaded 3 plugin(s)"),
        "all three installed packages should load: {}",
        stderr(&catalog)
    );
    assert_eq!(
        file_bytes(&agent.join("models.json")).as_deref(),
        Some(EXISTING_MODELS)
    );
    assert_eq!(
        file_bytes(&agent.join("auth.json")).as_deref(),
        Some(EXISTING_AUTH)
    );
    assert_eq!(
        top_level_entries(&agent),
        [
            "auth.json",
            "extensions",
            "models.json",
            "native-packages.json",
            "native-packages.lock"
        ]
        .iter()
        .map(|entry| entry.to_string())
        .collect::<BTreeSet<_>>(),
        "installation writes only the extension store and its registry"
    );

    // Reinstalling the installed version is a no-op.
    let registry_before = file_bytes(&agent.join("native-packages.json"));
    let artifact_before = file_bytes(&artifact(&agent, "rpi-multiprovider"));
    let reinstall = install(
        &agent,
        &work,
        &cargo_home,
        &["--build", "rpi-multiprovider"],
        false,
    );
    assert_success(&reinstall, "reinstall of the installed version");
    assert!(
        stdout(&reinstall).contains("is already installed; use --force to reinstall it"),
        "{}",
        stdout(&reinstall)
    );
    assert_eq!(
        file_bytes(&agent.join("native-packages.json")),
        registry_before
    );
    assert_eq!(
        file_bytes(&artifact(&agent, "rpi-multiprovider")),
        artifact_before
    );

    // Upgrading two packages leaves the third at its version.
    {
        let mut repository = repository.lock().unwrap();
        repository.publish("rpi-multiprovider", UPGRADE_VERSION);
        repository.publish("rpi-antigravity-auth", UPGRADE_VERSION);
    }
    for package in ["rpi-multiprovider", "rpi-antigravity-auth"] {
        let upgrade = install(&agent, &work, &cargo_home, &["--build", package], false);
        assert_success(&upgrade, &format!("upgrade of {package}"));
    }
    let upgraded = versions(&agent);
    assert_eq!(upgraded["rpi-multiprovider"], UPGRADE_VERSION);
    assert_eq!(upgraded["rpi-antigravity-auth"], UPGRADE_VERSION);
    assert_eq!(upgraded["rpi-cursor-sdk"], INITIAL_VERSION);

    // Downgrades require an explicit --force and are refused otherwise.
    let downgrade = install(
        &agent,
        &work,
        &cargo_home,
        &["--build", "rpi-multiprovider", "--version", INITIAL_VERSION],
        false,
    );
    assert_failure(
        &downgrade,
        "downgrade without --force",
        "rpi-multiprovider 0.2.0 is installed; use --force to downgrade to 0.1.0",
    );
    assert_eq!(versions(&agent)["rpi-multiprovider"], UPGRADE_VERSION);
    let forced = install(
        &agent,
        &work,
        &cargo_home,
        &[
            "--build",
            "rpi-multiprovider",
            "--version",
            INITIAL_VERSION,
            "--force",
        ],
        false,
    );
    assert_success(&forced, "forced downgrade");
    assert_eq!(versions(&agent)["rpi-multiprovider"], INITIAL_VERSION);

    // Uninstall is per package; unrelated store files survive.
    std::fs::write(agent.join("extensions").join("user-note.txt"), b"keep").unwrap();
    let uninstalled = uninstall(&agent, &work, &cargo_home, "rpi-cursor-sdk");
    assert_success(&uninstalled, "uninstall rpi-cursor-sdk");
    assert!(
        stdout(&uninstalled).contains("uninstalled Rust extension rpi-cursor-sdk"),
        "{}",
        stdout(&uninstalled)
    );
    let remaining = versions(&agent);
    assert_eq!(remaining.len(), 2);
    assert!(remaining.contains_key("rpi-antigravity-auth"));
    assert!(remaining.contains_key("rpi-multiprovider"));
    assert_eq!(
        file_bytes(&agent.join("extensions").join("user-note.txt")).as_deref(),
        Some(&b"keep"[..])
    );
    assert_eq!(
        stdout(&list_models(&agent, &work, &cargo_home, &[])),
        baseline_catalog
    );
    let after_uninstall = list_models(&agent, &work, &cargo_home, &["--verbose"]);
    assert_success(&after_uninstall, "catalog after one uninstall");
    assert!(
        stderr(&after_uninstall).contains("extensions: loaded 2 plugin(s)"),
        "the remaining packages still load: {}",
        stderr(&after_uninstall)
    );

    for package in ["rpi-antigravity-auth", "rpi-multiprovider"] {
        let output = uninstall(&agent, &work, &cargo_home, package);
        assert_success(&output, &format!("uninstall {package}"));
    }
    assert!(
        registry(&agent).is_empty(),
        "the registry disappears with the last package"
    );
    assert_eq!(
        store_snapshot(&agent),
        [(PathBuf::from("user-note.txt"), b"keep".to_vec())]
            .into_iter()
            .collect::<BTreeMap<_, _>>()
    );
    // Uninstalling an absent package is a reported no-op, not an error.
    let absent = uninstall(&agent, &work, &cargo_home, "rpi-multiprovider");
    assert_success(&absent, "uninstall an absent package");
    assert!(
        stdout(&absent).contains("Rust extension is not installed: rpi-multiprovider"),
        "{}",
        stdout(&absent)
    );
    // The final discovery still matches the pre-install catalog.
    let final_catalog = list_models(&agent, &work, &cargo_home, &[]);
    assert_success(&final_catalog, "catalog after uninstall");
    assert_eq!(stdout(&final_catalog), baseline_catalog);
}

#[test]
fn offline_operation_and_disabled_discovery_use_only_installed_packages() {
    let temp = tempfile::tempdir().unwrap();
    let repository = Arc::new(Mutex::new(Repository::with_providers()));
    let mut loopback = Loopback::serve(Arc::clone(&repository));
    let cargo_home = write_cargo_home(temp.path(), &loopback.registry());
    let work = work_dir(temp.path());
    let agent = agent_dir(temp.path());

    let baseline_agent = agent_dir(&temp.path().join("baseline"));
    let baseline = list_models(&baseline_agent, &work, &cargo_home, &[]);
    assert_success(&baseline, "baseline catalog");
    let baseline_catalog = stdout(&baseline);

    for package in PROVIDERS {
        let output = install(&agent, &work, &cargo_home, &["--build", package], false);
        assert_success(&output, &format!("install {package}"));
    }

    // Discovery works with no network at all: the repository is gone.
    loopback.stop();
    let served = loopback.requests().len();
    let store_before = store_snapshot(&agent);

    let offline = list_models(&agent, &work, &cargo_home, &["--verbose"]);
    assert_success(&offline, "offline catalog");
    assert_eq!(stdout(&offline), baseline_catalog);
    assert!(
        stderr(&offline).contains("extensions: loaded 3 plugin(s)"),
        "offline discovery still loads installed packages: {}",
        stderr(&offline)
    );

    // `--no-extensions` disables discovery without touching the store.
    let disabled = list_models(
        &agent,
        &work,
        &cargo_home,
        &["--verbose", "--no-extensions"],
    );
    assert_success(&disabled, "disabled-discovery catalog");
    assert_eq!(stdout(&disabled), baseline_catalog);
    assert!(
        !stderr(&disabled).contains("extensions:"),
        "--no-extensions must not discover installed packages: {}",
        stderr(&disabled)
    );
    assert_eq!(store_snapshot(&agent), store_before);
    assert_eq!(versions(&agent).len(), 3);

    // The Cargo cache still resolves the loopback repository's packages while
    // offline, so a forced reinstall of each succeeds and changes no version.
    for package in PROVIDERS {
        let offline_reinstall = install(
            &agent,
            &work,
            &cargo_home,
            &["--build", package, "--force"],
            true,
        );
        assert_success(
            &offline_reinstall,
            &format!("offline forced reinstall of {package}"),
        );
        assert_eq!(versions(&agent)[package], INITIAL_VERSION);
    }

    // A prebuilt install is refused before it touches the store when offline.
    // Reinstalls may rebuild the same source, so compare against the state
    // immediately before the refusal rather than the pre-reinstall snapshot.
    let registry_before = file_bytes(&agent.join("native-packages.json"));
    let store_before_refusal = store_snapshot(&agent);
    let prebuilt = install(&agent, &work, &cargo_home, &["rpi-multiprovider"], true);
    assert_failure(
        &prebuilt,
        "offline prebuilt install",
        "prebuilt installation requires network access; use --build for an explicit Cargo build",
    );
    assert_eq!(
        file_bytes(&agent.join("native-packages.json")),
        registry_before
    );
    assert_eq!(store_snapshot(&agent), store_before_refusal);

    // Nothing was fetched after the repository stopped.
    assert_eq!(
        loopback.requests().len(),
        served,
        "offline commands must not contact the repository"
    );
}

#[test]
fn repository_failures_leave_no_package_state_behind() {
    let temp = tempfile::tempdir().unwrap();
    let repository = Arc::new(Mutex::new(Repository::with_providers()));
    repository
        .lock()
        .unwrap()
        .publish_corrupt_checksum("rpi-antigravity-auth", INITIAL_VERSION);
    repository
        .lock()
        .unwrap()
        .publish_with_extension_manifest("rpi-cursor-sdk", INITIAL_VERSION);
    let loopback = Loopback::serve(Arc::clone(&repository));
    let cargo_home = write_cargo_home(temp.path(), &loopback.registry());
    let work = work_dir(temp.path());
    let agent = agent_dir(temp.path());
    let empty: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();

    // A crate the repository does not publish fails during resolution.
    let absent = install(
        &agent,
        &work,
        &cargo_home,
        &["--build", "rpi-absent-provider"],
        false,
    );
    assert_failure(
        &absent,
        "install from an unknown crate",
        "could not resolve",
    );
    assert!(registry(&agent).is_empty());
    assert_eq!(store_snapshot(&agent), empty);

    // A checksum that disagrees with the archive fails before activation.
    let corrupt = install(
        &agent,
        &work,
        &cargo_home,
        &["--build", "rpi-antigravity-auth"],
        false,
    );
    assert_failure(
        &corrupt,
        "install from a corrupted archive",
        "failed to verify the checksum",
    );
    assert!(registry(&agent).is_empty());
    assert_eq!(store_snapshot(&agent), empty);

    // A package that declares sidecar assets cannot be installed from source;
    // it needs its published prebuilt release, which also records the assets.
    let assets = install(
        &agent,
        &work,
        &cargo_home,
        &["--build", "rpi-cursor-sdk"],
        false,
    );
    assert_failure(
        &assets,
        "local build of a package that declares assets",
        "declares sidecar assets",
    );
    let combined = format!("{}{}", stdout(&assets), stderr(&assets));
    assert!(
        combined.contains("install the published prebuilt release instead"),
        "{combined}"
    );
    assert!(registry(&agent).is_empty());
    assert_eq!(store_snapshot(&agent), empty);

    // The repository is otherwise healthy: a valid package still installs.
    let healthy = install(
        &agent,
        &work,
        &cargo_home,
        &["--build", "rpi-multiprovider"],
        false,
    );
    assert_success(&healthy, "install a healthy package after failures");
    assert_eq!(versions(&agent)["rpi-multiprovider"], INITIAL_VERSION);
    assert!(artifact(&agent, "rpi-multiprovider").is_file());
}
