//! Adversarial admission/publication tests for the H05 coordinator.
use super::*;
use std::time::Duration;

struct Gate {
    calls: AtomicUsize,
    entered: tokio::sync::Notify,
    returned: tokio::sync::Notify,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}
extern "C" fn gated_catalog(
    _: rpi_plugin_sdk::StbStringRef,
    _: *const rpi_plugin_sdk::AuthRequestControl,
    output: *mut rpi_plugin_sdk::StbString,
    context: *mut std::ffi::c_void,
) -> i32 {
    let gate = unsafe { &*context.cast::<Gate>() };
    let first = gate.calls.fetch_add(1, Ordering::SeqCst) == 0;
    if first {
        gate.entered.notify_one();
        if gate
            .release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .is_err()
        {
            return 1;
        }
    }
    let id = if first {
        "stale-global"
    } else {
        "latest-provider"
    };
    unsafe {
        *output = rpi_plugin_sdk::StbString::from_string(
            json!({"version":1,"result":{"models":[model(id)]}}).to_string(),
        );
    }
    if first {
        gate.returned.notify_one();
    }
    0
}

#[tokio::test]
async fn global_refresh_cannot_publish_over_a_newer_provider_refresh() {
    let (release, receive) = std::sync::mpsc::channel();
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        entered: tokio::sync::Notify::new(),
        returned: tokio::sync::Notify::new(),
        release: std::sync::Mutex::new(receive),
    });
    let (runtime, owner, _) = fixture_with(|registry| {
        registry.register_provider_runtime(
            rpi_extensions::provider_runtime::RegisteredProviderRuntime {
                action_bridge: None,
                plugin: "test-plugin".into(),
                manifest: rpi_extensions::provider_runtime::RuntimeManifest {
                    version: 1,
                    id: "test".into(),
                    actions: vec!["refreshCatalog".into()],
                },
                request_fn: gated_catalog,
                free: rpi_plugin_sdk::callbacks::free_string,
                context: Arc::as_ptr(&gate).cast_mut().cast(),
            },
        );
    });
    let global = tokio::spawn({
        let (runtime, owner) = (runtime.clone(), owner.clone());
        async move {
            configured(
                &runtime,
                owner,
                json!({"action":"refresh","allowNetwork":false}),
                json!({}),
            )
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
    configured(
        &runtime,
        owner,
        json!({"action":"refresh","providers":["test"],"allowNetwork":false}),
        json!({}),
    )
    .await;
    let result = tokio::time::timeout(Duration::from_secs(5), global)
        .await
        .unwrap()
        .unwrap();
    assert!(result["errors"]["test"]
        .as_str()
        .unwrap()
        .contains("catalog changed"));
    // Release the uncooperative old callback, then drain before freeing context.
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), gate.returned.notified())
        .await
        .unwrap();
    assert_eq!(
        runtime.providers().await[0].models()[0].id,
        "latest-provider"
    );
}

#[tokio::test]
async fn retained_stale_publication_is_rejected_before_host_installation() {
    let (runtime, owner, host) = fixture();
    let publication = Arc::new(Publication {
        owner: owner.clone(),
        provider: "test".into(),
        generation: 1,
        signal: CancellationToken::new(),
        committed: AtomicU64::new(0),
    });
    runtime
        .publications
        .lock()
        .unwrap()
        .insert("retained-ticket".into(), Arc::downgrade(&publication));
    {
        let mut state = runtime.state.lock().await;
        state.generations.insert("test".into(), 2);
    }
    // Deliberately leave its signal live: the generation guard itself must work.
    let result = request(&runtime, owner, json!({"action":"publish","providerId":"test","models":[model("stale")],"publicationId":"retained-ticket"})).await;
    assert!(result.unwrap_err().contains("catalog changed"));
    assert_eq!(host.installs.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.providers().await[0].models()[0].id, "old");
}

#[tokio::test]
async fn cancelled_recomposition_does_not_install_or_revoke() {
    let (runtime, owner, host) = fixture();
    let epoch = runtime.state.lock().await.epoch.clone();
    let signal = CancellationToken::new();
    signal.cancel();
    let config = serde_json::from_value(json!({"providers":{"test":{"api":"openai-completions","baseUrl":"https://changed.invalid","models":[]}}})).unwrap();
    assert!(runtime
        .refresh_config(owner, json!({"action":"refresh"}), signal, config)
        .await
        .unwrap_err()
        .contains("cancelled"));
    assert_eq!(host.installs.load(Ordering::SeqCst), 0);
    assert!(Arc::ptr_eq(&epoch, &runtime.state.lock().await.epoch));
}

#[tokio::test]
async fn credential_mutation_invalidates_a_snapshot_captured_before_logout() {
    let directory = tempfile::tempdir().unwrap();
    let _environment = crate::config::test_support::ConfigDirEnv::new(Some(directory.path()));
    let (runtime, owner, _) = fixture();
    let ticket = runtime
        .state
        .lock()
        .await
        .catalog
        .begin_availability(None)
        .unwrap();
    runtime
        .request(
            owner,
            json!({"port":"auth","action":"logout","providerId":"test"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let state = runtime.state.lock().await;
    assert!(!state
        .catalog
        .finish_availability(
            ticket,
            vec![(
                "test".into(),
                crate::model_registry::AuthStatus::Authenticated
            )]
        )
        .unwrap());
    assert_eq!(
        state.catalog.get_auth_status("test"),
        Some(crate::model_registry::AuthStatus::Unauthenticated)
    );
}
