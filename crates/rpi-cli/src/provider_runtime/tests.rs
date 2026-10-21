use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct Host {
    fail: AtomicBool,
    installs: AtomicUsize,
    models: std::sync::Mutex<Vec<Model>>,
}
macro_rules! host {
    ($($name:ident),*) => {
        #[async_trait::async_trait]
        impl RuntimeActionHost for Host {
            $(async fn $name(&self, _:Value) -> Result<Value,String> { Err("unused test action".into()) })*
            async fn install_native_providers(&self, providers: Vec<Arc<dyn Provider>>) -> Result<(), String> {
                if self.fail.load(Ordering::SeqCst) { return Err("injected installation failure".into()); }
                *self.models.lock().unwrap() = providers.iter().flat_map(|p| p.models().to_vec()).collect();
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

#[tokio::test]
async fn authentication_discovery_uses_registered_native_legacy_or_protocol_auth() {
    let (runtime, owner, _) = fixture();
    let methods = runtime
        .auth_request(
            owner.clone(),
            json!({"action":"methods","providerId":"test"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        methods,
        json!([{"id":"api_key","name":"API key","kind":"api_key"}])
    );
    assert!(runtime
        .auth_request(
            owner,
            json!({"action":"methods","providerId":"unregistered"}),
            CancellationToken::new()
        )
        .await
        .is_err());
    let (runtime, owner, _) = fixture_with(|registry| {
        registry.register_oauth_provider(rpi_extensions::RegisteredOAuthProvider {
            plugin: "test-plugin".into(),
            manifest_json: r#"{"id":"test"}"#.into(),
            request_fn: unused,
            plugin_free_string: rpi_plugin_sdk::callbacks::free_string,
            user_data: std::ptr::null_mut(),
        });
    });
    let methods = runtime
        .auth_request(
            owner,
            json!({"action":"methods","providerId":"test"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        methods,
        json!([{"id":"oauth","name":"OAuth","kind":"oauth"}])
    );
}
#[tokio::test]
async fn publication_rejects_foreign_child_stale_and_failed_installs_without_committing() {
    let (runtime, owner, host) = fixture();
    let publish = json!({"action":"publish","providerId":"test","models":[model("new")]});
    let mut child = owner.clone();
    child.child = true;
    assert!(request(&runtime, child, publish.clone()).await.is_err());
    let mut foreign = owner.clone();
    foreign.plugin = "foreign".into();
    assert!(request(&runtime, foreign, publish.clone()).await.is_err());
    let mut stale = owner.clone();
    stale.generation = "stale".into();
    assert!(request(&runtime, stale, publish.clone()).await.is_err());
    host.fail.store(true, Ordering::SeqCst);
    assert!(request(&runtime, owner.clone(), publish.clone())
        .await
        .unwrap_err()
        .contains("installation"));
    let inspected = request(&runtime, owner.clone(), json!({"action":"inspect"}))
        .await
        .unwrap();
    assert_eq!(inspected["revision"], 1);
    assert_eq!(inspected["providers"][0]["models"][0]["id"], "old");
    assert_eq!(host.installs.load(Ordering::SeqCst), 0);
    host.fail.store(false, Ordering::SeqCst);
    assert_eq!(
        request(&runtime, owner.clone(), publish).await.unwrap()["revision"],
        2
    );
    assert_eq!(host.models.lock().unwrap()[0].id, "new");
    assert!(request(
        &runtime,
        owner.clone(),
        json!({"action":"publish","providerId":"test","models":[],"expectedRevision":1})
    )
    .await
    .is_err());
    assert_eq!(
        request(&runtime, owner, json!({"action":"inspect"}))
            .await
            .unwrap()["revision"],
        2
    );
}
#[tokio::test]
async fn captured_calls_survive_publication_but_old_proxies_use_current_registrations() {
    let (runtime, owner, _) = fixture();
    let proxy = runtime.providers().await.remove(0);
    let old_stream = proxy
        .stream_simple(
            &model("old"),
            &Context::new(vec![]),
            &SimpleStreamOptions::default(),
        )
        .await;
    request(
        &runtime,
        owner.clone(),
        json!({"action":"publish","providerId":"test","models":[model("new")]}),
    )
    .await
    .unwrap();
    assert_eq!(
        old_stream.result().await.unwrap().error_message.as_deref(),
        Some("captured provider")
    );
    assert_eq!(
        proxy
            .stream_simple(
                &model("old"),
                &Context::new(vec![]),
                &SimpleStreamOptions::default()
            )
            .await
            .result()
            .await
            .unwrap()
            .error_message
            .as_deref(),
        Some("unknown model")
    );
    request(
        &runtime,
        owner.clone(),
        json!({"action":"unregister","providerId":"test"}),
    )
    .await
    .unwrap();
    assert_eq!(
        proxy
            .stream_simple(
                &model("new"),
                &Context::new(vec![]),
                &SimpleStreamOptions::default()
            )
            .await
            .result()
            .await
            .unwrap()
            .error_message
            .as_deref(),
        Some("provider is no longer registered")
    );
    assert!(request(&runtime, owner.clone(), json!({"action":"register","providerId":"alias","implementationId":"test","models":[Model::new("alias-model","Alias",rpi_ai::Api::OpenaiCompletions,"alias","https://provider.invalid")]})).await.is_ok());
    assert_eq!(
        request(&runtime, owner, json!({"action":"inspect"}))
            .await
            .unwrap()["providers"][0]["id"],
        "alias"
    );
}
#[tokio::test]
async fn catalogs_reject_auth_material_and_duplicate_model_identities() {
    let (runtime, owner, _) = fixture();
    let mut credential_bearing = model("new");
    credential_bearing.headers = Some(BTreeMap::from([("authorization".into(), "private".into())]));
    for models in [
        vec![credential_bearing],
        vec![model("duplicate"), model("DUPLICATE")],
    ] {
        assert!(request(
            &runtime,
            owner.clone(),
            json!({"action":"publish","providerId":"test","models":models})
        )
        .await
        .is_err());
    }
    assert_eq!(
        request(&runtime, owner, json!({"action":"inspect"}))
            .await
            .unwrap()["revision"],
        1
    );
}

#[tokio::test]
async fn late_refresh_cannot_overwrite_a_newer_publication() {
    struct Gate {
        started: tokio::sync::Notify,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }
    extern "C" fn refresh(
        _: rpi_plugin_sdk::StbStringRef,
        _: *const rpi_plugin_sdk::AuthRequestControl,
        out: *mut rpi_plugin_sdk::StbString,
        context: *mut std::ffi::c_void,
    ) -> i32 {
        let gate = unsafe { &*context.cast::<Gate>() };
        gate.started.notify_one();
        if gate
            .release
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_err()
        {
            return 1;
        }
        unsafe {
            *out = rpi_plugin_sdk::StbString::from_string(
                json!({"version":1,"result":{"models":[model("late")]}}).to_string(),
            );
        }
        0
    }
    let (release, receiver) = std::sync::mpsc::channel();
    let gate = Arc::new(Gate {
        started: tokio::sync::Notify::new(),
        release: std::sync::Mutex::new(receiver),
    });
    let (runtime, owner, _) = fixture_with(|registry| {
        registry.register_provider_runtime(
            rpi_extensions::provider_runtime::RegisteredProviderRuntime {
                plugin: "test-plugin".into(),
                manifest: rpi_extensions::provider_runtime::RuntimeManifest {
                    version: 1,
                    id: "test".into(),
                    actions: vec!["refreshCatalog".into()],
                },
                request_fn: refresh,
                free: rpi_plugin_sdk::callbacks::free_string,
                context: Arc::as_ptr(&gate).cast_mut().cast(),
            },
        );
    });
    let pending = tokio::spawn({
        let runtime = runtime.clone();
        let owner = owner.clone();
        async move {
            request(
                &runtime,
                owner,
                json!({"action":"refresh","providerId":"test","allowNetwork":false}),
            )
            .await
        }
    });
    gate.started.notified().await;
    request(
        &runtime,
        owner.clone(),
        json!({"action":"publish","providerId":"test","models":[model("committed")]}),
    )
    .await
    .unwrap();
    release.send(()).unwrap();
    assert!(pending
        .await
        .unwrap()
        .unwrap_err()
        .contains("catalog changed"));
    let inspected = request(&runtime, owner, json!({"action":"inspect"}))
        .await
        .unwrap();
    assert_eq!(inspected["providers"][0]["models"][0]["id"], "committed");
    assert_eq!(inspected["revision"], 2);
}

#[test]
fn native_credential_transition_is_owner_bound_and_retains_lock_through_cancelled_commit() {
    use fs2::FileExt;
    struct Environment(Option<std::ffi::OsString>);
    impl Drop for Environment {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
            }
        }
    }
    struct Gate {
        started: tokio::sync::Notify,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        input: std::sync::Mutex<Option<Value>>,
    }
    extern "C" fn transition(
        input: rpi_plugin_sdk::StbStringRef,
        _: *const rpi_plugin_sdk::AuthRequestControl,
        out: *mut rpi_plugin_sdk::StbString,
        context: *mut std::ffi::c_void,
    ) -> i32 {
        rpi_plugin_sdk::guard_or(1, || {
            // The fixture remains owned until auth shutdown drains this callback.
            let gate = unsafe { &*context.cast::<Gate>() };
            let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
            let path = std::path::PathBuf::from(input["input"]["authPath"].as_str().unwrap());
            assert_eq!(input["action"], "credentialStore");
            assert_eq!(input["input"]["expiryUnit"], "seconds");
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path.with_extension("json.lock"))
                .unwrap();
            assert!(lock.try_lock_exclusive().is_err());
            *gate.input.lock().unwrap() = Some(input);
            gate.started.notify_one();
            gate.release
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            crate::config::atomic_write(
                &path,
                br#"{"test":{"type":"api_key","key":"fixture-marker"}}"#,
            )
            .unwrap();
            unsafe {
                *out = rpi_plugin_sdk::StbString::from_string(
                    json!({"version":1,"result":{"committed":true}}).to_string(),
                );
            }
            0
        })
    }
    let _environment_lock = crate::config::test_support::env_lock().lock().unwrap();
    let root = tempfile::tempdir().unwrap();
    let _environment = Environment(std::env::var_os(crate::config::CONFIG_DIR_ENV));
    std::env::set_var(crate::config::CONFIG_DIR_ENV, root.path());
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (release, receiver) = std::sync::mpsc::channel();
        let gate = Arc::new(Gate {
            started: tokio::sync::Notify::new(),
            release: std::sync::Mutex::new(receiver),
            input: std::sync::Mutex::new(None),
        });
        let (runtime, owner, _) = fixture_with(|registry| {
            registry.register_provider_runtime(
                rpi_extensions::provider_runtime::RegisteredProviderRuntime {
                    plugin: "test-plugin".into(),
                    manifest: rpi_extensions::provider_runtime::RuntimeManifest {
                        version: 1,
                        id: "test".into(),
                        actions: vec!["credentialStore".into()],
                    },
                    request_fn: transition,
                    free: rpi_plugin_sdk::callbacks::free_string,
                    context: Arc::as_ptr(&gate).cast_mut().cast(),
                },
            );
        });
        let args =
            json!({"action":"credentialStore","providerId":"test","input":{"operation":"enter"}});
        let mut foreign = owner.clone();
        foreign.plugin = "another-plugin".into();
        let mut child = owner.clone();
        child.child = true;
        for caller in [foreign, child] {
            assert!(runtime
                .auth_request(caller, args.clone(), CancellationToken::new())
                .await
                .is_err());
        }
        let mut redirected = args.clone();
        redirected["path"] = json!("untrusted-auth.json");
        assert!(runtime
            .auth_request(owner.clone(), redirected, CancellationToken::new())
            .await
            .is_err());
        let signal = CancellationToken::new();
        let pending = tokio::spawn({
            let runtime = runtime.clone();
            let signal = signal.clone();
            async move { runtime.auth_request(owner, args, signal).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.started.notified())
            .await
            .unwrap();
        signal.cancel();
        assert!(pending.await.unwrap().is_err());
        let path = root.path().join("auth.json");
        assert_eq!(
            gate.input.lock().unwrap().as_ref().unwrap()["input"]["authPath"],
            json!(path)
        );
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.with_extension("json.lock"))
            .unwrap();
        assert!(lock.try_lock_exclusive().is_err());
        runtime
            .session
            .snapshot()
            .unwrap()
            .active_flag()
            .store(false, Ordering::SeqCst);
        let shutdown = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.session.auth_runtime().shutdown().await }
        });
        tokio::task::yield_now().await;
        assert!(!shutdown.is_finished());
        release.send(()).unwrap();
        shutdown.await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"test":{"type":"api_key","key":"fixture-marker"}}"#
        );
        lock.try_lock_exclusive().unwrap();
    });
}

