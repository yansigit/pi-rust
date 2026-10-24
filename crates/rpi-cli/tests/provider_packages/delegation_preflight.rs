use super::delegation_v2::*;
use rpi_ai::{Context, Model, Provider, SimpleStreamOptions, StopReason};
use rpi_extensions::delegation::*;
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
struct PendingPreflight {
    inner: Arc<dyn Provider>,
    entered: Arc<tokio::sync::Notify>,
    dropped: Arc<tokio::sync::Notify>,
}
struct Dropped(Arc<tokio::sync::Notify>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}
#[async_trait::async_trait]
impl Provider for PendingPreflight {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn models(&self) -> &[Model] {
        self.inner.models()
    }
    async fn preflight(&self, _: &Model, _: CancellationToken) -> Result<(), String> {
        let _guard = Dropped(self.dropped.clone());
        self.entered.notify_one();
        std::future::pending().await
    }
    async fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> rpi_ai::AssistantMessageEventStream {
        self.inner.stream_simple(model, context, options).await
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loaded_unload_cancels_pending_target_preflight_and_drops_the_future() {
    let mut fixture = Fixture::new();
    let entered = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(tokio::sync::Notify::new());
    let chains = fixture.chains.iter().map(|chain| {
        if chain.id() == "b" {
            Arc::new(PendingPreflight {
                inner: chain.clone(),
                entered: entered.clone(),
                dropped: dropped.clone(),
            }) as Arc<dyn Provider>
        } else {
            chain.clone()
        }
    });
    fixture.epoch = DelegationEpoch::new(DelegationScopeId::fresh(), chains);
    fixture.resolver = Arc::new(Resolver(fixture.epoch.clone()));
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("preflight");
    let mut options = SimpleStreamOptions::default();
    options.metadata = Some(std::collections::BTreeMap::from([(
        "marker".into(),
        serde_json::json!(marker),
    )]));
    let stream = fixture.stream("pending-preflight", &options).await;
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    fixture.session.begin_shutdown();
    let message = tokio::time::timeout(Duration::from_secs(5), stream.result())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        message.stop_reason,
        StopReason::Aborted | StopReason::Error
    ));
    wait_file(&std::path::PathBuf::from(format!(
        "{}.finished",
        marker.display()
    )))
    .await;
    assert_eq!(
        std::fs::read_to_string(format!("{}.finished", marker.display())).unwrap(),
        "authentication cancelled"
    );
    tokio::time::timeout(Duration::from_secs(5), dropped.notified())
        .await
        .unwrap();
}
