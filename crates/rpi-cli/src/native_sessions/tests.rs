use super::*;
use rpi_harness::error::{SessionError, SessionResult};
use rpi_harness::session::memory::{CounterIdGenerator, FakeClock, InMemorySessionStorage};
use rpi_harness::session::types::*;

fn owner() -> NativeOwner {
    NativeOwner {
        generation: "generation-1".into(),
        id: 17,
        plugin: "fixture".into(),
        child: false,
    }
}
fn memory() -> Arc<InMemorySessionStorage> {
    Arc::new(InMemorySessionStorage::new(
        SessionMetadata {
            id: "original".into(),
            created_at: 0,
            parent_session_id: None,
        },
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    ))
}
async fn call(ports: &Arc<SessionPorts>, owner: NativeOwner, args: Value) -> Result<Value, String> {
    ports
        .request(owner, args, CancellationToken::new(), || {
            panic!("origin operations must not resolve the active session")
        })
        .await
}
async fn capture(ports: &Arc<SessionPorts>, storage: Arc<dyn SessionStorage>) -> String {
    ports
        .request(
            owner(),
            json!({"action":"capture"}),
            CancellationToken::new(),
            || Ok(storage),
        )
        .await
        .unwrap()["origin"]
        .as_str()
        .unwrap()
        .into()
}
fn append(origin: &str, action: &str) -> Value {
    json!({"action":action,"origin":origin,"customType":"fixture:journal","data":{"turnId":"original-turn"}})
}

