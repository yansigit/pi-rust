//! Offline H05 coordinator qualification (no service calls).
use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[path = "catalog_lifecycle_races.rs"]
mod races;
#[derive(Default)]
struct Host {
    fail: AtomicBool,
    installs: AtomicUsize,
    models: std::sync::Mutex<Vec<Model>>,
    child_models: std::sync::Mutex<BTreeMap<String, Vec<Model>>>,
}
macro_rules! host {
    ($($name:ident),*) => {
        #[async_trait::async_trait]
        impl RuntimeActionHost for Host {
            $(async fn $name(&self, _:Value) -> Result<Value,String> { Err("unused test action".into()) })*
            async fn install_native_providers(&self, providers: Vec<Arc<dyn Provider>>) -> Result<(), String> {
                if self.fail.load(Ordering::SeqCst) { return Err("injected installation failure".into()); }
                let models = providers.iter().flat_map(|p| p.models().to_vec()).collect();
                if let Some(scope) = rpi_agent::tool_execution_scope::current_scope() {
                    self.child_models.lock().unwrap().insert(scope.runtime_id().into(), models);
                } else {
                    *self.models.lock().unwrap() = models;
                }
                self.installs.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
    }
}
host!(
    send_message,
    send_user_message,
    append_entry,
    set_session_name,
    get_active_tools,
    set_active_tools,
    set_model,
    get_thinking_level,
    set_thinking_level,
    compact,
    get_system_prompt,
    new_session,
    fork,
    navigate_tree,
    switch_session,
    reload
);

#[derive(Clone)]
struct TestProvider(Vec<Model>);
#[async_trait::async_trait]
impl Provider for TestProvider {
    fn id(&self) -> &str {
        "test"
    }
    fn models(&self) -> &[Model] {
        &self.0
    }
    fn with_catalog(&self, models: Vec<Model>) -> Option<Arc<dyn Provider>> {
        Some(Arc::new(Self(models)))
    }
    async fn stream_simple(
        &self,
        model: &Model,
        _: &Context,
        _: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        error_stream(
            model,
            if self.0.iter().any(|m| m.id == model.id) {
                "captured provider"
            } else {
                "unknown model"
            }
            .into(),
        )
        .await
    }
}
fn model(id: &str) -> Model {
    Model::new(
        id,
        id,
        rpi_ai::Api::OpenaiCompletions,
        "test",
        "https://provider.invalid",
    )
}
extern "C" fn unused(
    _: rpi_plugin_sdk::StbStringRef,
    _: *mut rpi_plugin_sdk::StbString,
    _: *mut std::ffi::c_void,
) -> i32 {
    1
}
fn fixture() -> (Arc<ProviderRuntime>, NativeOwner, Arc<Host>) {
    fixture_with(|_| {})
}
fn fixture_with(
    configure: impl FnOnce(&mut rpi_extensions::ExtensionRegistry),
) -> (Arc<ProviderRuntime>, NativeOwner, Arc<Host>) {
    let host = Arc::new(Host::default());
    let bridge = rpi_extensions::ActionBridge::new(tokio::runtime::Handle::current(), host.clone());
    let owner = NativeOwner {
        generation: bridge.generation().into(),
        id: 1,
        plugin: "test-plugin".into(),
        child: false,
    };
    let mut registry = rpi_extensions::ExtensionRegistry::new();
    registry.register_provider_owned(
        rpi_extensions::RegisteredProvider {
            action_bridge: None,
            provider_id: "test".into(),
            base_url: "https://provider.invalid".into(),
            api_style: "openai-completions".into(),
            request_fn: unused,
            stream_fn: None,
            plugin_free_string: rpi_plugin_sdk::callbacks::free_string,
            user_data: std::ptr::null_mut(),
        },
        Some(owner.clone()),
    );
    configure(&mut registry);
    let session = ExtensionSession::from_parts(
        Arc::new(registry.snapshot()),
        rpi_extensions::PluginKeepalive::empty(),
        vec![],
        Some(bridge.clone()),
    );
    let runtime = ProviderRuntime::new(
        &session,
        &bridge,
        vec![Arc::new(TestProvider(vec![model("old")]))],
        vec![],
    )
    .unwrap();
    (runtime, owner, host)
}
async fn request(
    runtime: &ProviderRuntime,
    owner: NativeOwner,
    mut args: Value,
) -> Result<Value, String> {
    args["port"] = "providers".into();
    runtime.request(owner, args, CancellationToken::new()).await
}

async fn configured(
    runtime: &ProviderRuntime,
    owner: NativeOwner,
    args: Value,
    config: Value,
) -> Value {
    runtime
        .refresh_config(
            owner,
            args,
            CancellationToken::new(),
            serde_json::from_value(config).unwrap(),
        )
        .await
        .unwrap()
}
#[derive(Default)]
struct Callback {
    calls: AtomicUsize,
    network: AtomicUsize,
    fail: AtomicBool,
    host: std::sync::Mutex<Weak<Host>>,
    installs_seen: std::sync::Mutex<Vec<usize>>,
}
extern "C" fn catalog(
    input: rpi_plugin_sdk::StbStringRef,
    _: *const rpi_plugin_sdk::AuthRequestControl,
    output: *mut rpi_plugin_sdk::StbString,
    context: *mut std::ffi::c_void,
) -> i32 {
    let callback = unsafe { &*context.cast::<Callback>() };
    let Ok(input) = serde_json::from_str::<Value>(unsafe { input.as_str() }) else {
        return 1;
    };
    callback.calls.fetch_add(1, Ordering::SeqCst);
    if let Some(host) = callback.host.lock().unwrap().upgrade() {
        callback
            .installs_seen
            .lock()
            .unwrap()
            .push(host.installs.load(Ordering::SeqCst));
    }
    // The fixture's only network path is instrumented rather than executed.
    if input["input"]["allowNetwork"] == true {
        callback.network.fetch_add(1, Ordering::SeqCst);
        return 1;
    }
    if callback.fail.load(Ordering::SeqCst) {
        return 1;
    }
    unsafe {
        *output = rpi_plugin_sdk::StbString::from_string(
            json!({"version":1,"result":{"models":[model("remote")]}}).to_string(),
        );
    }
    0
}
fn loaded(callback: &Arc<Callback>) -> (Arc<ProviderRuntime>, NativeOwner, Arc<Host>) {
    let fixture = fixture_with(|registry| {
        registry.register_provider_runtime(
            rpi_extensions::provider_runtime::RegisteredProviderRuntime {
                action_bridge: None,
                plugin: "test-plugin".into(),
                manifest: rpi_extensions::provider_runtime::RuntimeManifest {
                    version: 1,
                    id: "test".into(),
                    actions: vec!["refreshCatalog".into()],
                },
                request_fn: catalog,
                free: rpi_plugin_sdk::callbacks::free_string,
                context: Arc::as_ptr(callback).cast_mut().cast(),
            },
        );
    });
    *callback.host.lock().unwrap() = Arc::downgrade(&fixture.2);
    fixture
}

#[tokio::test]
async fn config_recomposition_precedes_callback_and_removal_restores_base_catalog() {
    let callback = Arc::new(Callback::default());
    let (runtime, owner, host) = loaded(&callback);
    let old_epoch = runtime.state.lock().await.epoch.clone();
    let config = json!({"providers":{"test":{"api":"openai-completions","baseUrl":"https://config.invalid","models":[{"id":"configured"}]},"config-only":{"api":"openai-completions","baseUrl":"https://config.invalid","models":[{"id":"ephemeral"}]}}});
    callback.fail.store(true, Ordering::SeqCst);
    let result = configured(
        &runtime,
        owner.clone(),
        json!({"action":"refresh","allowNetwork":false}),
        config,
    )
    .await;
    assert!(result["errors"]["test"].is_string());
    assert_eq!(*callback.installs_seen.lock().unwrap(), vec![1]);
    assert_eq!(
        runtime.providers().await[0].models()[0].base_url,
        "https://config.invalid"
    );
    assert_eq!(
        runtime.state.lock().await.catalog.get_auth_status("test"),
        Some(crate::model_registry::AuthStatus::Unauthenticated)
    );
    assert_eq!(callback.calls.load(Ordering::SeqCst), 1);
    assert_eq!(callback.network.load(Ordering::SeqCst), 0);
    assert!(host
        .models
        .lock()
        .unwrap()
        .iter()
        .any(|model| model.id == "configured"));
    assert!(runtime
        .state
        .lock()
        .await
        .catalog
        .has_provider("config-only"));
    // Config is visible even when the subsequent callback fails.
    assert!(!Arc::ptr_eq(&old_epoch, &runtime.state.lock().await.epoch));
    configured(
        &runtime,
        owner.clone(),
        json!({"action":"refresh","providers":["test","config-only"],"allowNetwork":false}),
        json!({"providers":{}}),
    )
    .await;
    assert_eq!(runtime.providers().await[0].models()[0].id, "old");
    assert_eq!(
        runtime.providers().await[0].models()[0].base_url,
        "https://provider.invalid"
    );
    assert!(!runtime
        .state
        .lock()
        .await
        .catalog
        .has_provider("config-only"));
    callback.fail.store(false, Ordering::SeqCst);
    request(
        &runtime,
        owner,
        json!({"action":"refresh","providers":["test"],"allowNetwork":false}),
    )
    .await
    .unwrap();
    assert_eq!(runtime.providers().await[0].models()[0].id, "remote");
    assert_eq!(callback.network.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn selected_refresh_does_not_recompose_unselected_provider_and_headers_stay_private() {
    let callback = Arc::new(Callback::default());
    let (runtime, owner, _) = loaded(&callback);
    let config = json!({"providers":{"test":{"api":"openai-completions","baseUrl":"https://config.invalid","headers":{"Authorization":"private-header"},"models":[{"id":"configured"}]},"other":{"api":"openai-completions","baseUrl":"https://other.invalid","models":[{"id":"other"}]}}});
    configured(
        &runtime,
        owner.clone(),
        json!({"action":"refresh","providers":["test","test"],"allowNetwork":false}),
        config,
    )
    .await;
    assert_eq!(callback.calls.load(Ordering::SeqCst), 1);
    let snapshot = request(&runtime, owner, json!({"action":"inspect"}))
        .await
        .unwrap();
    assert!(!snapshot.to_string().contains("private-header"));
    assert!(!runtime.state.lock().await.catalog.has_provider("other"));
    assert!(runtime.providers().await[0]
        .models()
        .iter()
        .all(|model| model.headers.is_none()));
}

#[tokio::test]
async fn availability_sequences_reject_global_provider_overlap_and_logout_race() {
    use crate::model_registry::AuthStatus::*;
    let (runtime, _, _) = fixture();
    let state = runtime.state.lock().await;
    let registry = &state.catalog;
    let global = registry.begin_availability(None).unwrap();
    let local = registry.begin_availability(Some(&["test".into()])).unwrap();
    assert!(registry
        .finish_availability(local, vec![("test".into(), Authenticated)])
        .unwrap());
    assert!(!registry
        .finish_availability(global, vec![("test".into(), Unauthenticated)])
        .unwrap());
    let local = registry.begin_availability(Some(&["test".into()])).unwrap();
    let global = registry.begin_availability(None).unwrap();
    assert!(!registry
        .finish_availability(local, vec![("test".into(), Unauthenticated)])
        .unwrap());
    assert!(registry
        .finish_availability(global, vec![("test".into(), Authenticated)])
        .unwrap());
    let before_logout = registry.begin_availability(None).unwrap();
    registry
        .update_auth_status("test", Unauthenticated)
        .unwrap();
    assert!(!registry
        .finish_availability(before_logout, vec![("test".into(), Authenticated)])
        .unwrap());
    assert_eq!(registry.get_auth_status("test"), Some(Unauthenticated));
    let before_publish = registry.begin_availability(None).unwrap();
    registry
        .register_builtin_provider("new".into(), "New".into(), vec![])
        .unwrap();
    assert!(!registry
        .finish_availability(before_publish, vec![("test".into(), Authenticated)])
        .unwrap());
}
