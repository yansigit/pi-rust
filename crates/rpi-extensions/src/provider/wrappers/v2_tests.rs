use super::*;
use crate::delegation::{
    DelegationEpoch, DelegationResolver, DelegationScopeId, ResolvedAmbientAuth,
};

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
            rpi_ai::auth::ReplacementAuth::from_private_request(
                target.model().provider.clone(),
                rpi_ai::auth::RequestAuth {
                    api_key: Some("target-secret".into()),
                    ..Default::default()
                },
            ),
        ))
    }
}
struct Source;
#[async_trait::async_trait]
impl Provider for Source {
    fn id(&self) -> &str {
        "source"
    }
    fn models(&self) -> &[Model] {
        &[]
    }
    async fn stream_simple(
        &self,
        _: &Model,
        _: &Context,
        _: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        panic!("not dispatched")
    }
}
mod auth_options;

async fn fixture() -> (Arc<Continuation>, Arc<Continuation>, Arc<Resolver>) {
    fixture_with_registry(&crate::ExtensionRegistry::new()).await
}
async fn fixture_with_registry(
    registry: &crate::ExtensionRegistry,
) -> (Arc<Continuation>, Arc<Continuation>, Arc<Resolver>) {
    let base = super::tests::base(false);
    let epoch = DelegationEpoch::new(
        DelegationScopeId::fresh(),
        vec![base.clone() as Arc<dyn Provider>, Arc::new(Source)],
    );
    let resolver = Arc::new(Resolver(epoch.clone()));
    let port: Arc<dyn DelegationResolver> = resolver.clone();
    let binding = DelegationBinding::root(Arc::downgrade(&port), epoch, "source").unwrap();
    let session = super::tests::session(registry);
    let next = WrapperNext {
        next: base.clone(),
        session,
        plugin: "pool".into(),
        generation: "g".into(),
        instance: WrapperInstanceId::fresh(),
    };
    let make = || async {
        next.continuation(
            &base.models()[0],
            &Context::default(),
            &SimpleStreamOptions::default(),
        )
        .unwrap()
    };
    let first = binding.scope(make()).await;
    let second = binding.scope(make()).await;
    (first, second, resolver)
}
#[tokio::test]
async fn opaque_operations_bind_receipts_and_redact_target_metadata() {
    let (first, second, _resolver) = fixture().await;
    let model = first.model.id.clone();
    let target = first
        .operate_v2(
            json!({"version":2,"action":"resolveTarget","provider":"probe","model":model}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(target["model"]["baseUrl"], "");
    assert!(target["model"]["headers"].is_null());
    let auth = first.operate_v2(json!({"version":2,"action":"auth.resolveAmbient","provider":"probe","model":model,"target":target["target"]}), CancellationToken::new()).await.unwrap();
    assert!(!auth.to_string().contains("target-secret"));
    assert_eq!(
        second.target(&target).err().unwrap(),
        "delegation target belongs to a different continuation"
    );
    let wrong = first.operate_v2(json!({"version":2,"action":"auth.resolveAmbient","provider":"other","model":model,"target":target["target"]}), CancellationToken::new()).await;
    assert_eq!(wrong.err().unwrap(), "delegation target identity mismatch");
    let caller = first.caller.as_ref().unwrap();
    assert!(first
        .storage
        .lock()
        .unwrap()
        .tables
        .authentication(
            auth["receipt"].as_str().unwrap(),
            target["target"].as_str().unwrap(),
            caller
        )
        .is_ok());
    let retained = first.target(&target).unwrap();
    first
        .operate_v2(
            json!({"version":2,"action":"releaseTarget","target":target["target"]}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(retained.signal().is_cancelled());
    assert!(first
        .storage
        .lock()
        .unwrap()
        .tables
        .authentication(
            auth["receipt"].as_str().unwrap(),
            target["target"].as_str().unwrap(),
            caller
        )
        .is_err());
}
#[tokio::test]
async fn v2_protocol_keeps_descriptor_v1_and_negotiates_capabilities() {
    let (first, _, _resolver) = fixture().await;
    assert_eq!(first.descriptor()["version"], 1);
    let saved = first.clone();
    let result = tokio::task::spawn_blocking(move || {
        let input = json!({"version":1,"action":"capabilities","continuation":saved.descriptor()});
        let mut out = StbString::empty();
        assert_eq!(
            continuation_action(
                StbStringRef::from_str(&input.to_string()),
                std::ptr::null(),
                &mut out,
                std::ptr::null_mut()
            ),
            0
        );
        let out = unsafe { OwnedString::from_raw(out, Some(crate::host_free_string)) };
        serde_json::from_str::<Value>(&out.to_string_lossy()).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(
        result,
        json!({"version":1,"delegationVersions":[1,2],"opaqueTargets":true,"ambientReceipts":true,"epochRevocation":true})
    );
}

#[tokio::test]
async fn terminal_triggers_reclaim_all_capabilities_while_continuation_is_retained() {
    for trigger in ["cancel", "epoch", "close", "shutdown"] {
        let scope = Arc::new(rpi_agent::tool_execution_scope::ToolExecutionScope::default());
        let (first, second, resolver) = scope.run(fixture()).await;
        let caller = first.caller.as_ref().unwrap();
        let target = resolver.0.target(caller, "probe", &first.model.id).unwrap();
        let target_signal = target.signal();
        let (auth_signal, replacement_signal) = {
            let mut storage = first.storage.lock().unwrap();
            let id = storage.tables.insert_target(target).unwrap();
            let ambient = storage
                .tables
                .insert_auth(&id, caller, ReceiptKind::Ambient, AttemptAuth::Ambient)
                .unwrap();
            let replacement = rpi_ai::auth::ReplacementAuth::from_private_request(
                "probe".into(),
                rpi_ai::auth::RequestAuth::default(),
            );
            let replacement_id = storage
                .tables
                .insert_auth(&id, caller, ReceiptKind::Replacement, replacement.clone())
                .unwrap();
            storage.receipts.insert("v1".into(), replacement);
            (
                storage.tables.auth_signal(&ambient, &id, caller).unwrap(),
                storage
                    .tables
                    .auth_signal(&replacement_id, &id, caller)
                    .unwrap(),
            )
        };
        match trigger {
            "cancel" => first.options.signal.cancel(),
            "epoch" => resolver.0.revoke(),
            "close" => scope.close_runtime(),
            "shutdown" => first.session.begin_shutdown(),
            _ => unreachable!(),
        }
        // Do not use active/resolve here: this proves the weak monitor alone
        // reclaims storage while a blocked foreign callback could still own it.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                {
                    let storage = first.storage.lock().unwrap();
                    if storage.revoked {
                        assert!(storage.receipts.is_empty(), "{trigger}");
                        assert_eq!(storage.tables.counts(), (0, 0), "{trigger}");
                        break;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(target_signal.is_cancelled(), "{trigger}");
        assert!(auth_signal.is_cancelled(), "{trigger}");
        assert!(replacement_signal.is_cancelled(), "{trigger}");
        first.storage.lock().unwrap().revoke(); // terminal transition is idempotent
        assert!(first.admission().is_err());
        let destroyed = first.destroyed.clone();
        let weak = Arc::downgrade(&first);
        drop(first);
        drop(second);
        assert!(weak.upgrade().is_none());
        assert!(destroyed.is_cancelled());
    }
}

struct PausedResolver {
    epoch: Arc<DelegationEpoch>,
    entered: tokio::sync::Notify,
    resume: tokio::sync::Notify,
    auth: bool,
}
#[async_trait::async_trait]
impl DelegationResolver for PausedResolver {
    async fn resolve_target(
        &self,
        caller: &DelegationCaller,
        provider: &str,
        model: &str,
        _: CancellationToken,
    ) -> Result<ResolvedDelegationTarget, String> {
        let target = self.epoch.target(caller, provider, model)?;
        if !self.auth {
            self.entered.notify_one();
            self.resume.notified().await;
        }
        Ok(target)
    }
    async fn resolve_ambient(
        &self,
        _: &DelegationCaller,
        _: &ResolvedDelegationTarget,
        _: CancellationToken,
    ) -> Result<ResolvedAmbientAuth, String> {
        self.entered.notify_one();
        self.resume.notified().await;
        Ok(ResolvedAmbientAuth::new(AttemptAuth::Ambient))
    }
}
#[tokio::test]
async fn suspended_target_and_auth_resolution_cannot_repopulate_terminal_storage() {
    for auth in [false, true] {
        let (original, second, resolver) = fixture().await;
        drop(second);
        let paused = Arc::new(PausedResolver {
            epoch: resolver.0.clone(),
            entered: Default::default(),
            resume: Default::default(),
            auth,
        });
        let port: Arc<dyn DelegationResolver> = paused.clone();
        let binding =
            DelegationBinding::root(Arc::downgrade(&port), resolver.0.clone(), "source").unwrap();
        let next = WrapperNext {
            next: original.next.clone(),
            session: original.session.clone(),
            plugin: "pool".into(),
            generation: "race".into(),
            instance: WrapperInstanceId::fresh(),
        };
        let first = binding
            .scope(async {
                next.continuation(
                    &original.model,
                    &Context::default(),
                    &SimpleStreamOptions::default(),
                )
                .unwrap()
            })
            .await;
        let target = if auth {
            first
                .operate_v2(
                    json!({"action":"resolveTarget","provider":"probe","model":first.model.id}),
                    CancellationToken::new(),
                )
                .await
                .unwrap()["target"]
                .clone()
        } else {
            Value::Null
        };
        let saved = first.clone();
        let pending = tokio::spawn(async move {
            saved.operate_v2(json!({"action":if auth {"auth.resolveAmbient"} else {"resolveTarget"},"provider":"probe","model":saved.model.id,"target":target}), CancellationToken::new()).await
        });
        paused.entered.notified().await;
        first.options.signal.cancel();
        // Synchronous fallback also closes admission before the resolver resumes.
        assert!(!first.active());
        paused.resume.notify_one();
        assert!(pending.await.unwrap().is_err());
        let storage = first.storage.lock().unwrap();
        assert_eq!(storage.tables.counts(), (0, 0));
        assert!(storage.receipts.is_empty());
    }
}

#[tokio::test]
async fn inactive_lookup_reclaims_synchronously_and_preserves_lifecycle_errors() {
    for epoch in [false, true] {
        let (first, second, resolver) = fixture().await;
        let target = resolver
            .0
            .target(first.caller.as_ref().unwrap(), "probe", &first.model.id)
            .unwrap();
        let signal = target.signal();
        first
            .storage
            .lock()
            .unwrap()
            .tables
            .insert_target(target)
            .unwrap();
        let descriptor = first.descriptor();
        if epoch {
            resolver.0.revoke();
        } else {
            first.options.signal.cancel();
        }
        // No yield since the trigger: the monitor cannot have run yet.
        assert_eq!(
            Continuation::resolve(&descriptor).err().unwrap(),
            if epoch {
                "delegation epoch revoked"
            } else {
                "inactive provider continuation"
            }
        );
        assert_eq!(first.storage.lock().unwrap().tables.counts(), (0, 0));
        assert!(signal.is_cancelled());
        drop(first);
        assert_eq!(
            Continuation::resolve(&descriptor).err().unwrap(),
            "expired provider continuation"
        );
        drop(second);
    }
    let (first, second, _resolver) = fixture().await;
    let destroyed = first.destroyed.clone();
    let monitors_exited = [first.monitor_exited.clone(), second.monitor_exited.clone()];
    let weak = Arc::downgrade(&first);
    drop(first);
    drop(second);
    assert!(
        weak.upgrade().is_none(),
        "idle monitor must not retain continuation"
    );
    assert!(
        destroyed.is_cancelled(),
        "normal destruction must stop idle monitor"
    );
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        for exited in monitors_exited {
            exited.cancelled().await;
        }
    })
    .await
    .expect("reclamation monitor tasks did not exit after normal destruction");
}
