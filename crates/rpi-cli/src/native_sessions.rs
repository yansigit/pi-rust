//! Private session handles freeze storage ownership across branch/session changes.
use rpi_agent::tool_execution_scope::{current_scope, ToolExecutionScope};
use rpi_extensions::native::NativeOwner;
use rpi_harness::session::types::{
    AppendTarget, ProvisionedEntry, ProvisionedKind, SessionStorage,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const MAX_ORIGINS: usize = 1024;
const MAX_OWNER_ORIGINS: usize = 128;
const MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum Request {
    Capture {
        #[serde(default = "main_lane")]
        lane: String,
    },
    Snapshot {
        origin: String,
    },
    Append {
        origin: String,
        custom_type: String,
        #[serde(default, deserialize_with = "rpi_ai::types::deserialize_present_json")]
        data: Option<Value>,
    },
    JournalAppend {
        origin: String,
        custom_type: String,
        #[serde(default, deserialize_with = "rpi_ai::types::deserialize_present_json")]
        data: Option<Value>,
    },
    Sync {
        origin: String,
    },
    Release {
        origin: String,
    },
}
fn main_lane() -> String {
    "main".into()
}

struct Origin {
    owner: NativeOwner,
    scope: Option<Weak<ToolExecutionScope>>,
    storage: Arc<dyn SessionStorage>,
    lane: String,
    branch_revision: u64,
    anchor: tokio::sync::Mutex<Option<String>>,
}
impl Origin {
    fn alive(&self) -> bool {
        self.scope.as_ref().map_or(true, |scope| {
            scope
                .upgrade()
                .is_some_and(|scope| !scope.stopped().is_cancelled())
        })
    }
    fn matches(&self, owner: &NativeOwner, scope: &Option<Arc<ToolExecutionScope>>) -> bool {
        self.owner == *owner
            && self.alive()
            && match (&self.scope, scope) {
                (None, None) => true,
                (Some(expected), Some(scope)) => expected.ptr_eq(&Arc::downgrade(scope)),
                _ => false,
            }
    }
}
#[derive(Default)]
struct State {
    origins: HashMap<String, Arc<Origin>>,
    retired: HashSet<u64>,
    active: HashMap<u64, usize>,
}
#[derive(Default)]
pub(crate) struct SessionPorts {
    state: Mutex<State>,
    idle: Notify,
}
struct Active(Arc<SessionPorts>, u64);
impl Drop for Active {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        let active = state
            .active
            .get_mut(&self.1)
            .expect("admitted session request");
        *active -= 1;
        if *active == 0 {
            state.active.remove(&self.1);
        }
        self.0.idle.notify_waiters();
    }
}
impl SessionPorts {
    pub(crate) fn retire(&self, owner: u64) {
        let mut state = self.state.lock().unwrap();
        state.retired.insert(owner);
        state.origins.retain(|_, origin| origin.owner.id != owner);
    }
    pub(crate) async fn drain(&self, owners: &[u64]) {
        loop {
            let idle = self.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if owners
                .iter()
                .all(|owner| !self.state.lock().unwrap().active.contains_key(owner))
            {
                return;
            }
            idle.await;
        }
    }
    pub(crate) async fn request(
        self: &Arc<Self>,
        owner: NativeOwner,
        mut args: Value,
        signal: CancellationToken,
        current_storage: impl FnOnce() -> Result<Arc<dyn SessionStorage>, String>,
    ) -> Result<Value, String> {
        let signal = signal.child_token();
        let _cancel = signal.clone().drop_guard();
        let object = args.as_object_mut().ok_or("invalid session request")?;
        object.remove("version");
        object.remove("port");
        let request: Request =
            serde_json::from_value(args).map_err(|_| "invalid session request")?;
        let scope = current_scope();
        check_signal(&signal, &scope)?;
        let storage = if matches!(request, Request::Capture { .. }) {
            let storage = current_storage()?;
            if rpi_ai::identity::current()
                .and_then(|identity| identity.session_id)
                .is_some_and(|id| id != storage.metadata().id)
            {
                return Err("session changed before origin capture".into());
            }
            Some(storage)
        } else {
            None
        };
        let active = {
            let mut state = self.state.lock().unwrap();
            if state.retired.contains(&owner.id) {
                return Err("session owner stopped".into());
            }
            *state.active.entry(owner.id).or_default() += 1;
            Active(self.clone(), owner.id)
        };
        let ports = self.clone();
        let work_signal = signal.clone();
        let task = tokio::spawn(async move {
            let _active = active;
            ports
                .execute(owner, request, scope, work_signal, storage)
                .await
        });
        // Once an append begins, dropping/cancelling the waiter cannot discard its commit.
        tokio::select! {
            biased;
            _ = signal.cancelled() => Err("session request cancelled; inspect the journal before retrying".into()),
            result = task => result.map_err(|_| "session task failed; inspect the journal before retrying")?,
        }
    }
    async fn execute(
        &self,
        owner: NativeOwner,
        request: Request,
        scope: Option<Arc<ToolExecutionScope>>,
        signal: CancellationToken,
        storage: Option<Arc<dyn SessionStorage>>,
    ) -> Result<Value, String> {
        check_signal(&signal, &scope)?;
        if let Request::Capture { lane } = request {
            let storage = storage.ok_or("missing session storage")?;
            let snapshot = storage.snapshot().await.map_err(|e| e.to_string())?;
            let branch = snapshot
                .branches
                .iter()
                .find(|branch| branch.lane == lane)
                .ok_or("unknown session lane")?;
            check_signal(&signal, &scope)?;
            let mut state = self.state.lock().unwrap();
            state.origins.retain(|_, origin| origin.alive());
            if state.retired.contains(&owner.id) {
                return Err("session owner stopped".into());
            }
            if state.origins.len() >= MAX_ORIGINS
                || state
                    .origins
                    .values()
                    .filter(|origin| origin.owner == owner)
                    .count()
                    >= MAX_OWNER_ORIGINS
            {
                return Err("session origin limit reached; release unused origins".into());
            }
            let id = uuid::Uuid::new_v4().to_string();
            state.origins.insert(
                id.clone(),
                Arc::new(Origin {
                    owner,
                    scope: scope.as_ref().map(Arc::downgrade),
                    storage,
                    lane,
                    branch_revision: branch.revision,
                    anchor: tokio::sync::Mutex::new(branch.leaf_id.clone()),
                }),
            );
            return Ok(json!({"origin":id, "snapshot":snapshot}));
        }
        let id = match &request {
            Request::Capture { .. } => unreachable!(),
            Request::Snapshot { origin }
            | Request::Append { origin, .. }
            | Request::JournalAppend { origin, .. }
            | Request::Sync { origin }
            | Request::Release { origin } => origin,
        };
        let origin = {
            let mut state = self.state.lock().unwrap();
            if state.retired.contains(&owner.id) {
                return Err("session owner stopped".into());
            }
            let origin = state
                .origins
                .get(id)
                .filter(|origin| origin.matches(&owner, &scope))
                .cloned()
                .ok_or("expired or foreign session origin")?;
            if matches!(request, Request::Release { .. }) {
                state.origins.remove(id);
                return Ok(Value::Null);
            }
            origin
        };
        // Serialize journal writes so detached late entries keep their own parent chain.
        let mut anchor = origin.anchor.lock().await;
        check_signal(&signal, &scope)?;
        if self.state.lock().unwrap().retired.contains(&owner.id) {
            return Err("session owner stopped".into());
        }
        let journal = matches!(request, Request::JournalAppend { .. });
        match request {
            Request::Snapshot { .. } => {
                serde_json::to_value(origin.storage.snapshot().await.map_err(|e| e.to_string())?)
                    .map_err(|_| "invalid session snapshot".into())
            }
            Request::Sync { .. } => {
                Ok(json!({"durability":origin.storage.sync().await.map_err(|e| e.to_string())?}))
            }
            Request::Append {
                custom_type, data, ..
            }
            | Request::JournalAppend {
                custom_type, data, ..
            } => {
                let target = if journal {
                    AppendTarget::Origin {
                        lane: origin.lane.clone(),
                        branch_revision: origin.branch_revision,
                        anchor_id: anchor.clone(),
                    }
                } else {
                    AppendTarget::Current {
                        lane: origin.lane.clone(),
                        branch_revision: origin.branch_revision,
                    }
                };
                if custom_type.is_empty()
                    || custom_type.len() > 256
                    || custom_type.chars().any(char::is_control)
                    || serde_json::to_vec(&data)
                        .map_err(|_| "invalid session entry")?
                        .len()
                        > MAX_ENTRY_BYTES
                {
                    return Err("invalid or oversized session entry".into());
                }
                let entry = ProvisionedEntry {
                    id: uuid::Uuid::new_v4().to_string(),
                    kind: ProvisionedKind::Custom { custom_type, data },
                };
                let receipt = origin
                    .storage
                    .commit_entry(entry, target)
                    .await
                    .map_err(|e| e.to_string())?;
                *anchor = Some(receipt.entry.id().to_owned());
                serde_json::to_value(receipt).map_err(|_| "invalid session commit receipt".into())
            }
            _ => unreachable!(),
        }
    }
}
fn check_signal(
    signal: &CancellationToken,
    scope: &Option<Arc<ToolExecutionScope>>,
) -> Result<(), String> {
    if signal.is_cancelled()
        || scope
            .as_ref()
            .is_some_and(|scope| scope.stopped().is_cancelled())
    {
        Err("session request cancelled".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
