//! Sidecar asset removal through the real `rpi uninstall` command. Installing a
//! prebuilt archive needs the network, so installation is covered by the unit
//! tests beside the installer; these tests drive the offline uninstall path
//! against registries and files written into an isolated agent directory.
use std::path::Path;
use std::process::{Command, Stdio};

fn sha(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

fn prebuilt_record(artifacts: &[&str], assets: &[(&str, &[u8])]) -> serde_json::Value {
    let assets: Vec<serde_json::Value> = assets
        .iter()
        .map(|(path, bytes)| serde_json::json!({"path": path, "sha256": sha(bytes)}))
        .collect();
    serde_json::json!({
        "name": "demo",
        "version": "1.0.0",
        "source": "prebuilt:https://github.com/owner/repo",
        "artifacts": artifacts,
        "prebuilt": {
            "repository": "https://github.com/owner/repo",
            "target": "x86_64-unknown-linux-gnu",
            "manifest_sha256": "a".repeat(64),
            "source_commit": "b".repeat(40),
            "sdk_version": "0.3.23",
            "plugin_abi": 4,
            "archive_sha256": "c".repeat(64),
            "library_sha256": "d".repeat(64),
            "custom_repository": true,
            "assets": assets
        }
    })
}

fn uninstall(agent: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rpi"))
        .args(["uninstall", "demo"])
        .env("RPI_CODING_AGENT_DIR", agent)
        .env("RPI_OFFLINE", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn uninstall_removes_library_and_recorded_assets_and_prunes_empty_directories() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let store = agent.join("extensions");
    write(&store.join("libdemo.so"), b"library");
    write(&store.join("runtime/main.mjs"), b"main");
    write(&store.join("runtime/dist/a.mjs"), b"dist");
    write(&store.join("runtime/user.txt"), b"user");
    write(&store.join("other/keep.txt"), b"other");
    let record = prebuilt_record(
        &["libdemo.so"],
        &[
            ("runtime/main.mjs", b"main"),
            ("runtime/dist/a.mjs", b"dist"),
        ],
    );
    write(
        &agent.join("native-packages.json"),
        serde_json::to_string_pretty(&[record]).unwrap().as_bytes(),
    );

    let output = uninstall(&agent);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!store.join("libdemo.so").exists());
    assert!(!store.join("runtime/main.mjs").exists());
    assert!(!store.join("runtime/dist").exists());
    assert_eq!(
        std::fs::read(store.join("runtime/user.txt")).unwrap(),
        b"user"
    );
    assert_eq!(
        std::fs::read(store.join("other/keep.txt")).unwrap(),
        b"other"
    );
    assert!(!agent.join("native-packages.json").exists());
}

#[test]
fn legacy_single_library_records_uninstall_without_touching_other_files() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let store = agent.join("extensions");
    write(&store.join("libdemo.so"), b"library");
    write(&store.join("runtime/main.mjs"), b"not recorded");
    write(
        &agent.join("native-packages.json"),
        serde_json::to_string_pretty(&[serde_json::json!({
            "name": "demo",
            "version": "1.0.0",
            "source": null,
            "artifacts": ["libdemo.so"]
        })])
        .unwrap()
        .as_bytes(),
    );

    let output = uninstall(&agent);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!store.join("libdemo.so").exists());
    assert_eq!(
        std::fs::read(store.join("runtime/main.mjs")).unwrap(),
        b"not recorded"
    );
}

#[test]
fn uninstall_refuses_recorded_asset_paths_that_escape_the_store() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let store = agent.join("extensions");
    write(&store.join("libdemo.so"), b"library");
    write(&temp.path().join("outside.mjs"), b"keep");
    write(
        &agent.join("native-packages.json"),
        serde_json::to_string_pretty(&[prebuilt_record(
            &["libdemo.so"],
            &[("../../outside.mjs", b"keep")],
        )])
        .unwrap()
        .as_bytes(),
    );

    let output = uninstall(&agent);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot safely uninstall"));
    assert_eq!(
        std::fs::read(temp.path().join("outside.mjs")).unwrap(),
        b"keep"
    );
    assert_eq!(std::fs::read(store.join("libdemo.so")).unwrap(), b"library");
    assert!(agent.join("native-packages.json").exists());
}

#[cfg(unix)]
#[test]
fn uninstall_refuses_an_asset_whose_parent_is_a_symbolic_link() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let store = agent.join("extensions");
    write(&store.join("libdemo.so"), b"library");
    let outside = temp.path().join("outside");
    write(&outside.join("main.mjs"), b"keep");
    std::os::unix::fs::symlink(&outside, store.join("runtime")).unwrap();
    write(
        &agent.join("native-packages.json"),
        serde_json::to_string_pretty(&[prebuilt_record(
            &["libdemo.so"],
            &[("runtime/main.mjs", b"keep")],
        )])
        .unwrap()
        .as_bytes(),
    );

    let output = uninstall(&agent);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read(outside.join("main.mjs")).unwrap(), b"keep");
    assert_eq!(std::fs::read(store.join("libdemo.so")).unwrap(), b"library");
    assert!(agent.join("native-packages.json").exists());
}
