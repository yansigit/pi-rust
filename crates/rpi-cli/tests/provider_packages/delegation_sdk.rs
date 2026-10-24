use super::delegation_v2::{delegation_library, wait_file, Resolver};
use rpi_ai::{Context, SimpleStreamOptions, StopReason};
use rpi_extensions::delegation::*;
use std::{sync::Arc, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loaded_sdk_adapters_forward_release_and_propagate_caller_cancellation() {
    let directory = tempfile::tempdir().unwrap();
    let session = rpi_extensions::load_session_mixed(
        &[],
        &[
            delegation_library::library().into(),
            delegation_library::sdk_library().into(),
        ],
        Arc::new(rpi_extensions::NullDiagnostics),
        None,
    );
    assert_eq!(session.loaded_paths().len(), 2);
    let bases = rpi_extensions::PluggableProvider::from_session(
        &session,
        tokio::runtime::Handle::current(),
    );
    let chains = rpi_extensions::wrap_providers(&session, bases);
    let epoch = DelegationEpoch::new(DelegationScopeId::fresh(), chains.clone());
    let resolver: Arc<dyn DelegationResolver> = Arc::new(Resolver(epoch.clone()));
    let source = chains.iter().find(|p| p.id() == "sdk-source").unwrap();
    for cancel in [false, true] {
        let marker = directory
            .path()
            .join(if cancel { "cancel" } else { "success" });
        let options = SimpleStreamOptions {
            metadata: Some(std::collections::BTreeMap::from([(
                "marker".into(),
                serde_json::json!(marker),
            )])),
            ..Default::default()
        };
        let context = Context {
            system_prompt: Some(if cancel { "flood|" } else { "sdk|" }.into()),
            ..Default::default()
        };
        let binding =
            DelegationBinding::root(Arc::downgrade(&resolver), epoch.clone(), "sdk-source")
                .unwrap();
        let mut stream = binding
            .scope(source.stream_simple(&source.models()[0], &context, &options))
            .await;
        if cancel {
            // An actual forwarded source event, not just provider entry.
            assert!(tokio::time::timeout(Duration::from_secs(15), stream.next())
                .await
                .unwrap()
                .is_some());
            options.signal.cancel();
            wait_file(&marker.with_extension("cancelled")).await;
            wait_file(&marker.with_extension("nonzero")).await;
            wait_file(&marker.with_extension("finished")).await;
            let result = tokio::time::timeout(Duration::from_secs(15), stream.result())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result.stop_reason, StopReason::Aborted);
        } else {
            let mut events = Vec::new();
            while let Some(event) = tokio::time::timeout(Duration::from_secs(15), stream.next())
                .await
                .unwrap()
            {
                events.push(event);
            }
            assert!(events.iter().any(|event| matches!(event, rpi_ai::AssistantMessageEvent::TextDelta { delta, .. } if delta == "sdk-forwarded-delta")));
            assert_eq!(events.iter().filter(|e| e.is_terminal()).count(), 1);
            assert_eq!(
                events.last().unwrap().partial().stop_reason,
                StopReason::Stop
            );
            assert!(serde_json::to_string(&events.last().unwrap().partial())
                .unwrap()
                .contains("base:c"));
            wait_file(&marker.with_extension("released")).await;
        }
    }
    session.begin_shutdown();
}
