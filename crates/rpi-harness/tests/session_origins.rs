use rpi_harness::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
use rpi_harness::session::memory::{CounterIdGenerator, FakeClock};
use rpi_harness::session::types::{
    AppendTarget, ProvisionedEntry, ProvisionedKind, SessionCreateOptions, SessionMetadata,
    SessionRepo, SessionStorage,
};
use rpi_harness::session::{InMemorySessionStorage, SqliteSessionRepo};
use rpi_tools::{Durability, FileSystem, OsExecutionEnv};
use std::sync::Arc;

fn note(id: &str) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.into(),
        kind: ProvisionedKind::Custom {
            custom_type: "journal".into(),
            data: Some(serde_json::json!({"id":id})),
        },
    }
}
fn current(revision: u64) -> AppendTarget {
    AppendTarget::Current {
        lane: "main".into(),
        branch_revision: revision,
    }
}
fn origin(anchor: &str) -> AppendTarget {
    AppendTarget::Origin {
        lane: "main".into(),
        branch_revision: 0,
        anchor_id: Some(anchor.into()),
    }
}

async fn exercise(storage: &dyn SessionStorage, durability: Durability) {
    let initial = storage.snapshot().await.unwrap();
    assert_eq!(initial.revision, 0);
    assert_eq!(initial.branches[0].revision, 0);
    assert!(storage
        .commit_entry(note("future"), current(1))
        .await
        .is_err());
    assert!(storage
        .commit_entry(note("missing-anchor"), origin("missing"))
        .await
        .is_err());
    assert_eq!(storage.snapshot().await.unwrap().revision, 0);
    let first = storage
        .commit_entry(note("base"), current(0))
        .await
        .unwrap();
    assert_eq!(first.entry.id(), "base");
    assert_eq!(first.revision, first.entry.seq());
    assert_eq!(first.durability, durability);
    assert!(first.advanced_lane);
    storage.set_label("base", Some("Origin")).await.unwrap();
    storage.set_name(Some("Test session")).await.unwrap();
    storage
        .commit_entry(note("same"), current(0))
        .await
        .unwrap();
    storage.move_lane("main", Some("base")).await.unwrap();
    let branched = storage.snapshot().await.unwrap();
    let revision = branched.branches[0].revision;
    assert!(revision > 0);
    storage
        .commit_entry(note("new-branch"), current(revision))
        .await
        .unwrap();
    assert!(storage
        .commit_entry(note("stale"), current(0))
        .await
        .is_err());
    assert!(storage.get_entry("stale").await.unwrap().is_none());
    let late = storage
        .commit_entry(note("late"), origin("same"))
        .await
        .unwrap();
    assert!(!late.advanced_lane);
    assert_eq!(late.durability, durability);
    assert_eq!(late.entry.parent_id(), Some("same"));
    assert_eq!(
        storage.snapshot().await.unwrap().branches[0]
            .leaf_id
            .as_deref(),
        Some("new-branch")
    );

    // Returning to the same leaf does not revive an old request's branch authority.
    storage.move_lane("main", Some("same")).await.unwrap();
    let late = storage
        .commit_entry(note("late-again"), origin("late"))
        .await
        .unwrap();
    assert!(!late.advanced_lane);
    assert_eq!(late.entry.parent_id(), Some("late"));
    let snapshot = storage.snapshot().await.unwrap();
    assert_eq!(snapshot.branches[0].leaf_id.as_deref(), Some("same"));
    assert!(snapshot.branches[0].revision > revision);
    let active = storage
        .commit_entry(note("active"), current(snapshot.branches[0].revision))
        .await
        .unwrap();
    assert!(active.advanced_lane);
    let snapshot = storage.snapshot().await.unwrap();
    assert_eq!(snapshot.entries.len(), 6);
    assert_eq!(snapshot.labels["base"], "Origin");
    assert_eq!(snapshot.name.as_deref(), Some("Test session"));
    assert_eq!(snapshot.branches[0].leaf_id.as_deref(), Some("active"));
    assert_eq!(snapshot.revision, active.revision);
}

#[tokio::test]
async fn committed_entries_and_origin_branches_survive_backend_reopen() {
    let clock = Arc::new(FakeClock::new());
    let ids = Arc::new(CounterIdGenerator::new());
    let memory = InMemorySessionStorage::new(
        SessionMetadata {
            id: "memory".into(),
            created_at: 0,
            parent_session_id: None,
        },
        clock.clone(),
        ids.clone(),
    );
    exercise(&memory, Durability::Ephemeral).await;
    let dir = tempfile::tempdir().unwrap();
    let sqlite = SqliteSessionRepo::open(dir.path().join("session.db")).unwrap();
    let stored = sqlite
        .create(&SessionCreateOptions::default())
        .await
        .unwrap();
    exercise(&stored, Durability::Durable).await;
    let snapshot = stored.snapshot().await.unwrap();
    let reopened = sqlite
        .open(&snapshot.metadata)
        .await
        .unwrap()
        .snapshot()
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(reopened).unwrap(),
        serde_json::to_value(snapshot).unwrap()
    );

    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().to_owned()));
    let jsonl = JsonlSessionStorage::create(
        fs.clone(),
        "session.jsonl",
        JsonlV4Header::new(
            "file".into(),
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
    exercise(&jsonl, Durability::Durable).await;
    let snapshot = jsonl.snapshot().await.unwrap();
    let reopened = JsonlSessionStorage::load(fs, "session.jsonl", clock, ids)
        .await
        .unwrap()
        .snapshot()
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(reopened).unwrap(),
        serde_json::to_value(snapshot).unwrap()
    );
}
