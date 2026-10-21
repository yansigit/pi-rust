//! SQLite-backed session repository.
//!
//! The reducer remains the source of truth for session invariants. SQLite
//! stores one metadata row and a serialized reducer snapshot per session,
//! giving callers a durable, queryable repository without duplicating the
//! JSONL reducer logic. Writes are serialized per connection and committed
//! atomically after the in-memory mutation succeeds.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::FutureExt;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::error::{SessionError, SessionResult};
use crate::session::memory::{Clock, InMemorySessionStorage, SystemClock};
use crate::session::state::SessionState;
use crate::session::types::{
    BranchBounds, Entry, EntryQuery, ForkOptions, IdGenerator, LanePointer, LaneRecord, LogItem,
    LogOptions, OperationStartedRecord, ProvisionedEntry, RecordQuery, SessionCreateOptions,
    SessionMetadata, SessionRepo, SessionStats, SessionStorage,
};

#[derive(Clone)]
pub struct SqliteSessionStorage {
    db: Arc<Mutex<Connection>>,
    metadata: SessionMetadata,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
}

impl SqliteSessionStorage {
    fn new(
        db: Arc<Mutex<Connection>>,
        metadata: SessionMetadata,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> Self {
        Self {
            db,
            metadata,
            clock,
            ids,
        }
    }

    fn write<T>(
        &self,
        mutate: impl FnOnce(&InMemorySessionStorage) -> SessionResult<T>,
    ) -> SessionResult<T> {
        let mut db = self
            .db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?;
        let transaction = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| SessionError::storage(format!("begin SQLite mutation: {error}")))?;
        let inner = self.load(&transaction)?;
        let result = mutate(&inner)?;
        let state = serde_json::to_string(&inner.snapshot_state())
            .map_err(|e| SessionError::storage(format!("serialize SQLite session: {e}")))?;
        let changed = transaction
            .execute(
                "UPDATE sessions SET state_json = ?1 WHERE id = ?2",
                params![state, self.metadata.id],
            )
            .map_err(|e| SessionError::storage(format!("persist SQLite session: {e}")))?;
        if changed != 1 {
            return Err(SessionError::not_found(format!(
                "Session not found: {}",
                self.metadata.id
            )));
        }
        transaction
            .commit()
            .map_err(|error| SessionError::storage(format!("commit SQLite mutation: {error}")))?;
        Ok(result)
    }

    fn read<T>(
        &self,
        read: impl FnOnce(&InMemorySessionStorage) -> SessionResult<T>,
    ) -> SessionResult<T> {
        let db = self
            .db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?;
        read(&self.load(&db)?)
    }

    fn load(&self, db: &Connection) -> SessionResult<InMemorySessionStorage> {
        let state_json: String = db
            .query_row(
                "SELECT state_json FROM sessions WHERE id = ?1",
                params![self.metadata.id],
                |row| row.get(0),
            )
            .map_err(|e| SessionError::storage(format!("refresh SQLite session: {e}")))?;
        let state = serde_json::from_str(&state_json)
            .map_err(|e| SessionError::storage(format!("decode SQLite session: {e}")))?;
        Ok(InMemorySessionStorage::from_state(
            self.metadata.clone(),
            state,
            self.clock.clone(),
            self.ids.clone(),
        ))
    }
}

// Memory operations do no I/O; never hold a SQLite transaction across an await.
fn immediate<T>(
    operation: impl std::future::Future<Output = SessionResult<T>>,
) -> SessionResult<T> {
    operation
        .now_or_never()
        .ok_or_else(|| SessionError::storage("in-memory reducer unexpectedly yielded"))?
}

#[async_trait]
impl SessionStorage for SqliteSessionStorage {
    async fn snapshot(&self) -> SessionResult<super::types::SessionSnapshot> {
        self.read(|inner| immediate(inner.snapshot()))
    }

    async fn commit_entry(
        &self,
        entry: ProvisionedEntry,
        target: super::types::AppendTarget,
    ) -> SessionResult<super::types::CommittedEntry> {
        let mut receipt = self.write(|inner| immediate(inner.commit_entry(entry, target)))?;
        receipt.durability = self.sync().await?;
        Ok(receipt)
    }

    async fn sync(&self) -> SessionResult<rpi_tools::Durability> {
        let db = self
            .db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?;
        self.load(&db)?;
        match db.path() {
            Some("") => Ok(rpi_tools::Durability::Ephemeral),
            Some(_) => Ok(rpi_tools::Durability::Durable),
            None => Err(SessionError::storage(
                "SQLite persistence location is unavailable",
            )),
        }
    }

