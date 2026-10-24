use super::delegation_v2::*;
use rpi_ai::{SimpleStreamOptions, StopReason};
use rpi_extensions::delegation::*;
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loaded_parent_child_capabilities_cannot_be_used_or_released_in_either_direction() {
    let fixture = Fixture::new();
    let directory = tempfile::tempdir().unwrap();
    let options = marker_options(&directory.path().join("capabilities"));
    let message = tokio::time::timeout(Duration::from_secs(8), async {
        fixture
            .stream("transfer", &options)
            .await
            .result()
            .await
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(message.stop_reason, StopReason::Stop, "{message:?}");
    // Fresh execution scope/epoch and continuation, same actual loaded library.
    let other = Fixture::new();
    let message = tokio::time::timeout(Duration::from_secs(8), async {
        other
            .stream("scope", &options)
            .await
            .result()
            .await
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(message.stop_reason, StopReason::Stop, "{message:?}");
    other.session.begin_shutdown();
    fixture.session.begin_shutdown();
}
fn marker_options(marker: &std::path::Path) -> SimpleStreamOptions {
    let mut options = SimpleStreamOptions::default();
    options.metadata = Some(std::collections::BTreeMap::from([(
        "marker".into(),
        serde_json::json!(marker),
    )]));
    options
}
struct PendingResolver {
    epoch: Arc<DelegationEpoch>,
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
impl DelegationResolver for PendingResolver {
    async fn resolve_target(
        &self,
        caller: &DelegationCaller,
        provider: &str,
        model: &str,
        _: CancellationToken,
    ) -> Result<ResolvedDelegationTarget, String> {
        self.epoch.target(caller, provider, model)
    }
    async fn resolve_ambient(
        &self,
        _: &DelegationCaller,
        _: &ResolvedDelegationTarget,
        _: CancellationToken,
    ) -> Result<ResolvedAmbientAuth, String> {
        let _dropped = Dropped(self.dropped.clone());
        self.entered.notify_one();
        std::future::pending().await
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loaded_unload_cancels_pending_target_auth_and_drops_the_future() {
    let mut fixture = Fixture::new();
    let entered = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(tokio::sync::Notify::new());
    fixture.resolver = Arc::new(PendingResolver {
        epoch: fixture.epoch.clone(),
        entered: entered.clone(),
        dropped: dropped.clone(),
    });
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("pending");
    let stream = fixture.stream("pending", &marker_options(&marker)).await;
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
    assert!(!std::path::PathBuf::from(format!("{}.resolved", marker.display())).exists());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loaded_c_emit_drains_more_than_four_mib_in_order() {
    let fixture = Fixture::new();
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("bytes");
    let mut stream = fixture.stream("bytes", &marker_options(&marker)).await;
    wait_file(&marker).await;
    assert!(!std::path::PathBuf::from(format!("{}.finished", marker.display())).exists());
    let mut index = 0;
    let mut terminals = 0;
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(event) = stream.next().await {
            match event {
                rpi_ai::AssistantMessageEvent::TextDelta { delta, .. } => {
                    assert_eq!(delta, format!("{index:04},{}", "x".repeat(8192)));
                    index += 1;
                }
                rpi_ai::AssistantMessageEvent::Done { message, .. } => {
                    assert_eq!(message.stop_reason, StopReason::Stop);
                    terminals += 1;
                }
                rpi_ai::AssistantMessageEvent::Error { error, .. } => panic!("{error:?}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(index, 1024);
    assert_eq!(terminals, 1);
    fixture.session.begin_shutdown();
}
