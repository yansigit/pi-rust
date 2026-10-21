use super::*;
use rpi_plugin_sdk::{AuthRequestControl, StbString};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Mutex,
};

#[derive(Default)]
struct Fixture {
    calls: Mutex<Vec<Value>>,
    entered: AtomicBool,
    finish: AtomicBool,
    releases: AtomicUsize,
    delay_lock: bool,
    malformed: bool,
    fail_commit: bool,
}
fn credential() -> Value {
    json!({"type":"oauth","access":"private","refresh":"private-refresh","expires":1234567})
}
extern "C" fn request(
    input: StbStringRef,
    control: *const AuthRequestControl,
    out: *mut StbString,
    context: *mut c_void,
) -> i32 {
    rpi_plugin_sdk::guard_or(1, || {
        let fixture = unsafe { &*context.cast::<Fixture>() };
        let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
        fixture.calls.lock().unwrap().push(input.clone());
        let result = match input["action"].as_str().unwrap() {
            "read" => json!({"version":1,"kind":"credential","payload":credential()}),
            "lock" => {
                fixture.entered.store(true, Ordering::Release);
                if fixture.delay_lock {
                    let control = unsafe { &*control };
                    let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
                    while std::time::Instant::now() < until
                        && ((control.is_cancelled)(control.user_data) == 0
                            || !fixture.finish.load(Ordering::Acquire))
                    {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                }
                json!({"version":1,"kind":"locked","payload":{"lockId":"fixture-lock","credential":if fixture.malformed { json!({"type":"unknown"}) } else { credential() }}})
            }
            "replace" | "remove" => {
                if fixture.fail_commit {
                    return 1;
                }
                assert_eq!(input["input"]["lockId"], "fixture-lock");
                json!({"version":1,"kind":"success","payload":{}})
            }
            _ => return 1,
        };
        unsafe {
            *out = StbString::from_string(result.to_string());
        }
        0
    })
}
extern "C" fn release(id: StbStringRef, context: *mut c_void) {
    rpi_plugin_sdk::guard_or((), || {
        assert_eq!(unsafe { id.as_str() }, "fixture-lock");
        unsafe { &*context.cast::<Fixture>() }
            .releases
            .fetch_add(1, Ordering::Release);
    });
}
fn setup(fixture: &Fixture) -> (crate::ExtensionRegistry, NativeStoreHandle) {
    let mut registry = crate::ExtensionRegistry::new();
    let record = RegisteredCredentialStore {
        plugin: "fixture".into(),
        manifest: StoreManifest::parse(r#"{"version":1,"id":"pool"}"#).unwrap(),
        request,
        release,
        free: rpi_plugin_sdk::callbacks::free_string,
        context: (fixture as *const Fixture).cast_mut().cast(),
    };
    assert!(!registry.register_credential_store(record.clone()));
    assert!(registry.register_credential_store(record));
    let session = crate::ExtensionSession::from_parts(
        Arc::new(registry.snapshot()),
        crate::PluginKeepalive::empty(),
        vec![],
        None,
    );
    let store =
        NativeCredentialStore::from_session(&session, "pool", "provider", "account").unwrap();
    (registry, NativeStoreHandle(store))
}

#[tokio::test]
async fn store_transactions_preserve_identity_units_and_acknowledge_only_committed_updates() {
    for fail_commit in [false, true] {
        let fixture = Fixture {
            fail_commit,
            ..Default::default()
        };
        let (registry, store) = setup(&fixture);
        assert!(matches!(
            store.read("wrong-provider").await,
            Err(AuthError::Store)
        ));
        assert!(
            matches!(store.read("provider").await.unwrap(), Some(Credential::Oauth(token)) if token.expires_ms == 1234567)
        );
        let mut transaction = store
            .lock("provider", CancellationToken::new())
            .await
            .unwrap();
        registry.invalidate();
        assert!(matches!(
            store.read("provider").await,
            Err(AuthError::Cancelled)
        ));
        let replacement: Credential = serde_json::from_value(
            json!({"type":"oauth","access":"new","refresh":"rotated","expires":9876543}),
        )
        .unwrap();
        let result = transaction.replace(replacement.clone()).await;
        if fail_commit {
            assert_eq!(result, Err(AuthError::Store));
            assert!(
                matches!(transaction.current(), Some(Credential::Oauth(token)) if token.expires_ms == 1234567)
            );
        } else {
            result.unwrap();
            assert!(transaction.current() == Some(replacement));
            transaction.remove().await.unwrap();
            assert!(transaction.current().is_none());
        }
        drop(transaction);
        assert_eq!(fixture.releases.load(Ordering::Acquire), 1);
        let calls = fixture.calls.lock().unwrap();
        assert!(calls.iter().all(|call| call["providerId"] == "provider"
            && call["accountId"] == "account"
            && call["storeId"] == "pool"));
        assert_eq!(calls[2]["input"]["credential"]["expires"], 9876543);
    }
}

#[tokio::test]
async fn malformed_locked_credentials_release_the_foreign_transaction() {
    let fixture = Fixture {
        malformed: true,
        ..Default::default()
    };
    let (_registry, store) = setup(&fixture);
    assert!(matches!(
        store.lock("provider", CancellationToken::new()).await,
        Err(AuthError::Store)
    ));
    assert_eq!(fixture.releases.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn cancelled_lock_waiter_releases_a_late_lock_and_retains_the_library_until_then() {
    let fixture = Fixture {
        delay_lock: true,
        ..Default::default()
    };
    let (_registry, store) = setup(&fixture);
    let library = Arc::downgrade(&store.0.keepalive);
    let signal = CancellationToken::new();
    let pending = tokio::spawn({
        let signal = signal.clone();
        async move { store.lock("provider", signal).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !fixture.entered.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    signal.cancel();
    let cancelled = matches!(pending.await.unwrap(), Err(AuthError::Cancelled));
    let retained = library.upgrade().is_some();
    fixture.finish.store(true, Ordering::Release);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while fixture.releases.load(Ordering::Acquire) == 0 || library.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(cancelled && retained);
    assert_eq!(fixture.releases.load(Ordering::Acquire), 1);
}
