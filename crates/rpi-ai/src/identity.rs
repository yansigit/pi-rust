//! Host-assigned correlation for a run, its turns, and provider invocations.
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestIdentity {
    pub session_id: Option<String>,
    pub operation_id: String,
    pub purpose: String,
    pub turn_id: Option<String>,
    pub request_id: Option<String>,
}
impl RequestIdentity {
    pub fn operation(session_id: Option<String>, purpose: impl Into<String>) -> Self {
        Self {
            session_id,
            operation_id: uuid::Uuid::new_v4().to_string(),
            purpose: purpose.into(),
            turn_id: None,
            request_id: None,
        }
    }
    pub fn next_turn(&mut self) {
        self.turn_id = Some(uuid::Uuid::new_v4().to_string());
        self.request_id = None;
    }
    pub fn next_request(&mut self) {
        self.request_id = Some(uuid::Uuid::new_v4().to_string());
    }
}

pub struct Operation(Mutex<RequestIdentity>);
tokio::task_local! { static CURRENT: Arc<Operation>; }
pub fn operation() -> Option<Arc<Operation>> {
    CURRENT.try_with(Clone::clone).ok()
}
pub async fn inherit<F: Future>(operation: Option<Arc<Operation>>, future: F) -> F::Output {
    match operation {
        Some(operation) => CURRENT.scope(operation, future).await,
        None => future.await,
    }
}
pub async fn scope_if_absent<F: Future>(identity: RequestIdentity, future: F) -> F::Output {
    if operation().is_some() {
        future.await
    } else {
        scope(identity, future).await
    }
}

pub async fn scope<F: Future>(identity: RequestIdentity, future: F) -> F::Output {
    CURRENT
        .scope(Arc::new(Operation(Mutex::new(identity))), future)
        .await
}
pub fn current() -> Option<RequestIdentity> {
    CURRENT
        .try_with(|identity| identity.0.lock().unwrap().clone())
        .ok()
}
pub fn next_turn() {
    let _ = CURRENT.try_with(|identity| identity.0.lock().unwrap().next_turn());
}
pub fn next_request() -> Option<RequestIdentity> {
    CURRENT
        .try_with(|identity| {
            let mut identity = identity.0.lock().unwrap();
            identity.next_request();
            identity.clone()
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn concurrent_runs_and_nested_operations_have_separate_identities() {
        let a = RequestIdentity::operation(Some("same-session".into()), "prompt");
        let b = RequestIdentity::operation(Some("same-session".into()), "prompt");
        assert_ne!(a.operation_id, b.operation_id);
        let (left, right) = tokio::join!(
            scope(a.clone(), async {
                next_turn();
                let first = next_request().unwrap();
                let retry = next_request().unwrap();
                assert_eq!(first.turn_id, retry.turn_id);
                assert_ne!(first.request_id, retry.request_id);
                scope(RequestIdentity::operation(None, "compaction"), async {
                    assert_eq!(current().unwrap().purpose, "compaction");
                })
                .await;
                assert_eq!(current().unwrap(), retry);
                next_turn();
                let second = next_request().unwrap();
                assert_ne!(first.turn_id, second.turn_id);
                second
            }),
            scope(b.clone(), async {
                tokio::task::yield_now().await;
                next_turn();
                next_request().unwrap()
            })
        );
        assert_eq!(left.operation_id, a.operation_id);
        assert_eq!(right.operation_id, b.operation_id);
        assert!(current().is_none());
    }
}
