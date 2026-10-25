//! Replay tolerance for durable context edits.
//!
//! Pi validates a context edit only when it is appended
//! (`session-manager.ts:1358-1400`): projection tolerates an edit whose target
//! is not in the session (`buildSessionProjection` looks the target up in the
//! finalized context and simply finds nothing, `:540-561`). A lane that
//! replays with strict validation instead turns a Pi-loadable file into an
//! unopenable one, so replay must keep the entry and leave validation to new
//! appends.
use rpi_agent::AgentMessage;
use rpi_harness::session::jsonl::storage::JsonlSessionStorage;
use rpi_harness::session::jsonl::types::JsonlV4Header;
use rpi_harness::session::memory::SystemClock;
use rpi_harness::session::session::Session;
use rpi_harness::session::types::*;
use rpi_harness::session::DefaultIdGenerator;
use rpi_tools::{FileContent, FileSystem, OsExecutionEnv};
use serde_json::Value;
use std::sync::Arc;

const PATH: &str = "session.jsonl";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_tolerates_a_context_edit_whose_target_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().to_owned()));
    let clock = Arc::new(SystemClock);
    let ids = Arc::new(DefaultIdGenerator::new());
    let storage = JsonlSessionStorage::create(
        fs.clone(),
        PATH,
        JsonlV4Header::new(
            "replay".into(),
            0,
            dir.path().to_string_lossy().into_owned(),
            None,
            None,
            None,
        ),
        clock.clone(),
        ids.clone(),
    )
    .await
    .unwrap();
    storage
        .append_entry(
            ProvisionedEntry {
                id: "message".into(),
                kind: ProvisionedKind::Message {
                    message: AgentMessage::User(rpi_ai::types::UserMessage::new("original", 1)),
                    terminate: None,
                },
            },
            "main",
        )
        .await
        .unwrap();
    storage
        .append_context_edit("edit".into(), "message".into(), Value::Null, "main")
        .await
        .unwrap();

    // The append-time invariant holds: a dangling new edit is rejected.
    let live = Session::new(Arc::new(storage), None);
    let error = live
        .append_context_edit("missing", Value::Null)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("Entry missing not found"),
        "{error}"
    );

    // Rewrite the persisted edit so its target no longer exists, the way a
    // fork or an older writer can leave a Pi session on disk.
    let text = fs.read_text_file(PATH, None).await.unwrap();
    let patched = text.replace("\"targetId\":\"message\"", "\"targetId\":\"missing\"");
    assert_ne!(text, patched, "{text}");
    fs.write_file(PATH, FileContent::Text(patched), None)
        .await
        .unwrap();

    let reloaded = JsonlSessionStorage::load(fs.clone(), PATH, clock, ids)
        .await
        .expect("a context edit with a missing target must not make the file unopenable");
    let session = Session::new(Arc::new(reloaded), None);
    let entries = session
        .view("main")
        .find_entries(&Default::default())
        .await
        .unwrap();
    let edit = entries
        .iter()
        .find_map(|entry| match entry {
            Entry::ContextEdit(edit) => Some(edit.clone()),
            _ => None,
        })
        .expect("replay keeps the edit entry");
    assert_eq!(edit.target_id, "missing");
    assert!(edit.replacement.is_null());

    // And it stays a no-op at projection time, exactly like Pi.
    let projected = rpi_harness::session::context::project_context_messages(
        &entries,
        &rpi_harness::session::context::SessionContextBuildOptions::default(),
    );
    assert!(
        projected
            .iter()
            .any(|(id, message)| id == "message" && matches!(message, AgentMessage::User(_))),
        "{projected:#?}"
    );
}
