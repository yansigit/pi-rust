use super::*;
use rpi_extensions::delegation::*;
use std::time::Duration;
#[path = "../../../tests/fixtures/delegation_library.rs"]
mod delegation_library;
struct Resolver(Arc<ProviderRuntime>);
#[async_trait::async_trait]
impl DelegationResolver for Resolver {
    async fn resolve_target(
        &self,
        caller: &DelegationCaller,
        provider: &str,
        model: &str,
        signal: CancellationToken,
    ) -> Result<ResolvedDelegationTarget, String> {
        self.0.resolve_target(caller, provider, model, signal).await
    }
    async fn resolve_ambient(
        &self,
        _: &DelegationCaller,
        _: &ResolvedDelegationTarget,
        _: CancellationToken,
    ) -> Result<ResolvedAmbientAuth, String> {
        Ok(ResolvedAmbientAuth::new(
            rpi_ai::auth::ReplacementAuth::from_private_request("c".into(), Default::default()),
        ))
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loaded_runtime_publication_terminates_active_stream_and_rejects_stale_capabilities() {
    let host = Arc::new(Host::default());
    let bridge = rpi_extensions::ActionBridge::new(tokio::runtime::Handle::current(), host);
    let session = rpi_extensions::load_session_mixed(
        &[],
        &[delegation_library::library().into()],
        Arc::new(rpi_extensions::NullDiagnostics),
        Some(bridge.clone()),
    );
    assert_eq!(session.loaded_paths().len(), 1);
    let providers = rpi_extensions::PluggableProvider::from_session(
        &session,
        tokio::runtime::Handle::current(),
    );
    let runtime = ProviderRuntime::new(&session, &bridge, providers, vec![]).unwrap();
    let epoch = runtime.state.lock().await.epoch.clone();
    let resolver: Arc<dyn DelegationResolver> = Arc::new(Resolver(runtime.clone()));
    let binding = DelegationBinding::root(Arc::downgrade(&resolver), epoch.clone(), "a").unwrap();
    let caller = binding.caller(WrapperInstanceId::fresh()).unwrap();
    let target = runtime
        .resolve_target(&caller, "c", "m", CancellationToken::new())
        .await
        .unwrap();
    let mut capabilities = CapabilityTables::default();
    let target_id = capabilities.insert_target(target).unwrap();
    let receipt = capabilities
        .insert_auth(
            &target_id,
            &caller,
            ReceiptKind::Ambient,
            rpi_ai::auth::ReplacementAuth::from_private_request("c".into(), Default::default()),
        )
        .unwrap();
    let source = runtime.state.lock().await.chains["a"].clone();
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("blocked");
    let mut context = Context::default();
    context.system_prompt = Some("publication|".into());
    let mut options = SimpleStreamOptions::default();
    options.metadata = Some(BTreeMap::from([("marker".into(), json!(marker))]));
    let mut stream = binding
        .scope(source.stream_simple(&source.models()[0], &context, &options))
        .await;
    tokio::time::timeout(Duration::from_secs(8), async {
        while !marker.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let owner = session
        .snapshot()
        .unwrap()
        .provider_owner("c")
        .unwrap()
        .clone();
    let models = runtime.state.lock().await.chains["c"].models().to_vec();
    request(
        &runtime,
        owner,
        json!({"action":"publish","providerId":"c","models":models}),
    )
    .await
    .unwrap();
    assert!(epoch.signal().is_cancelled());
    let mut terminal = None;
    tokio::time::timeout(Duration::from_secs(8), async {
        while let Some(event) = stream.next().await {
            match event {
                rpi_ai::AssistantMessageEvent::Error { error, .. } => {
                    assert!(terminal.replace(error).is_none());
                }
                rpi_ai::AssistantMessageEvent::Done { message, .. } => {
                    assert!(terminal.replace(message).is_none());
                }
                _ => {
                    assert!(terminal.is_none(), "event after terminal");
                }
            }
        }
    })
    .await
    .unwrap();
    let terminal = terminal.unwrap();
    assert_eq!(
        terminal.stop_reason,
        rpi_ai::StopReason::Error,
        "{:?}",
        terminal.error_message
    );
    assert_eq!(
        terminal.error_message.as_deref(),
        Some("delegation epoch revoked")
    );
    assert_eq!(
        capabilities.target(&target_id, &caller).err().unwrap(),
        "delegation epoch revoked"
    );
    assert_eq!(
        capabilities
            .authentication(&receipt, &target_id, &caller)
            .err()
            .unwrap(),
        "delegation epoch revoked"
    );
    let finished = std::path::PathBuf::from(format!("{}.finished", marker.display()));
    tokio::time::timeout(Duration::from_secs(8), async {
        while std::fs::read_to_string(&finished).ok().as_deref() != Some("drained") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for provider in ["a", "b", "c"] {
        let stale = std::path::PathBuf::from(format!("{}.stale.{provider}", marker.display()));
        tokio::time::timeout(Duration::from_secs(8), async {
            while std::fs::read_to_string(&stale).ok().as_deref() != Some("rejected") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    session.begin_shutdown();
}
