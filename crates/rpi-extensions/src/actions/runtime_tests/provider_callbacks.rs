use super::*;
use crate::provider_auth::{AuthAction, AuthManifest, NativeProviderAuth, RegisteredAuthProvider};
use crate::provider_runtime::{
    NativeProviderOperations, RegisteredProviderRuntime, RuntimeManifest,
};
use crate::{ExtensionRegistry, ExtensionSession, PluginKeepalive, RegisteredProvider};
use rpi_agent::tool_execution_scope::ToolExecutionScope;
use rpi_plugin_sdk::AuthRequestControl;

struct Probe {
    entered: tokio::sync::mpsc::UnboundedSender<Value>,
    block: bool,
    release: (std::sync::Mutex<bool>, std::sync::Condvar),
    cancelled: AtomicUsize,
}
extern "C" fn callback(
    input: StbStringRef,
    control: *const AuthRequestControl,
    output: *mut StbString,
    data: *mut c_void,
) -> i32 {
    rpi_plugin_sdk::guard_or(1, || {
        // SAFETY: each test retains its probe until the foreign callback has drained.
        let probe = unsafe { &*data.cast::<Probe>() };
        let control = unsafe { &*control };
        let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
        let bridge = ActionBridge::native_invocation(0);
        let observed = match &bridge {
            Some(bridge) => {
                let mut context = StbString::empty();
                assert_eq!(
                    trampoline_runtime_action(
                        RuntimeActionId::GetExecutionContext.into(),
                        StbStringRef::from_str("{}"),
                        &mut context,
                        bridge.user_data(),
                    ),
                    0
                );
                let context = unsafe {
                    rpi_plugin_sdk::callbacks::OwnedString::from_raw(
                        context,
                        Some(crate::host_free_string),
                    )
                };
                json!({"owner":bridge.native_owner(), "identity":bridge.operation,
                    "context":serde_json::from_str::<Value>(&context.to_string_lossy()).unwrap()})
            }
            None => Value::Null,
        };
        probe.entered.send(observed.clone()).unwrap();
        if probe.block {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let (lock, ready) = &probe.release;
            let mut release = lock.lock().unwrap();
            while !*release {
                if (control.is_cancelled)(control.user_data) != 0 {
                    assert!(bridge.as_ref().unwrap().native_signal().is_cancelled());
                    probe.cancelled.store(1, Ordering::Release);
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "callback did not drain"
                );
                release = ready
                    .wait_timeout(release, Duration::from_millis(1))
                    .unwrap()
                    .0;
            }
        }
        let response = if input["action"] == "resolveApiKey" {
            json!({"version":1,"kind":"auth","payload":{"apiKey":"fixture"}})
        } else if input["action"] == "refresh" {
            json!({"version":1,"kind":"credential","payload":{"access":"new","refresh":"rotated","expires":9999999999999_i64}})
        } else if input["action"] == "toAuth" {
            json!({"version":1,"kind":"auth","payload":observed})
        } else {
            json!({"version":1,"result":observed})
        };
        unsafe {
            *output = StbString::from_string(response.to_string());
        }
        0
    })
}
extern "C" fn unused(_: StbStringRef, _: *mut StbString, _: *mut c_void) -> i32 {
    1
}