    fn metadata(&self) -> SessionMetadata {
        self.metadata.clone()
    }
    async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
        self.read(|inner| immediate(inner.get_lanes()))
    }
    async fn create_lane(&self, lane: &str, at: Option<&str>) -> SessionResult<()> {
        self.write(|inner| immediate(inner.create_lane(lane, at)))
    }
    async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
        self.write(|inner| immediate(inner.move_lane(lane, to)))
    }
    async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
        self.write(|inner| immediate(inner.append_entry(entry, lane)))
    }
    async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        self.write(|inner| immediate(inner.append_record(record)))
    }
    async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
        self.read(|inner| immediate(inner.get_entry(id)))
    }
    async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
        self.read(|inner| immediate(inner.find_entries(query)))
    }
    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<Entry>> {
        self.read(|inner| immediate(inner.find_entries_on_branch(query, bounds, start)))
    }
    async fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        self.read(|inner| immediate(inner.find_records(query)))
    }
    async fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> SessionResult<Vec<OperationStartedRecord>> {
        self.read(|inner| immediate(inner.find_open_operations(lane, limit)))
    }
    async fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
        self.read(|inner| immediate(inner.get_log(options)))
    }
    async fn get_name(&self) -> SessionResult<Option<String>> {
        self.read(|inner| immediate(inner.get_name()))
    }
    async fn set_name(&self, name: Option<&str>) -> SessionResult<()> {
        self.write(|inner| immediate(inner.set_name(name)))
    }
    async fn get_label(&self, id: &str) -> SessionResult<Option<String>> {
        self.read(|inner| immediate(inner.get_label(id)))
    }
    async fn set_label(&self, id: &str, label: Option<&str>) -> SessionResult<()> {
        self.write(|inner| immediate(inner.set_label(id, label)))
    }
    async fn get_stats(&self) -> SessionResult<SessionStats> {
        self.read(|inner| immediate(inner.get_stats()))
    }
}

pub struct SqliteSessionRepo {
    path: PathBuf,
    db: Arc<Mutex<Connection>>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
}

impl SqliteSessionRepo {
    pub fn open(path: impl AsRef<Path>) -> SessionResult<Self> {
        Self::with_clock_ids(
            path,
            Arc::new(SystemClock),
            Arc::new(crate::session::DefaultIdGenerator::new()),
        )
    }

    pub fn with_clock_ids(
        path: impl AsRef<Path>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> SessionResult<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| SessionError::storage(format!("create SQLite parent: {e}")))?;
        }
        let conn = Connection::open(&path)
            .map_err(|e| SessionError::storage(format!("open SQLite database: {e}")))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 created_at INTEGER NOT NULL,
                 parent_session_id TEXT,
                 state_json TEXT NOT NULL
             );",
        )
        .map_err(|e| SessionError::storage(format!("initialize SQLite database: {e}")))?;
        Ok(Self {
            path,
            db: Arc::new(Mutex::new(conn)),
            clock,
            ids,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn insert(&self, metadata: &SessionMetadata, state: &SessionState) -> SessionResult<()> {
        let state = serde_json::to_string(state)
            .map_err(|e| SessionError::storage(format!("serialize SQLite state: {e}")))?;
        self.db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?
            .execute(
                "INSERT INTO sessions (id, created_at, parent_session_id, state_json) VALUES (?1, ?2, ?3, ?4)",
                params![metadata.id, metadata.created_at, metadata.parent_session_id, state],
            )
            .map_err(|e| match e {
                rusqlite::Error::SqliteFailure(err, _)
                    if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    SessionError::already_exists(format!("Session already exists: {}", metadata.id))
                }
                other => SessionError::storage(format!("insert SQLite session: {other}")),
            })?;
        Ok(())
    }

    fn load_row(&self, id: &str) -> SessionResult<Option<(SessionMetadata, SessionState)>> {
        let db = self
            .db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?;
        db.query_row(
            "SELECT id, created_at, parent_session_id, state_json FROM sessions WHERE id = ?1",
            params![id],
            |row| {
                let metadata = SessionMetadata {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    parent_session_id: row.get(2)?,
                };
                let state_json: String = row.get(3)?;
                let state = serde_json::from_str(&state_json).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                Ok((metadata, state))
            },
        )
        .optional()
        .map_err(|e| SessionError::storage(format!("read SQLite session: {e}")))
    }
}

