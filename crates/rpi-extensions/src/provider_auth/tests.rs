use super::*;
use crate::ExtensionRegistry;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const MANIFEST: &str = r#"{"version":1,"id":"example","displayName":"Example","apiKey":true,"oauth":true,"methods":[{"id":"browser","name":"Browser login","kind":"oauth"}]}"#;

#[derive(Default)]
struct Fixture {
    requests: std::sync::Mutex<Vec<Value>>,
    started: AtomicBool,
    completed: AtomicUsize,
    wait_for_cancel: bool,
    status: i32,
    public_error: Option<Value>,
}

extern "C" fn request(
    input: StbStringRef,
    control: *const AuthRequestControl,
    out: *mut StbString,
    context: *mut c_void,
) -> i32 {
    rpi_plugin_sdk::guard_or(70, || {
        // SAFETY: test fixture and ABI arguments outlive the callback.
        let fixture = unsafe { &*context.cast::<Fixture>() };
        let control = unsafe { &*control };
        let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
        fixture.requests.lock().unwrap().push(input.clone());
        fixture.started.store(true, Ordering::Release);
        if fixture.wait_for_cancel {
            while (control.is_cancelled)(control.user_data) == 0 {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        let (kind, payload) = match input["action"].as_str().unwrap() {
            "refresh" => (
                "credential",
                json!({"access":"new-access","refresh":"new-refresh","expires":1234567890123_i64}),
            ),
            "login" => (
                "interaction",
                json!({"kind":"input","title":"Enter code","continuation":"private-state"}),
            ),
            "continue" => (
                "credential",
                json!({"type":"oauth","access":"login-access","refresh":"login-refresh","expires":1234567890123_i64}),
            ),
            _ => ("auth", json!({"apiKey":"private-key"})),
        };
        let response = match &fixture.public_error {
            Some(payload) => json!({"version":1,"kind":"error","payload":payload}),
            None => json!({"version":1,"kind":kind,"payload":payload}),
        };
        unsafe {
            *out = StbString::from_string(response.to_string());
        }
        fixture.completed.fetch_add(1, Ordering::Release);
        fixture.status
    })
}

struct LoginStore {
    credential: std::sync::Mutex<Option<Credential>>,
    entered: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
    fail: bool,
}
struct LoginTransaction(Arc<LoginStore>);
struct LoginStoreHandle(Arc<LoginStore>);

#[async_trait::async_trait]
impl rpi_ai::auth::CredentialTransaction for LoginTransaction {
    fn current(&self) -> Option<Credential> {
        self.0.credential.lock().unwrap().clone()
    }
    async fn replace(&mut self, credential: Credential) -> Result<(), AuthError> {
        self.0.entered.add_permits(1);
        self.0.release.acquire().await.unwrap().forget();
        if self.0.fail {
            return Err(AuthError::Store);
        }
        *self.0.credential.lock().unwrap() = Some(credential);
        Ok(())
    }
}

#[async_trait::async_trait]
impl rpi_ai::auth::CredentialStore for LoginStoreHandle {
    async fn read(&self, _: &str) -> Result<Option<Credential>, AuthError> {
        Ok(self.0.credential.lock().unwrap().clone())
    }
    async fn lock(
        &self,
        _: &str,
        _: CancellationToken,
    ) -> Result<Box<dyn rpi_ai::auth::CredentialTransaction>, AuthError> {
        Ok(Box::new(LoginTransaction(self.0.clone())))
    }
}

#[tokio::test]
async fn login_forwards_private_continuation_and_waits_for_store_acknowledgement() {
    for fail in [false, true] {
        let fixture = Fixture::default();
        let (_registry, auth) = setup(&fixture);
        let store = Arc::new(LoginStore {
            credential: std::sync::Mutex::new(None),
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
            fail,
        });
        let pending = tokio::spawn({
            let store = store.clone();
            async move {
                auth.login(
                    "browser",
                    Arc::new(LoginStoreHandle(store)),
                    CancellationToken::new(),
                    |interaction| async move {
                        let AuthInteraction::Input {
                            title,
                            continuation,
                            ..
                        } = interaction
                        else {
                            panic!("expected input")
                        };
                        assert_eq!(title, "Enter code");
                        Ok(json!({"value":"callback-code","continuation":continuation}))
                    },
                )
                .await
            }
        });
        store.entered.acquire().await.unwrap().forget();
        assert!(!pending.is_finished());
        assert!(store.credential.lock().unwrap().is_none());
        store.release.add_permits(1);
        let result = pending.await.unwrap();
        assert_eq!(result, if fail { Err(AuthError::Store) } else { Ok(()) });
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests[0]["input"]["methodId"], "browser");
        assert_eq!(
            requests[1]["input"],
            json!({"value":"callback-code","continuation":"private-state"})
        );
        if !fail {
            assert!(
                matches!(&*store.credential.lock().unwrap(), Some(Credential::Oauth(credential)) if credential.expires_ms == 1234567890123)
            );
        }
    }
}

fn setup(fixture: &Fixture) -> (ExtensionRegistry, Arc<NativeProviderAuth>) {
    let mut registry = ExtensionRegistry::new();
    let record = RegisteredAuthProvider {
        plugin: "fixture".into(),
        manifest: AuthManifest::parse(MANIFEST).unwrap(),
        request_fn: request,
        plugin_free_string: rpi_plugin_sdk::callbacks::free_string,
        user_data: (fixture as *const Fixture).cast_mut().cast(),
    };
    assert!(!registry.register_provider_auth(record.clone()));
    assert!(registry.register_provider_auth(record));
    let session = ExtensionSession::from_parts(
        Arc::new(registry.snapshot()),
        PluginKeepalive::empty(),
        vec![],
        None,
    );
    (
        registry,
        NativeProviderAuth::from_session(&session, "example").unwrap(),
    )
}

#[test]
fn manifest_rejects_versions_duplicate_methods_and_secret_fields() {
    for change in [
        json!({"version":2}),
        json!({"id":" "}),
        json!({"apiKey":"secret"}),
        json!({"refresh":"secret"}),
        json!({"oauth":false}),
    ] {
        let mut value: Value = serde_json::from_str(MANIFEST).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .extend(change.as_object().unwrap().clone());
        assert!(AuthManifest::parse(&value.to_string()).is_err());
    }
    let mut value: Value = serde_json::from_str(MANIFEST).unwrap();
    let duplicate = value["methods"][0].clone();
    value["methods"].as_array_mut().unwrap().push(duplicate);
    assert!(AuthManifest::parse(&value.to_string()).is_err());
}

#[tokio::test]
async fn static_methods_reject_cancelled_and_invalidated_sessions() {
    let fixture = Fixture::default();
    let (registry, auth) = setup(&fixture);
    assert_eq!(
        auth.methods(CancellationToken::new()).await.unwrap()[0].id,
        "browser"
    );
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        auth.methods(cancelled).await,
        Err(AuthError::Cancelled)
    ));
    registry.invalidate();
    assert!(matches!(
        auth.methods(CancellationToken::new()).await,
        Err(AuthError::Cancelled)
    ));
    assert_eq!(fixture.completed.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn private_callbacks_preserve_login_refresh_and_request_overrides() {
    let fixture = Fixture::default();
    let (_registry, auth) = setup(&fixture);
    let response = auth
        .request(
            AuthAction::Login,
            json!({"methodId":"browser"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(response.kind == AuthResponseKind::Interaction);
    assert_eq!(response.payload["continuation"], "private-state");
    let credential = OAuthCredential {
        access: "old".into(),
        refresh: "old-refresh".into(),
        expires_ms: 1,
        extra: Default::default(),
    };
    let refreshed = auth
        .refresh(credential, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(refreshed.expires_ms, 1234567890123);
    let derived = auth.to_auth(refreshed).await.unwrap();
    assert_eq!(derived.api_key.as_deref(), Some("private-key"));
    let context = AuthContext {
        env: [("CUSTOM_KEY".into(), "attempt-key".into())].into(),
    };
    auth.resolve_api_key(context, None, CancellationToken::new())
        .await
        .unwrap();
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests[1]["input"]["credential"]["expires"], 1);
    assert_eq!(requests[3]["input"]["env"]["CUSTOM_KEY"], "attempt-key");
    assert_eq!(requests[3]["providerId"], "example");
}

#[tokio::test]
async fn invalidation_prevents_new_requests_and_errors_do_not_expose_payloads() {
    let fixture = Fixture {
        status: 1,
        ..Default::default()
    };
    let (registry, auth) = setup(&fixture);
    let error = auth
        .to_auth(OAuthCredential {
            access: "secret".into(),
            refresh: String::new(),
            expires_ms: 0,
            extra: Default::default(),
        })
        .await
        .err()
        .unwrap();
    assert_eq!(error, AuthError::Derive);
    assert!(!error.to_string().contains("private"));
    registry.invalidate();
    assert!(matches!(
        auth.request(AuthAction::Login, Value::Null, CancellationToken::new())
            .await,
        Err(AuthError::Cancelled)
    ));
    assert_eq!(fixture.completed.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn cancellation_reaches_foreign_callback_and_keeps_library_alive() {
    let fixture = Arc::new(Fixture {
        wait_for_cancel: true,
        ..Default::default()
    });
    let (registry, auth) = setup(&fixture);
    let keepalive = Arc::downgrade(&auth.keepalive);
    let signal = CancellationToken::new();
    let pending = tokio::spawn({
        let signal = signal.clone();
        async move { auth.request(AuthAction::Login, Value::Null, signal).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !fixture.started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    registry.invalidate();
    assert!(keepalive.upgrade().is_some());
    signal.cancel();
    assert!(matches!(pending.await.unwrap(), Err(AuthError::Cancelled)));
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while keepalive.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(fixture.completed.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn explicit_public_diagnostics_survive_login_refresh_and_derivation() {
    let message = "Pi holds the vault placeholder, not a login.";
    let fixture = Fixture {
        public_error: Some(json!({"message":message})),
        ..Default::default()
    };
    let (_registry, auth) = setup(&fixture);
    let credential = OAuthCredential {
        access: "secret".into(),
        refresh: "private-refresh".into(),
        expires_ms: 0,
        extra: Default::default(),
    };
    let expected = AuthError::Provider(message.into());
    assert_eq!(
        auth.request(
            AuthAction::Login,
            json!({"methodId":"browser"}),
            CancellationToken::new()
        )
        .await
        .err(),
        Some(expected.clone())
    );
    assert_eq!(
        auth.refresh(credential.clone(), CancellationToken::new())
            .await
            .err(),
        Some(expected.clone())
    );
    assert_eq!(auth.to_auth(credential).await.err(), Some(expected));
    for payload in [
        json!({"message":null}),
        json!({"message":"public","refresh":"private"}),
        json!("public"),
    ] {
        let fixture = Fixture {
            public_error: Some(payload),
            ..Default::default()
        };
        let (_registry, auth) = setup(&fixture);
        assert_eq!(
            auth.request(AuthAction::Login, json!({}), CancellationToken::new())
                .await
                .err(),
            Some(AuthError::Resolve)
        );
    }
}