struct Fixture {
    root: Arc<ActionBridge>,
    plugin: Arc<ActionBridge>,
    auth: Arc<NativeProviderAuth>,
    runtime: Arc<NativeProviderOperations>,
    probe: Arc<Probe>,
    entered: tokio::sync::mpsc::UnboundedReceiver<Value>,
    keepalive: Arc<PluginKeepalive>,
}
impl Fixture {
    fn new(block: bool) -> Self {
        let root = ActionBridge::new(tokio::runtime::Handle::current(), Arc::new(Host::default()));
        let plugin = root.for_plugin("fixture");
        let (send, entered) = tokio::sync::mpsc::unbounded_channel();
        let probe = Arc::new(Probe {
            entered: send,
            block,
            release: Default::default(),
            cancelled: AtomicUsize::new(0),
        });
        let mut registry = ExtensionRegistry::new();
        registry.register_provider_owned(
            RegisteredProvider {
                action_bridge: None,
                provider_id: "example".into(),
                base_url: String::new(),
                api_style: "faux".into(),
                request_fn: unused,
                stream_fn: None,
                plugin_free_string: crate::host_free_string,
                user_data: std::ptr::null_mut(),
            },
            plugin.native_owner(),
        );
        registry.register_provider_auth(RegisteredAuthProvider {
            action_bridge: Some(Arc::downgrade(&plugin)), plugin: "fixture".into(),
            manifest: AuthManifest::parse(r#"{"version":1,"id":"example","displayName":"Example","apiKey":true,"oauth":true,"methods":[]}"#).unwrap(),
            request_fn: callback, plugin_free_string: crate::host_free_string,
            user_data: Arc::as_ptr(&probe).cast_mut().cast(),
        });
        registry.register_provider_runtime(RegisteredProviderRuntime {
            action_bridge: Some(Arc::downgrade(&plugin)),
            plugin: "fixture".into(),
            manifest: RuntimeManifest {
                version: 1,
                id: "example".into(),
                actions: vec!["refreshCatalog".into(), "credentialStore".into()],
            },
            request_fn: callback,
            free: crate::host_free_string,
            context: Arc::as_ptr(&probe).cast_mut().cast(),
        });
        let keepalive = PluginKeepalive::empty();
        let session = ExtensionSession::from_parts(
            Arc::new(registry.snapshot()),
            keepalive.clone(),
            vec![],
            None,
        );
        let auth = NativeProviderAuth::from_session(&session, "example").unwrap();
        let runtime =
            Arc::new(NativeProviderOperations::from_session(&session, "example").unwrap());
        Self {
            root,
            plugin,
            auth,
            runtime,
            probe,
            entered,
            keepalive,
        }
    }
}
async fn request(f: &Fixture, auth: bool) -> Value {
    if auth {
        f.auth
            .request(AuthAction::ToAuth, json!({}), CancellationToken::new())
            .await
            .unwrap()
            .payload
    } else {
        f.runtime
            .request("refreshCatalog", json!({}), CancellationToken::new())
            .await
            .unwrap()
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_and_runtime_callbacks_retain_concurrent_child_context_and_identity() {
    let f = Fixture::new(false);
    let child = Arc::new(ToolExecutionScope::with_context(
        json!({"scope":"child","tools":["read"]}),
    ));
    for auth in [false, true] {
        let identity =
            rpi_ai::identity::RequestIdentity::operation(Some("child-session".into()), "prompt");
        let (parent, child_result) = tokio::join!(
            request(&f, auth),
            child.run(rpi_ai::identity::scope(identity.clone(), request(&f, auth)))
        );
        assert_eq!(parent["context"], json!({"scope":"parent"}));
        assert_eq!(parent["identity"], Value::Null);
        assert_eq!(parent["owner"]["child"], false);
        assert_eq!(
            child_result["context"],
            json!({"scope":"child","tools":["read"]})
        );
        assert_eq!(child_result["owner"]["child"], true);
        assert_eq!(child_result["owner"]["id"], parent["owner"]["id"]);
        assert_eq!(child_result["owner"]["generation"], f.root.generation());
        assert_eq!(
            child_result["identity"],
            serde_json::to_value(identity).unwrap()
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_auth_and_runtime_waiters_cancels_native_actions_but_retains_foreign_calls() {
    for auth in [false, true] {
        let mut f = Fixture::new(true);
        let scope = Arc::new(ToolExecutionScope::default());
        let auth_provider = f.auth.clone();
        let runtime = f.runtime.clone();
        let call = tokio::spawn({
            let scope = scope.clone();
            async move {
                scope
                    .run(async move {
                        if auth {
                            auth_provider
                                .request(AuthAction::ToAuth, json!({}), CancellationToken::new())
                                .await
                                .map(|_| ())
                                .map_err(|_| ())
                        } else {
                            runtime
                                .request("refreshCatalog", json!({}), CancellationToken::new())
                                .await
                                .map(|_| ())
                                .map_err(|_| ())
                        }
                    })
                    .await
            }
        });
        f.entered.recv().await.unwrap();
        call.abort();
        assert!(call.await.unwrap_err().is_cancelled());
        let alive = Arc::downgrade(&f.keepalive);
        drop(f.auth);
        drop(f.runtime);
        drop(f.keepalive);
        drop(f.root);
        drop(f.plugin);
        tokio::time::timeout(Duration::from_secs(2), async {
            while f.probe.cancelled.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(alive.upgrade().is_some());
        assert!(
            tokio::time::timeout(Duration::from_millis(5), scope.wait_idle())
                .await
                .is_err()
        );
        *f.probe.release.0.lock().unwrap() = true;
        f.probe.release.1.notify_all();
        tokio::time::timeout(Duration::from_secs(2), scope.wait_idle())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while alive.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
#[tokio::test]
async fn expired_callback_owner_refuses_new_requests_but_credential_commit_remains_unscoped() {
    let f = Fixture::new(false);
    drop(f.plugin);
    drop(f.root);
    assert!(f
        .auth
        .request(AuthAction::ToAuth, json!({}), CancellationToken::new())
        .await
        .is_err());
    assert!(f
        .runtime
        .request("refreshCatalog", json!({}), CancellationToken::new())
        .await
        .is_err());
    let scope = Arc::new(ToolExecutionScope::default());
    scope.close_runtime();
    assert_eq!(
        scope
            .run(f.runtime.credential_store(json!({"action":"replace"})))
            .await
            .unwrap(),
        Value::Null
    );
}

struct Store(Arc<std::sync::Mutex<Option<rpi_ai::auth::Credential>>>);
#[async_trait::async_trait]
impl rpi_ai::auth::CredentialTransaction for Store {
    fn current(&self) -> Option<rpi_ai::auth::Credential> {
        self.0.lock().unwrap().clone()
    }
    async fn replace(
        &mut self,
        value: rpi_ai::auth::Credential,
    ) -> Result<(), rpi_ai::auth::AuthError> {
        *self.0.lock().unwrap() = Some(value);
        Ok(())
    }
}
#[async_trait::async_trait]
impl rpi_ai::auth::CredentialStore for Store {
    async fn read(
        &self,
        _: &str,
    ) -> Result<Option<rpi_ai::auth::Credential>, rpi_ai::auth::AuthError> {
        Ok(self.0.lock().unwrap().clone())
    }
    async fn lock(
        &self,
        _: &str,
        _: CancellationToken,
    ) -> Result<Box<dyn rpi_ai::auth::CredentialTransaction>, rpi_ai::auth::AuthError> {
        Ok(Box::new(Self(self.0.clone())))
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_auth_resolution_captures_child_scope_before_task_migration() {
    let mut f = Fixture::new(false);
    let runtime = rpi_ai::auth::AuthRuntime::default();
    let child = Arc::new(ToolExecutionScope::with_context(json!({"scope":"child"})));
    let identity =
        rpi_ai::identity::RequestIdentity::operation(Some("child-auth".into()), "prompt");
    let auth = Arc::new(crate::provider_auth::ProviderAuthentication::Native(
        f.auth.clone(),
    ));
    let store = Arc::new(Store(Default::default()));
    for replacement in [false, true] {
        child
            .run(rpi_ai::identity::scope(identity.clone(), async {
                if replacement {
                    runtime
                        .resolve_replacement(
                            "example".into(),
                            auth.clone(),
                            store.clone(),
                            rpi_ai::auth::AuthOptions {
                                api_key: Some("fixture".into()),
                                ..Default::default()
                            },
                            CancellationToken::new(),
                            Arc::new(rpi_ai::auth::SystemClock),
                        )
                        .await
                        .map(|_| ())
                } else {
                    runtime
                        .resolve(
                            "example".into(),
                            auth.clone(),
                            store.clone(),
                            Default::default(),
                            Default::default(),
                            CancellationToken::new(),
                            Arc::new(rpi_ai::auth::SystemClock),
                        )
                        .await
                        .map(|_| ())
                }
            }))
            .await
            .unwrap();
        let observed = f.entered.recv().await.unwrap();
        assert_eq!(observed["context"]["scope"], "child");
        assert_eq!(observed["owner"]["child"], true);
        assert_eq!(
            observed["identity"],
            serde_json::to_value(&identity).unwrap()
        );
    }
    runtime.shutdown().await;
    child.wait_idle().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_child_refresh_retains_scope_and_persists_rotated_token() {
    use rpi_ai::auth::{AuthRuntime, Credential, OAuthCredential, SystemClock};
    let mut f = Fixture::new(true);
    let runtime = Arc::new(AuthRuntime::default());
    let scope = Arc::new(ToolExecutionScope::with_context(
        json!({"scope":"refresh-child"}),
    ));
    let store = Arc::new(Store(Arc::new(std::sync::Mutex::new(Some(
        Credential::Oauth(OAuthCredential {
            access: "old".into(),
            refresh: "old".into(),
            expires_ms: 1,
            extra: Default::default(),
        }),
    )))));
    let signal = CancellationToken::new();
    let pending = tokio::spawn({
        let (runtime, scope, store, signal, auth) = (
            runtime.clone(),
            scope.clone(),
            store.clone(),
            signal.clone(),
            f.auth.clone(),
        );
        async move {
            scope
                .run(runtime.resolve(
                    "example".into(),
                    auth,
                    store,
                    Default::default(),
                    Default::default(),
                    signal,
                    Arc::new(SystemClock),
                ))
                .await
        }
    });
    let observed = f.entered.recv().await.unwrap();
    assert_eq!(observed["context"]["scope"], "refresh-child");
    signal.cancel();
    scope.close_runtime();
    assert!(matches!(
        pending.await.unwrap(),
        Err(rpi_ai::auth::AuthError::Cancelled)
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(5), scope.wait_idle())
            .await
            .is_err()
    );
    assert_eq!(f.probe.cancelled.load(Ordering::Acquire), 0);
    *f.probe.release.0.lock().unwrap() = true;
    f.probe.release.1.notify_all();
    tokio::time::timeout(Duration::from_secs(2), runtime.shutdown())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), scope.wait_idle())
        .await
        .unwrap();
    assert!(
        matches!(store.0.lock().unwrap().as_ref(), Some(Credential::Oauth(value)) if value.refresh == "rotated")
    );
}