#[test]
fn owned_auth_resolution_keeps_configuration_private_and_environment_request_local() {
    struct Environment(Option<std::ffi::OsString>);
    impl Drop for Environment {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
            }
        }
    }
    extern "C" fn auth(
        input: rpi_plugin_sdk::StbStringRef,
        _: *const rpi_plugin_sdk::AuthRequestControl,
        output: *mut rpi_plugin_sdk::StbString,
        data: *mut std::ffi::c_void,
    ) -> i32 {
        rpi_plugin_sdk::guard_or(1, || {
            let calls = unsafe { &*data.cast::<AtomicUsize>() };
            calls.fetch_add(1, Ordering::SeqCst);
            let request: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
            assert_eq!(request["action"], "resolveApiKey");
            let input = &request["input"];
            let key = input["credential"]["key"].as_str().unwrap_or("fallback");
            unsafe {
                *output = rpi_plugin_sdk::StbString::from_string(json!({"version":1,"kind":"auth","payload":{
                    "apiKey":key,"env":{"RPI_AUTH_TEST_OVERLAY":input["env"]["RPI_AUTH_TEST_OVERLAY"].as_str().unwrap_or("ambient")}
                }}).to_string());
            }
            0
        })
    }
    let _lock = crate::config::test_support::env_lock().lock().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let _environment = Environment(std::env::var_os(crate::config::CONFIG_DIR_ENV));
    std::env::set_var(crate::config::CONFIG_DIR_ENV, directory.path());
    std::fs::write(directory.path().join("models.json"), r#"{"providers":{"test":{"apiKey":"configured-private-key","headers":{"X-Overlay":"${RPI_AUTH_TEST_OVERLAY}"}}}}"#).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (runtime, owner, _) = fixture_with(|registry| {
            registry.register_provider_auth(rpi_extensions::provider_auth::RegisteredAuthProvider {
                plugin: "test-plugin".into(),
                manifest: serde_json::from_value(json!({"version":1,"id":"test","displayName":"Test","apiKey":true,"oauth":false,"methods":[]})).unwrap(),
                request_fn: auth,
                plugin_free_string: rpi_plugin_sdk::callbacks::free_string,
                user_data: Arc::as_ptr(&calls).cast_mut().cast(),
            });
        });
        // Catalog refresh and commands resolve from this generation's same configured auth snapshot.
        std::fs::write(directory.path().join("models.json"), "{}").unwrap();
        let request = |env: &str| json!({"port":"auth","action":"resolve","providerId":"test","env":{"RPI_AUTH_TEST_OVERLAY":env}});
        let (left, right) = tokio::join!(
            runtime.request(owner.clone(),request("left"), CancellationToken::new()),
            runtime.request(owner.clone(),request("right"), CancellationToken::new()),
        );
        let (left,right) = (left.unwrap(),right.unwrap());
        assert_eq!(left["apiKey"], "configured-private-key");
        assert_eq!(left["headers"]["X-Overlay"], "left");
        assert_eq!(right["headers"]["X-Overlay"], "right");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        for (caller,mut input) in [
            (NativeOwner { plugin:"foreign".into(), ..owner.clone() }, request("rejected")),
            (NativeOwner { id:2, ..owner.clone() }, request("rejected")),
            (owner.clone(), { let mut v=request("rejected");v["storeId"]="foreign-store".into();v }),
            (owner.clone(), { let mut v=request("rejected");v["env"]=json!({"RPI_AUTH_TEST_OVERLAY":false});v }),
        ] {
            input["version"] = 1.into();
            assert!(runtime.request(caller,input,CancellationToken::new()).await.is_err());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let signal = CancellationToken::new();signal.cancel();
        assert!(runtime.request(owner.clone(),request("cancelled"),signal).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        std::fs::write(directory.path().join("auth.json"),r#"{"test":{"type":"api_key","key":"stored-private-key"}}"#).unwrap();
        let result = runtime.request(owner,request("stored"),CancellationToken::new()).await.unwrap();
        assert_eq!(result["apiKey"],"stored-private-key");
        let catalog = runtime.providers().await;
        let serialized = serde_json::to_string(&catalog[0].models()).unwrap();
        assert!(!serialized.contains("private-key"));
        assert!(!serialized.contains("X-Overlay"));
        runtime.session.auth_runtime().shutdown().await;
    });
}

