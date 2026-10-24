//! Durable context edits: validation is atomic with branch selection, not a
//! facade-only check. Original Pi: dist/core/session-manager.js:962-994.
use rpi_agent::AgentMessage;
use rpi_harness::session::jsonl::{JsonlSessionRepo, JsonlSessionRepoOptions};
use rpi_harness::session::memory::{CounterIdGenerator, FakeClock, InMemorySessionRepo};
use rpi_harness::session::sqlite::SqliteSessionRepo;
use rpi_harness::session::types::*;
use serde_json::{json, Value};
use std::sync::Arc;

async fn qualify(repo: &impl SessionRepo) {
    let store = repo.create(&SessionCreateOptions::default()).await.unwrap();
    store
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
    store.create_lane("other", None).await.unwrap();
    let error = store
        .append_context_edit("bad".into(), "message".into(), Value::Null, "other")
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Entry message is not on the active branch"),
        "{error}"
    );
    assert!(store.get_entry("bad").await.unwrap().is_none());
    assert!(store
        .append_context_edit("bad".into(), "message".into(), json!({}), "main")
        .await
        .unwrap_err()
        .to_string()
        .contains("Context edit replacement must be null or contain string/array content"));
    store
        .append_context_edit(
            "replace".into(),
            "message".into(),
            json!({"content":"changed"}),
            "main",
        )
        .await
        .unwrap();
    let edit = store
        .append_context_edit("omit".into(), "message".into(), Value::Null, "main")
        .await
        .unwrap();
    let wire = serde_json::to_value(&edit).unwrap();
    assert_eq!(wire["replacement"], Value::Null);
    assert_eq!(serde_json::from_value::<Entry>(wire.clone()).unwrap(), edit);
    let mut absent = wire;
    absent.as_object_mut().unwrap().remove("replacement");
    assert!(serde_json::from_value::<Entry>(absent).is_err());
    store.sync().await.unwrap();
    let reopened = repo.open(&store.metadata()).await.unwrap();
    assert_eq!(reopened.get_entry("omit").await.unwrap(), Some(edit));
    assert!(reopened.get_entry("bad").await.unwrap().is_none());
}

#[tokio::test]
async fn context_edit_backends_validate_and_reopen() {
    qualify(&InMemorySessionRepo::new(
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    ))
    .await;
    let dir = tempfile::tempdir().unwrap();
    qualify(&SqliteSessionRepo::open(dir.path().join("edits.db")).unwrap()).await;
    qualify(&JsonlSessionRepo::new(
        JsonlSessionRepoOptions {
            fs: Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/proj".into())),
            sessions_root: "/sessions".into(),
            clock: Arc::new(FakeClock::new()),
            ids: Arc::new(CounterIdGenerator::new()),
        },
        "/proj".into(),
    ))
    .await;
}

#[tokio::test]
async fn context_edit_projection_is_branch_local_and_compaction_does_not_resurrect() {
    use rpi_harness::session::context::{build_session_context, SessionContextBuildOptions};
    let repo = InMemorySessionRepo::new(
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    );
    let store = repo.create(&SessionCreateOptions::default()).await.unwrap();
    for (id, text) in [
        ("first", "secret ".repeat(200)),
        ("second", "question".into()),
        ("third", "tail".into()),
    ] {
        store
            .append_entry(
                ProvisionedEntry {
                    id: id.into(),
                    kind: ProvisionedKind::Message {
                        message: AgentMessage::User(rpi_ai::types::UserMessage::new(text, 1)),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
    }
    store.create_lane("fork", Some("third")).await.unwrap();
    store
        .append_context_edit(
            "edit".into(),
            "first".into(),
            json!({"content":"replacement ".repeat(200)}),
            "main",
        )
        .await
        .unwrap();
    store
        .append_context_edit("omit".into(), "second".into(), Value::Null, "main")
        .await
        .unwrap();
    let mut all = store.find_entries(&EntryQuery::default()).await.unwrap();
    all.sort_by_key(Entry::seq);
    let opts = SessionContextBuildOptions::default();
    let context = build_session_context(&all, &opts);
    assert_eq!(context.messages.len(), 2);
    let wire = serde_json::to_string(&context.messages).unwrap();
    assert!(wire.contains("replacement"));
    assert!(!wire.contains("secret"));
    assert!(!wire.contains("question"));
    let fork = build_session_context(&all[..3], &opts);
    assert_eq!(fork.messages.len(), 3);
    let prep = rpi_harness::compaction::wiring::projected_preparation(
        &all,
        rpi_harness::types::CompactionSettings {
            enabled: true,
            reserve_tokens: 1,
            keep_recent_tokens: 1,
        },
    )
    .unwrap();
    let prepared = serde_json::to_string(&(
        prep.messages_to_summarize,
        prep.turn_prefix_messages,
        prep.retained_tail,
    ))
    .unwrap();
    assert!(prepared.contains("replacement"));
    assert!(!prepared.contains("secret"));
    assert!(!prepared.contains("question"));
}