#[async_trait]
impl SessionRepo for SqliteSessionRepo {
    type Storage = SqliteSessionStorage;

    async fn create(&self, options: &SessionCreateOptions) -> SessionResult<Self::Storage> {
        let metadata = SessionMetadata {
            id: options.id.clone().unwrap_or_else(|| self.ids.next()),
            created_at: self.clock.now_ms(),
            parent_session_id: options.parent_session_id.clone(),
        };
        let state = SessionState::new();
        self.insert(&metadata, &state)?;
        Ok(SqliteSessionStorage::new(
            self.db.clone(),
            metadata,
            self.clock.clone(),
            self.ids.clone(),
        ))
    }

    async fn open(&self, metadata: &SessionMetadata) -> SessionResult<Self::Storage> {
        let (metadata, _) = self.load_row(&metadata.id)?.ok_or_else(|| {
            SessionError::not_found(format!("Session not found: {}", metadata.id))
        })?;
        Ok(SqliteSessionStorage::new(
            self.db.clone(),
            metadata,
            self.clock.clone(),
            self.ids.clone(),
        ))
    }

    async fn list(&self) -> SessionResult<Vec<SessionMetadata>> {
        let db = self
            .db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?;
        let mut stmt = db
            .prepare(
                "SELECT id, created_at, parent_session_id FROM sessions ORDER BY created_at DESC",
            )
            .map_err(|e| SessionError::storage(format!("list SQLite sessions: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SessionMetadata {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    parent_session_id: row.get(2)?,
                })
            })
            .map_err(|e| SessionError::storage(format!("list SQLite sessions: {e}")))?;
        rows.map(|row| row.map_err(|e| SessionError::storage(format!("read SQLite metadata: {e}"))))
            .collect()
    }

    async fn delete(&self, metadata: &SessionMetadata) -> SessionResult<()> {
        self.db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?
            .execute("DELETE FROM sessions WHERE id = ?1", params![metadata.id])
            .map_err(|e| SessionError::storage(format!("delete SQLite session: {e}")))?;
        Ok(())
    }

    async fn fork(
        &self,
        source: &SessionMetadata,
        options: &SessionCreateOptions,
        fork: &ForkOptions,
    ) -> SessionResult<Self::Storage> {
        let source_storage = self.open(source).await?;
        let id = options.id.clone().unwrap_or_else(|| self.ids.next());
        let metadata = SessionMetadata {
            id,
            created_at: self.clock.now_ms(),
            parent_session_id: Some(source.id.clone()),
        };
        let forked = source_storage.read(|inner| {
            inner.fork_from(metadata.clone(), fork, self.clock.clone(), self.ids.clone())
        })?;
        let state = forked.snapshot_state();
        self.insert(&metadata, &state)?;
        Ok(SqliteSessionStorage::new(
            self.db.clone(),
            metadata,
            self.clock.clone(),
            self.ids.clone(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::memory::{CounterIdGenerator, FakeClock};
    use crate::session::types::{EntryQuery, ForkOptions, ProvisionedKind};
    use rpi_agent::message::AgentMessage;

    fn note(id: impl Into<String>) -> ProvisionedEntry {
        ProvisionedEntry {
            id: id.into(),
            kind: ProvisionedKind::Custom {
                custom_type: "durability-test".into(),
                data: None,
            },
        }
    }

    #[test]
    fn separate_sqlite_connections_do_not_lose_concurrent_committed_entries() {
        let (repo, _dir) = repo();
        let session =
            futures::executor::block_on(repo.create(&SessionCreateOptions::default())).unwrap();
        let mut stores = Vec::new();
        for _ in 0..4 {
            let other = SqliteSessionRepo::open(repo.path()).unwrap();
            stores.push(futures::executor::block_on(other.open(&session.metadata())).unwrap());
        }
        let barrier = Arc::new(std::sync::Barrier::new(stores.len()));
        let workers: Vec<_> = stores
            .into_iter()
            .enumerate()
            .map(|(worker, store)| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for index in 0..30 {
                        futures::executor::block_on(
                            store.append_entry(note(format!("{worker}-{index}")), "main"),
                        )
                        .unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let entries =
            futures::executor::block_on(session.find_entries(&EntryQuery::default())).unwrap();
        assert_eq!(entries.len(), 120);
        let mut entries = entries;
        entries.sort_by_key(Entry::seq);
        for (index, entry) in entries.iter().enumerate() {
            assert_eq!(entry.seq(), index as u64 + 1);
            assert_eq!(
                entry.parent_id(),
                index.checked_sub(1).map(|previous| entries[previous].id())
            );
        }
    }

    #[tokio::test]
    async fn sqlite_failed_commit_keeps_the_previous_state_and_acknowledges_only_durable_storage() {
        let (repo, _dir) = repo();
        let session = repo.create(&SessionCreateOptions::default()).await.unwrap();
        session.append_entry(note("before"), "main").await.unwrap();
        {
            let db = repo.db.lock().unwrap();
            assert_eq!(
                db.query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                2
            );
            db.execute_batch("CREATE TRIGGER fail_write BEFORE UPDATE ON sessions BEGIN SELECT RAISE(FAIL, 'injected commit failure'); END;").unwrap();
        }
        assert!(session.append_entry(note("failed"), "main").await.is_err());
        assert!(session.get_entry("failed").await.unwrap().is_none());
        assert_eq!(
            session.get_lanes().await.unwrap()[0].leaf_id.as_deref(),
            Some("before")
        );
        repo.db
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_write;")
            .unwrap();
        let committed = session.append_entry(note("after"), "main").await.unwrap();
        assert_eq!(committed.seq(), 2);
        assert_eq!(
            session.sync().await.unwrap(),
            rpi_tools::Durability::Durable
        );
        let memory = SqliteSessionRepo::open(":memory:").unwrap();
        let ephemeral = memory
            .create(&SessionCreateOptions::default())
            .await
            .unwrap();
        ephemeral
            .append_entry(note("memory"), "main")
            .await
            .unwrap();
        assert_eq!(
            ephemeral.sync().await.unwrap(),
            rpi_tools::Durability::Ephemeral
        );
        repo.delete(&session.metadata()).await.unwrap();
        assert!(session.sync().await.is_err());
    }

    fn repo() -> (SqliteSessionRepo, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let repo = SqliteSessionRepo::with_clock_ids(
            dir.path().join("sessions.db"),
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .unwrap();
        (repo, dir)
    }

    #[tokio::test]
    async fn round_trip_append_and_fork() {
        let (repo, _dir) = repo();
        let session = repo
            .create(&SessionCreateOptions {
                id: Some("root".into()),
                parent_session_id: None,
                metadata: None,
            })
            .await
            .unwrap();
        session
            .append_entry(
                ProvisionedEntry {
                    id: "m1".into(),
                    kind: ProvisionedKind::Message {
                        message: AgentMessage::from(rpi_ai::types::UserMessage::new("hello", 1)),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        let reopened = repo.open(&session.metadata()).await.unwrap();
        assert_eq!(
            reopened
                .find_entries(&EntryQuery::default())
                .await
                .unwrap()
                .len(),
            1
        );
        let second_handle = repo.open(&session.metadata()).await.unwrap();
        second_handle
            .append_entry(
                ProvisionedEntry {
                    id: "m2".into(),
                    kind: ProvisionedKind::Message {
                        message: AgentMessage::from(rpi_ai::types::UserMessage::new("world", 2)),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        assert_eq!(
            session
                .find_entries(&EntryQuery::default())
                .await
                .unwrap()
                .len(),
            2
        );
        let fork = repo
            .fork(
                &session.metadata(),
                &SessionCreateOptions {
                    id: Some("child".into()),
                    parent_session_id: None,
                    metadata: None,
                },
                &ForkOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(fork.metadata().parent_session_id.as_deref(), Some("root"));
        assert_eq!(repo.list().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn duplicate_id_is_already_exists_and_delete_works() {
        let (repo, _dir) = repo();
        repo.create(&SessionCreateOptions {
            id: Some("same".into()),
            parent_session_id: None,
            metadata: None,
        })
        .await
        .unwrap();
        let err = match repo
            .create(&SessionCreateOptions {
                id: Some("same".into()),
                parent_session_id: None,
                metadata: None,
            })
            .await
        {
            Ok(_) => panic!("expected duplicate"),
            Err(e) => e,
        };
        assert_eq!(err.code, crate::error::SessionErrorCode::AlreadyExists);
        repo.delete(&SessionMetadata {
            id: "same".into(),
            created_at: 0,
            parent_session_id: None,
        })
        .await
        .unwrap();
        assert!(repo.list().await.unwrap().is_empty());
    }
}
