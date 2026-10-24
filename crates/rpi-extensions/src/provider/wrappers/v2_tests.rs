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
        .capabilities
        .lock()
        .unwrap()
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
        .capabilities
        .lock()
        .unwrap()
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
