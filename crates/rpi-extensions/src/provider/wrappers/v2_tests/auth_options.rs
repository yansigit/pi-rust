use super::*;
use std::sync::atomic::AtomicUsize;

extern "C" fn unexpected_auth(
    _: StbStringRef,
    _: *const AuthRequestControl,
    _: *mut StbString,
    data: *mut c_void,
) -> i32 {
    unsafe { &*data.cast::<AtomicUsize>() }.fetch_add(1, Ordering::SeqCst);
    1
}

extern "C" fn release(_: StbStringRef, _: *mut c_void) {}

#[tokio::test]
async fn raw_v2_auth_rejects_caller_credentials_before_target_callbacks_or_account_lookup() {
    let calls = AtomicUsize::new(0);
    let mut registry = crate::ExtensionRegistry::new();
    registry.register_provider_auth(crate::provider_auth::RegisteredAuthProvider {
        action_bridge: None,
        plugin: "provider".into(),
        manifest: crate::provider_auth::AuthManifest::parse(r#"{"version":1,"id":"probe","displayName":"Probe","apiKey":true,"oauth":false,"methods":[]}"#).unwrap(),
        request_fn: unexpected_auth,
        plugin_free_string: rpi_plugin_sdk::callbacks::free_string,
        user_data: (&calls as *const AtomicUsize).cast_mut().cast(),
    });
    registry.register_credential_store(crate::credential_store::RegisteredCredentialStore {
        plugin: "pool".into(),
        manifest: crate::credential_store::StoreManifest::parse(r#"{"version":1,"id":"pool"}"#)
            .unwrap(),
        request: unexpected_auth,
        release,
        free: rpi_plugin_sdk::callbacks::free_string,
        context: (&calls as *const AtomicUsize).cast_mut().cast(),
    });
    let (first, _, _resolver) = fixture_with_registry(&registry).await;
    let target = first
        .operate_v2(
            json!({"version":2,"action":"resolveTarget","provider":"probe","model":"m"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    for action in ["resolveAuth", "auth.resolveAmbient"] {
        for options in [
            json!({"apiKey":"source-secret"}),
            json!({"headers":{"Authorization":"source-secret"}}),
            json!({"authentication":{"apiKey":"source-secret"}}),
            json!({"env":{"TARGET_API_KEY":"source-secret"}}),
            json!({"env":{"REGION":"untrusted"}}),
        ] {
            let input = json!({"version":2,"action":action,"target":target["target"],"provider":"probe","model":"m","storeId":"pool","accountId":"missing","options":options});
            let saved = first.clone();
            // Exercise the raw protocol entry, not an SDK adapter.
            let error = tokio::task::spawn_blocking(move || {
                let mut input = input;
                input["continuation"] = saved.descriptor();
                let mut out = StbString::empty();
                assert_eq!(
                    continuation_action(
                        StbStringRef::from_str(&input.to_string()),
                        std::ptr::null(),
                        &mut out,
                        std::ptr::null_mut()
                    ),
                    1
                );
                let out = unsafe { OwnedString::from_raw(out, Some(crate::host_free_string)) };
                serde_json::from_str::<Value>(&out.to_string_lossy()).unwrap()["error"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .await
            .unwrap();
            assert!(error.starts_with("v2 auth options"), "{error}");
            assert!(!error.contains("source-secret"));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(first
                .storage
                .lock()
                .unwrap()
                .tables
                .authentication(
                    "forged",
                    target["target"].as_str().unwrap(),
                    first.caller.as_ref().unwrap()
                )
                .is_err());
        }
    }
    // Permitted options still perform account lookup; there is no explicit-key shortcut.
    let result = first.operate_v2(json!({"version":2,"action":"resolveAuth","target":target["target"],"storeId":"pool","accountId":"missing","options":{"env":{},"minimumValidityMs":1000}}), CancellationToken::new()).await;
    assert!(result.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    first.session.auth_runtime().shutdown().await;
}