#[tokio::test]
async fn origin_operations_preserve_owner_branch_and_real_commit_receipts() {
    let ports = Arc::new(SessionPorts::default());
    let storage = memory();
    let origin = capture(&ports, storage.clone()).await;
    let first = call(&ports, owner(), append(&origin, "append"))
        .await
        .unwrap();
    assert_eq!(first["durability"], "ephemeral");
    assert_eq!(first["revision"], 1);
    let first: CommittedEntry = serde_json::from_value(first).unwrap();
    assert!(storage.get_entry(first.entry.id()).await.unwrap().is_some());
    storage.move_lane("main", None).await.unwrap();
    assert!(call(&ports, owner(), append(&origin, "append"))
        .await
        .is_err());
    let late: CommittedEntry = serde_json::from_value(
        call(&ports, owner(), append(&origin, "journalAppend"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(!late.advanced_lane);
    assert_eq!(late.entry.parent_id(), Some(first.entry.id()));
    let later: CommittedEntry = serde_json::from_value(
        call(&ports, owner(), append(&origin, "journalAppend"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(later.entry.parent_id(), Some(late.entry.id()));
    let snapshot = call(
        &ports,
        owner(),
        json!({"action":"snapshot", "origin":origin}),
    )
    .await
    .unwrap();
    assert_eq!(snapshot["entries"].as_array().unwrap().len(), 3);
    assert_eq!(snapshot["branches"][0]["leafId"], Value::Null);
    for other in [
        NativeOwner { id: 18, ..owner() },
        NativeOwner {
            generation: "generation-2".into(),
            ..owner()
        },
    ] {
        assert!(call(&ports, other, append(&origin, "journalAppend"))
            .await
            .is_err());
    }
    assert_eq!(
        call(&ports, owner(), json!({"action":"sync", "origin":origin}))
            .await
            .unwrap()["durability"],
        "ephemeral"
    );
    call(
        &ports,
        owner(),
        json!({"action":"release", "origin":origin}),
    )
    .await
    .unwrap();
    assert!(call(&ports, owner(), append(&origin, "journalAppend"))
        .await
        .is_err());
}

#[tokio::test]
async fn child_origins_are_scoped_and_do_not_pin_child_lifetimes() {
    let ports = Arc::new(SessionPorts::default());
    let storage = memory();
    let parent = capture(&ports, storage.clone()).await;
    let child = Arc::new(ToolExecutionScope::default());
    let child_origin = child.run(capture(&ports, storage.clone())).await;
    assert!(call(&ports, owner(), append(&child_origin, "append"))
        .await
        .is_err());
    child
        .run(async {
            assert!(call(&ports, owner(), append(&parent, "append"))
                .await
                .is_err());
            call(&ports, owner(), append(&child_origin, "append"))
                .await
                .unwrap();
        })
        .await;
    let child_weak = Arc::downgrade(&child);
    drop(child);
    assert!(child_weak.upgrade().is_none());
    assert!(call(&ports, owner(), append(&child_origin, "append"))
        .await
        .is_err());
    call(&ports, owner(), append(&parent, "append"))
        .await
        .unwrap();
    let _ = capture(&ports, storage).await;
    assert!(!ports
        .state
        .lock()
        .unwrap()
        .origins
        .contains_key(&child_origin));
}

#[tokio::test]
async fn capture_refuses_stale_identity_and_bounds_handles_and_untrusted_input() {
    let ports = Arc::new(SessionPorts::default());
    let storage = memory();
    let stale = rpi_ai::identity::RequestIdentity::operation(Some("other-session".into()), "test");
    assert!(rpi_ai::identity::scope(
        stale,
        ports.request(
            owner(),
            json!({"action":"capture"}),
            CancellationToken::new(),
            || Ok(storage.clone())
        )
    )
    .await
    .is_err());
    for _ in 0..MAX_OWNER_ORIGINS {
        capture(&ports, storage.clone()).await;
    }
    assert!(ports
        .request(
            owner(),
            json!({"action":"capture"}),
            CancellationToken::new(),
            || Ok(storage.clone())
        )
        .await
        .is_err());
    let origin = ports
        .state
        .lock()
        .unwrap()
        .origins
        .keys()
        .next()
        .unwrap()
        .clone();
    for args in [
        json!({"action":"append","origin":origin,"customType":"","data":{}}),
        json!({"action":"append","origin":origin,"customType":"valid","data":{},"id":"forged"}),
        json!({"action":"append","origin":origin,"customType":"valid","data":"x".repeat(MAX_ENTRY_BYTES)}),
    ] {
        assert!(call(&ports, owner(), args).await.is_err());
    }
    assert!(storage.snapshot().await.unwrap().entries.is_empty());
    ports.retire(owner().id);
    assert!(ports.state.lock().unwrap().origins.is_empty());
    assert!(ports
        .request(
            owner(),
            json!({"action":"capture"}),
            CancellationToken::new(),
            || Ok(storage)
        )
        .await
        .is_err());
}

struct GatedStorage {
    inner: Arc<InMemorySessionStorage>,
    entered: Notify,
    release: Notify,
    fail: bool,
}
#[async_trait::async_trait]
impl SessionStorage for GatedStorage {
    fn metadata(&self) -> SessionMetadata {
        self.inner.metadata()
    }
    async fn commit_entry(
        &self,
        entry: ProvisionedEntry,
        target: AppendTarget,
    ) -> SessionResult<CommittedEntry> {
        self.entered.notify_one();
        self.release.notified().await;
        if self.fail {
            return Err(SessionError::storage("injected fsync failure"));
        }
        self.inner.commit_entry(entry, target).await
    }
    async fn snapshot(&self) -> SessionResult<SessionSnapshot> {
        self.inner.snapshot().await
    }
    async fn sync(&self) -> SessionResult<rpi_tools::Durability> {
        self.inner.sync().await
    }
    async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
        self.inner.get_lanes().await
    }
    async fn create_lane(&self, lane: &str, at: Option<&str>) -> SessionResult<()> {
        self.inner.create_lane(lane, at).await
    }
    async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
        self.inner.move_lane(lane, to).await
    }
    async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
        self.inner.append_entry(entry, lane).await
    }
    async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        self.inner.append_record(record).await
    }
    async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
        self.inner.get_entry(id).await
    }
    async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
        self.inner.find_entries(query).await
    }
    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<Entry>> {
        self.inner
            .find_entries_on_branch(query, bounds, start)
            .await
    }
    async fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        self.inner.find_records(query).await
    }
    async fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> SessionResult<Vec<OperationStartedRecord>> {
        self.inner.find_open_operations(lane, limit).await
    }
    async fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
        self.inner.get_log(options).await
    }
    async fn get_name(&self) -> SessionResult<Option<String>> {
        self.inner.get_name().await
    }
    async fn set_name(&self, name: Option<&str>) -> SessionResult<()> {
        self.inner.set_name(name).await
    }
    async fn get_label(&self, id: &str) -> SessionResult<Option<String>> {
        self.inner.get_label(id).await
    }
    async fn set_label(&self, id: &str, label: Option<&str>) -> SessionResult<()> {
        self.inner.set_label(id, label).await
    }
    async fn get_stats(&self) -> SessionResult<SessionStats> {
        self.inner.get_stats().await
    }
}

