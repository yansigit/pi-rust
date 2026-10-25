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
    let runtime =
        ProviderRuntime::with_config(&session, &bridge, providers, vec![], Default::default())
            .unwrap();
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

// Supply only synthetic ambient auth, without consulting HOME or a real store.
// Streaming and wrapper execution still use the independently loaded DLL.
struct PublicTestProvider(Arc<dyn Provider>);
#[async_trait::async_trait]
impl Provider for PublicTestProvider {
    fn id(&self) -> &str {
        self.0.id()
    }
    fn models(&self) -> &[Model] {
        self.0.models()
    }
    fn with_catalog(&self, models: Vec<Model>) -> Option<Arc<dyn Provider>> {
        Some(Arc::new(Self(self.0.with_catalog(models)?)))
    }
    async fn delegation_auth(
        &self,
        _: &Model,
        _: CancellationToken,
    ) -> Result<rpi_ai::auth::AttemptAuth, String> {
        Ok(rpi_ai::auth::ReplacementAuth::from_private_request(
            self.id().into(),
            Default::default(),
        ))
    }
    async fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.0.stream_simple(model, context, options).await
    }
}

async fn public_loaded_stream_termination(caller_cancel: bool) {
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
    )
    .into_iter()
    .map(|provider| Arc::new(PublicTestProvider(provider)) as Arc<dyn Provider>)
    .collect();
    let runtime =
        ProviderRuntime::with_config(&session, &bridge, providers, vec![], Default::default())
            .unwrap();
    let source = runtime
        .providers()
        .await
        .into_iter()
        .find(|provider| provider.id() == "a")
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("blocked");
    let mut context = Context::default();
    context.system_prompt = Some(
        if caller_cancel {
            "flood|"
        } else {
            "publication|"
        }
        .into(),
    );
    let mut options = SimpleStreamOptions::default();
    options.metadata = Some(BTreeMap::from([("marker".into(), json!(marker))]));
    let mut stream = source
        .stream_simple(&source.models()[0], &context, &options)
        .await;
    tokio::time::timeout(Duration::from_secs(8), async {
        while !marker.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("loaded upstream must be active before termination");
    if caller_cancel {
        options.signal.cancel();
    } else {
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
        assert!(!options.signal.is_cancelled());
    }
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let event = stream.next().await.expect("public terminal event");
            if event.is_terminal() {
                let rpi_ai::AssistantMessageEvent::Error { reason, error } = event else {
                    panic!("expected failure terminal, got {event:?}");
                };
                assert_eq!(
                    reason,
                    if caller_cancel {
                        rpi_ai::ErrorReason::Aborted
                    } else {
                        rpi_ai::ErrorReason::Error
                    }
                );
                assert_eq!(
                    error.stop_reason,
                    if caller_cancel {
                        rpi_ai::StopReason::Aborted
                    } else {
                        rpi_ai::StopReason::Error
                    }
                );
                assert_eq!(
                    error.error_message.as_deref(),
                    Some(if caller_cancel {
                        "provider request was cancelled"
                    } else {
                        "delegation epoch revoked"
                    })
                );
                assert!(stream.next().await.is_none(), "event after terminal");
                break;
            }
        }
        let finished = std::path::PathBuf::from(format!("{}.finished", marker.display()));
        let rejected = std::path::PathBuf::from(format!("{}.nonzero", marker.display()));
        while std::fs::read_to_string(&finished).ok().as_deref() != Some("drained")
            || std::fs::read_to_string(&rejected).ok().as_deref() != Some("emit cancelled")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("public terminal and upstream stop must be prompt");
    // Let every loaded wrapper finish its stale-capability probe before the
    // marker directory (and the loaded session) is released.
    tokio::time::timeout(Duration::from_secs(8), async {
        if !caller_cancel {
            for provider in ["a", "b", "c"] {
                let stale =
                    std::path::PathBuf::from(format!("{}.stale.{provider}", marker.display()));
                while std::fs::read_to_string(&stale).ok().as_deref() != Some("rejected") {
                    tokio::task::yield_now().await;
                }
            }
        }
    })
    .await
    .unwrap();
    session.begin_shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_loaded_runtime_publication_reports_epoch_revoked_and_stops_upstream() {
    public_loaded_stream_termination(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_loaded_runtime_caller_cancel_remains_aborted_and_stops_upstream() {
    public_loaded_stream_termination(true).await;
}