#[tokio::test]
async fn awaited_publication_commits_once_and_rejects_stale_or_expired_refresh_tickets() {
    struct Gate {
        started: tokio::sync::mpsc::Sender<Value>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        returned: tokio::sync::Notify,
    }
    extern "C" fn refresh(
        input: rpi_plugin_sdk::StbStringRef,
        _: *const rpi_plugin_sdk::AuthRequestControl,
        output: *mut rpi_plugin_sdk::StbString,
        context: *mut std::ffi::c_void,
    ) -> i32 {
        let gate = unsafe { &*context.cast::<Gate>() };
        gate.started
            .blocking_send(serde_json::from_str(unsafe { input.as_str() }).unwrap())
            .unwrap();
        gate.release
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        unsafe {
            *output = rpi_plugin_sdk::StbString::from_string(
                json!({"version":1,"result":{"published":true}}).to_string(),
            );
        }
        gate.returned.notify_one();
        0
    }
    let (send, mut started) = tokio::sync::mpsc::channel(1);
    let (release, receive) = std::sync::mpsc::channel();
    let gate = Arc::new(Gate {
        started: send,
        release: std::sync::Mutex::new(receive),
        returned: tokio::sync::Notify::new(),
    });
    let (runtime, owner, host) = fixture_with(|registry| {
        registry.register_provider_runtime(
            rpi_extensions::provider_runtime::RegisteredProviderRuntime {
                plugin: "test-plugin".into(),
                manifest: rpi_extensions::provider_runtime::RuntimeManifest {
                    version: 1,
                    id: "test".into(),
                    actions: vec!["refreshCatalog".into()],
                },
                request_fn: refresh,
                free: rpi_plugin_sdk::callbacks::free_string,
                context: Arc::as_ptr(&gate).cast_mut().cast(),
            },
        );
    });
    for stale in [false, true] {
        let pending = tokio::spawn({
            let runtime = runtime.clone();
            let owner = owner.clone();
            async move {
                request(
                    &runtime,
                    owner,
                    json!({"action":"refresh","providerId":"test"}),
                )
                .await
            }
        });
        let input = started.recv().await.unwrap();
        let publish = json!({"action":"publish","providerId":"test","models":[model("awaited")],"publicationId":input["input"]["publicationId"]});
        assert!(request(
            &runtime,
            NativeOwner {
                plugin: "foreign".into(),
                ..owner.clone()
            },
            publish.clone()
        )
        .await
        .is_err());
        if stale {
            request(
                &runtime,
                owner.clone(),
                json!({"action":"publish","providerId":"test","models":[model("newer")]}),
            )
            .await
            .unwrap();
            assert!(request(&runtime, owner.clone(), publish.clone())
                .await
                .unwrap_err()
                .contains("catalog changed"));
        } else {
            let receipt = request(&runtime, owner.clone(), publish.clone())
                .await
                .unwrap();
            assert_eq!(receipt["revision"], 2);
            assert!(request(&runtime, owner.clone(), publish.clone())
                .await
                .unwrap_err()
                .contains("does not own"));
        }
        release.send(()).unwrap();
        let result = pending.await.unwrap();
        gate.returned.notified().await;
        if stale {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap()["revision"], 2);
            assert_eq!(host.installs.load(Ordering::SeqCst), 1);
        }
        assert!(request(&runtime, owner.clone(), publish)
            .await
            .unwrap_err()
            .contains("expired"));
    }
    assert_eq!(runtime.providers().await[0].models()[0].id, "newer");
}