#[tokio::test]
async fn cancellation_and_shutdown_wait_for_started_commit_and_failed_sync_never_acknowledges() {
    for fail in [false, true] {
        let ports = Arc::new(SessionPorts::default());
        let storage = Arc::new(GatedStorage {
            inner: memory(),
            entered: Notify::new(),
            release: Notify::new(),
            fail,
        });
        let origin = capture(&ports, storage.clone()).await;
        let signal = CancellationToken::new();
        let pending = tokio::spawn({
            let ports = ports.clone();
            let signal = signal.clone();
            async move {
                ports
                    .request(
                        owner(),
                        append(&origin, "journalAppend"),
                        signal,
                        || panic!(),
                    )
                    .await
            }
        });
        storage.entered.notified().await;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            ports.drain(&[owner().id + 1]),
        )
        .await
        .unwrap();
        if !fail {
            signal.cancel();
            assert!(pending.await.unwrap().is_err());
            ports.retire(owner().id);
            assert!(tokio::time::timeout(
                std::time::Duration::from_millis(10),
                ports.drain(&[owner().id])
            )
            .await
            .is_err());
            storage.release.notify_one();
        } else {
            storage.release.notify_one();
            assert_eq!(
                pending.await.unwrap().unwrap_err(),
                "injected fsync failure"
            );
        }
        ports.drain(&[owner().id]).await;
        assert_eq!(
            storage.inner.snapshot().await.unwrap().entries.len(),
            usize::from(!fail)
        );
    }
}

#[tokio::test]
async fn custom_entries_keep_absent_data_distinct_from_explicit_null() {
    let ports = Arc::new(SessionPorts::default());
    let storage = memory();
    let origin = capture(&ports, storage.clone()).await;
    for present in [false, true] {
        let mut args = json!({"action":"append","origin":origin,"customType":"fixture"});
        if present {
            args["data"] = Value::Null;
        }
        let receipt = call(&ports, owner(), args).await.unwrap();
        assert_eq!(
            receipt["entry"].get("data"),
            if present { Some(&Value::Null) } else { None }
        );
        let entry: Entry = serde_json::from_value(receipt["entry"].clone()).unwrap();
        let Entry::Custom(entry) = entry else {
            panic!("expected custom entry")
        };
        assert_eq!(entry.data, if present { Some(Value::Null) } else { None });
    }
}

#[tokio::test]
async fn dropping_a_queued_waiter_cancels_before_the_append_begins() {
    let ports = Arc::new(SessionPorts::default());
    let storage = memory();
    let origin = capture(&ports, storage.clone()).await;
    let handle = ports.state.lock().unwrap().origins[&origin].clone();
    let blocked = handle.anchor.lock().await;
    let pending = tokio::spawn({
        let ports = ports.clone();
        async move { call(&ports, owner(), append(&origin, "journalAppend")).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !ports.state.lock().unwrap().active.contains_key(&owner().id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    drop(blocked);
    ports.drain(&[owner().id]).await;
    assert!(storage.snapshot().await.unwrap().entries.is_empty());
}
