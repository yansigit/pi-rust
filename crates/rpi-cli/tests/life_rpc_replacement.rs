//! Offline real-process RPC replacement identity and failure recovery.
#![cfg(unix)]
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

#[tokio::test]
async fn rpc_new_resume_and_invalid_destination_use_real_session_files() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let tmp = temp.path().join("tmp");
    let cwd = temp.path().join("project");
    for dir in [&home, &tmp, &cwd] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_rpi"))
        .current_dir(&cwd)
        .args([
            "--mode",
            "rpc",
            "--provider",
            "anthropic",
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-tools",
            "--no-skills",
            "--no-context-files",
            "--no-prompt-templates",
            "--system-prompt",
            "test",
            "--session-dir",
        ])
        .arg(temp.path().join("sessions"))
        .env("HOME", &home)
        .env("TMPDIR", &tmp)
        .env("RPI_CODING_AGENT_DIR", temp.path().join("agent"))
        .env_remove("RPI_CODING_AGENT_SESSION_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let ready = output.next_line().await.unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&ready).unwrap()["type"],
        "ready"
    );
    async fn request(
        input: &mut tokio::process::ChildStdin,
        output: &mut tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
        msg: Value,
    ) -> Value {
        input
            .write_all(format!("{msg}\n").as_bytes())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                let line = output.next_line().await.unwrap().expect("RPC closed");
                let value: Value = serde_json::from_str(&line).unwrap();
                if value["type"] == "response" && value["id"] == msg["id"] {
                    return value;
                }
            }
        })
        .await
        .unwrap()
    }
    let first = request(&mut input, &mut output, json!({"id":1,"type":"get_state"})).await;
    let new = request(
        &mut input,
        &mut output,
        json!({"id":2,"type":"new_session"}),
    )
    .await;
    assert_eq!(new["success"], true, "{new}");
    assert_eq!(new["data"]["cancelled"], false);
    assert_ne!(first["data"]["sessionId"], new["data"]["sessionId"]);
    assert!(std::path::Path::new(new["data"]["sessionFile"].as_str().unwrap()).is_file());
    let missing = request(
        &mut input,
        &mut output,
        json!({"id":3,"type":"switch_session","sessionPath":"does-not-exist"}),
    )
    .await;
    assert_eq!(missing["success"], false);
    let after = request(&mut input, &mut output, json!({"id":4,"type":"get_state"})).await;
    assert_eq!(after["data"]["sessionId"], new["data"]["sessionId"]);
    let resumed = request(
        &mut input,
        &mut output,
        json!({"id":5,"type":"switch_session","sessionPath":first["data"]["sessionId"]}),
    )
    .await;
    assert_eq!(resumed["success"], true, "{resumed}");
    assert_eq!(resumed["data"]["sessionId"], first["data"]["sessionId"]);
    let invalid = request(
        &mut input,
        &mut output,
        json!({"id":6,"type":"fork","entryId":"absent"}),
    )
    .await;
    assert_eq!(invalid["success"], false);
    assert_eq!(invalid["error"], "Invalid entry ID for forking");
    request(&mut input, &mut output, json!({"id":7,"type":"stop"})).await;
    drop(input);
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(15), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}
