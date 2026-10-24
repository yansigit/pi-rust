//! Mandatory independent-DLL coverage: no environment-dependent passing skips.
use rpi_ai::{
    auth::{ReplacementAuth, RequestAuth},
    Context, Provider, SimpleStreamOptions, StopReason,
};
use rpi_extensions::delegation::*;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[path = "../fixtures/delegation_library.rs"]
pub(super) mod delegation_library;
use delegation_library::library;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loaded_frozen_legacy_prefix_still_registers_on_the_new_host() {
    let directory = tempfile::tempdir().unwrap();
    let library = directory.path().join(format!(
        "{}legacy{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    let output = std::process::Command::new(
        std::env::var_os("RPI_TEST_RUSTC").unwrap_or_else(|| "rustc".into()),
    )
    .args(["--edition=2021", "--crate-type=cdylib"])
    .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/delegation_legacy.rs"))
    .arg("-o")
    .arg(&library)
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let session = rpi_extensions::load_session_mixed(
        &[],
        &[library],
        Arc::new(rpi_extensions::NullDiagnostics),
        None,
    );
    assert_eq!(session.loaded_paths().len(), 1);
    let providers = rpi_extensions::PluggableProvider::from_session(
        &session,
        tokio::runtime::Handle::current(),
    );
    assert_eq!(providers.len(), 1);
    let provider = &providers[0];
    assert_eq!(provider.id(), "legacy");
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        provider
            .stream_simple(
                &provider.models()[0],
                &Context::default(),
                &SimpleStreamOptions::default(),
            )
            .await
            .result()
            .await
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(result.stop_reason, StopReason::Stop, "{result:?}");
    assert!(serde_json::to_string(&result)
        .unwrap()
        .contains("legacy executed"));
    session.begin_shutdown();
}
#[test]
fn loaded_cli_runtime_resolves_the_target_binding_without_source_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let agent = directory.path().join("agent");
    std::fs::create_dir(&agent).unwrap();
    std::fs::write(
        agent.join("settings.json"),
        r#"{"retry":{"enabled":false}}"#,
    )
    .unwrap();
    std::fs::write(agent.join("auth.json"),r#"{"a":{"type":"api_key","key":"source-secret"},"b":{"type":"api_key","key":"middle-secret"},"c":{"type":"api_key","key":"target-secret"}}"#).unwrap();
    for scenario in ["runtime|", "runtime-pool|"] {
        let mut command = super::command(&directory, library());
        command.args([
            "--provider",
            "a",
            "--model",
            "m",
            "--system-prompt",
            scenario,
            "--print",
            "--no-session",
            "--no-tools",
            "hello",
        ]);
        let output = super::bounded_output(&mut command);
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            format!(
                "{scenario}\n\n<cwd>\n{}\n</cwd>,wrapper:a,wrapper:b,wrapper:c,base:c",
                directory.path().display()
            )
        );
        for secret in [
            "source-secret",
            "middle-secret",
            "target-secret",
            "pool-secret",
        ] {
            assert!(!text.contains(secret));
        }
    }
}
pub(super) struct Resolver(pub(super) Arc<DelegationEpoch>);
#[async_trait::async_trait]
impl DelegationResolver for Resolver {
    async fn resolve_target(
        &self,
        caller: &DelegationCaller,
        provider: &str,
        model: &str,
        _: CancellationToken,
    ) -> Result<ResolvedDelegationTarget, String> {
        self.0.target(caller, provider, model)
    }
    async fn resolve_ambient(
        &self,
        caller: &DelegationCaller,
        target: &ResolvedDelegationTarget,
        _: CancellationToken,
    ) -> Result<ResolvedAmbientAuth, String> {
        target.validate(caller)?;
        Ok(ResolvedAmbientAuth::new(
            ReplacementAuth::from_private_request(
                target.model().provider.clone(),
                RequestAuth {
                    api_key: Some("target-secret".into()),
                    ..Default::default()
                },
            ),
        ))
    }
}
pub(super) struct Fixture {
    pub(super) session: rpi_extensions::ExtensionSession,
    pub(super) chains: Vec<Arc<dyn Provider>>,
    pub(super) epoch: Arc<DelegationEpoch>,
    pub(super) resolver: Arc<dyn DelegationResolver>,
}
impl Fixture {
    pub(super) fn new() -> Self {
        let session = rpi_extensions::load_session_mixed(
            &[],
            &[library().into()],
            Arc::new(rpi_extensions::NullDiagnostics),
            None,
        );
        let bases = rpi_extensions::PluggableProvider::from_session(
            &session,
            tokio::runtime::Handle::current(),
        );
        assert_eq!(
            bases.len(),
            3,
            "fixture registration must not be silently skipped"
        );
        let chains = rpi_extensions::wrap_providers(&session, bases);
        let epoch = DelegationEpoch::new(DelegationScopeId::fresh(), chains.clone());
        let resolver = Arc::new(Resolver(epoch.clone()));
        Self {
            session,
            chains,
            epoch,
            resolver,
        }
    }
    pub(super) async fn stream(
        &self,
        scenario: &str,
        options: &SimpleStreamOptions,
    ) -> rpi_ai::AssistantMessageEventStream {
        let source = self
            .chains
            .iter()
            .find(|provider| provider.id() == "a")
            .unwrap();
        let binding =
            DelegationBinding::root(Arc::downgrade(&self.resolver), self.epoch.clone(), "a")
                .unwrap();
        let mut context = Context::default();
        context.system_prompt = Some(format!("{scenario}|"));
        binding
            .scope(source.stream_simple(&source.models()[0], &context, options))
            .await
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loaded_nested_virtuals_run_every_target_wrapper_and_refuse_poison_and_cycles() {
    let fixture = Fixture::new();
    for (scenario, error) in [
        ("normal", None),
        ("cycle", Some("provider delegation cycle")),
        ("self", Some("provider delegation cycle")),
        (
            "poison",
            Some("v2 delegation does not accept model descriptors"),
        ),
        ("options", Some("unsupported delegated option")),
        ("receipt", Some("auth receipt target mismatch")),
        (
            "foreign",
            Some("delegation target belongs to a different continuation"),
        ),
        (
            "release",
            Some("delegation target belongs to a different continuation"),
        ),
    ] {
        let mut options = SimpleStreamOptions::default();
        options.api_key = Some("source-secret".into());
        let message = tokio::time::timeout(Duration::from_secs(5), async {
            fixture
                .stream(scenario, &options)
                .await
                .result()
                .await
                .unwrap()
        })
        .await
        .unwrap();
        match error {
            Some(error) => {
                assert_eq!(message.stop_reason, StopReason::Error, "{message:?}");
                assert_eq!(message.error_message.as_deref(), Some(error));
            }
            None => {
                assert_eq!(message.stop_reason, StopReason::Stop, "{message:?}");
                assert_eq!(
                    serde_json::to_value(&message).unwrap()["content"][0]["text"],
                    "normal|,wrapper:a,wrapper:b,wrapper:c,base:c"
                );
            }
        }
        assert!(!serde_json::to_string(&message).unwrap().contains("secret"));
    }
    fixture.session.begin_shutdown();
}
struct Reenter {
    inner: Arc<dyn Provider>,
    models: Vec<rpi_ai::Model>,
}
#[async_trait::async_trait]
impl Provider for Reenter {
    fn id(&self) -> &str {
        "b"
    }
    fn models(&self) -> &[rpi_ai::Model] {
        &self.models
    }
    async fn stream_simple(
        &self,
        model: &rpi_ai::Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> rpi_ai::AssistantMessageEventStream {
        self.inner.stream_simple(model, context, options).await
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loaded_active_instance_reentry_fails_instead_of_skipping_the_node() {
    let mut fixture = Fixture::new();
    let source = fixture
        .chains
        .iter()
        .find(|provider| provider.id() == "a")
        .unwrap()
        .clone();
    let mut model = source.models()[0].clone();
    model.provider = "b".into();
    let alias = Arc::new(Reenter {
        inner: source.clone(),
        models: vec![model],
    }) as Arc<dyn Provider>;
    fixture.epoch = DelegationEpoch::new(DelegationScopeId::fresh(), [source, alias]);
    fixture.resolver = Arc::new(Resolver(fixture.epoch.clone()));
    let message = fixture
        .stream("normal", &SimpleStreamOptions::default())
        .await
        .result()
        .await
        .unwrap();
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some("provider wrapper instance reentry")
    );
    fixture.session.begin_shutdown();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loaded_sequential_retries_derive_from_the_original_ancestry() {
    let fixture = Fixture::new();
    let message = fixture
        .stream("retry", &SimpleStreamOptions::default())
        .await
        .result()
        .await
        .unwrap();
    assert_eq!(message.stop_reason, StopReason::Stop, "{message:?}");
    assert_eq!(
        serde_json::to_value(&message).unwrap()["content"][0]["text"],
        "retry|,wrapper:a,wrapper:b,wrapper:c,base:c"
    );
    fixture.session.begin_shutdown();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loaded_concurrent_invocations_do_not_share_active_ancestry() {
    let fixture = Fixture::new();
    let options = SimpleStreamOptions::default();
    let run = || async {
        fixture
            .stream("normal", &options)
            .await
            .result()
            .await
            .unwrap()
    };
    let (a, b) = tokio::join!(run(), run());
    assert_eq!(a.stop_reason, StopReason::Stop);
    assert_eq!(b.stop_reason, StopReason::Stop);
    fixture.session.begin_shutdown();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loaded_bounded_forwarding_resumes_without_loss_or_reordering() {
    let fixture = Fixture::new();
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("blocked");
    let mut options = SimpleStreamOptions::default();
    options.metadata = Some(std::collections::BTreeMap::from([(
        "marker".into(),
        serde_json::json!(marker),
    )]));
    let mut stream = fixture.stream("order", &options).await;
    wait_file(&marker).await;
    assert!(!PathBuf::from(format!("{}.finished", marker.display())).exists());
    let mut text = String::new();
    let mut terminals = 0;
    tokio::time::timeout(Duration::from_secs(8), async {
        while let Some(event) = stream.next().await {
            match event {
                rpi_ai::AssistantMessageEvent::TextDelta { delta, .. } => text.push_str(&delta),
                rpi_ai::AssistantMessageEvent::Done { message, .. } => {
                    terminals += 1;
                    assert_eq!(message.stop_reason, StopReason::Stop);
                }
                rpi_ai::AssistantMessageEvent::Error { error, .. } => panic!("{error:?}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        text,
        (0..1024)
            .map(|index| format!("{index:04},"))
            .collect::<String>()
    );
    assert_eq!(terminals, 1);
    fixture.session.begin_shutdown();
}
pub(super) async fn wait_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(8), async {
        while !std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("DLL handshake timed out");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loaded_cancel_revoke_and_unload_drain_blocked_foreign_emit() {
    for mode in ["cancel", "revoke", "unload", "drop"] {
        let fixture = Fixture::new();
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("blocked");
        let mut options = SimpleStreamOptions::default();
        options.metadata = Some(std::collections::BTreeMap::from([(
            "marker".into(),
            serde_json::json!(marker),
        )]));
        let stream = fixture.stream("flood", &options).await;
        wait_file(&marker).await;
        tokio::time::timeout(Duration::from_secs(15), async {
            while !rpi_extensions::delegation_test_hooks::pending_emit(marker.to_str().unwrap()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("C emit never waited on bounded queue capacity");
        let stopped = std::time::Instant::now();
        let finished = PathBuf::from(format!("{}.finished", marker.display()));
        assert!(
            !finished.exists(),
            "producer must still be blocked on bounded forwarding"
        );
        match mode {
            "cancel" => options.signal.cancel(),
            "revoke" => fixture.epoch.revoke(),
            "unload" => fixture.session.begin_shutdown(),
            "drop" => {
                drop(stream);
                wait_file(&finished).await;
                assert!(PathBuf::from(format!("{}.nonzero", marker.display())).exists());
                assert!(stopped.elapsed() < Duration::from_secs(8));
                fixture.session.begin_shutdown();
                continue;
            }
            _ => unreachable!(),
        }
        let message = tokio::time::timeout(Duration::from_secs(8), stream.result())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            message.stop_reason,
            if mode == "revoke" {
                StopReason::Error
            } else {
                StopReason::Aborted
            },
            "{mode}: {:?}",
            message.error_message
        );
        wait_file(&finished).await;
        assert!(stopped.elapsed() < Duration::from_secs(8));
        assert!(
            PathBuf::from(format!("{}.nonzero", marker.display())).exists(),
            "C emit must observe cancellation"
        );
        fixture.session.begin_shutdown();
    }
}
