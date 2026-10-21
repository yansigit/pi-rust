use super::*;
use rpi_agent::tool_execution_scope::ToolExecutionScope;

async fn models(runtime: &ProviderRuntime, owner: &NativeOwner) -> Value {
    request(runtime, owner.clone(), json!({"action":"inspect"}))
        .await
        .unwrap()["providers"]
        .clone()
}
#[tokio::test]
async fn child_catalog_publications_registration_and_removal_leave_parent_and_sibling_unchanged() {
    let (runtime, owner, host) = fixture();
    let child_owner = NativeOwner {
        child: true,
        ..owner.clone()
    };
    let first = Arc::new(ToolExecutionScope::default());
    let second = Arc::new(ToolExecutionScope::default());
    let parent = models(&runtime, &owner).await;
    for (scope, name) in [(&first, "first"), (&second, "second")] {
        scope
            .run(request(
                &runtime,
                child_owner.clone(),
                json!({"action":"publish","providerId":"test","models":[model(name)]}),
            ))
            .await
            .unwrap();
        assert_eq!(
            scope.run(models(&runtime, &child_owner)).await[0]["models"][0]["id"],
            name
        );
        assert_eq!(
            host.child_models.lock().unwrap()[scope.runtime_id()][0].id,
            name
        );
    }
    assert_eq!(models(&runtime, &owner).await, parent);
    assert!(host.models.lock().unwrap().is_empty());
    let mut alias = model("alias-model");
    alias.provider = "alias".into();
    first.run(request(&runtime, child_owner.clone(), json!({"action":"register","providerId":"alias","implementationId":"test","models":[alias]}))).await.unwrap();
    assert_eq!(
        first
            .run(models(&runtime, &child_owner))
            .await
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        second
            .run(models(&runtime, &child_owner))
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
    first
        .run(request(
            &runtime,
            child_owner.clone(),
            json!({"action":"unregister","providerId":"test"}),
        ))
        .await
        .unwrap();
    assert_eq!(
        first.run(models(&runtime, &child_owner)).await[0]["id"],
        "alias"
    );
    assert_eq!(
        second.run(models(&runtime, &child_owner)).await[0]["models"][0]["id"],
        "second"
    );
    assert_eq!(models(&runtime, &owner).await, parent);
    assert!(first
        .run(request(
            &runtime,
            owner.clone(),
            json!({"action":"inspect"})
        ))
        .await
        .is_err());
    first.close_runtime();
    assert!(first
        .run(request(
            &runtime,
            child_owner.clone(),
            json!({"action":"inspect"})
        ))
        .await
        .is_err());
    second.run(models(&runtime, &child_owner)).await;
    assert!(!runtime
        .children
        .lock()
        .await
        .contains_key(first.runtime_id()));
    assert_eq!(models(&runtime, &owner).await, parent);
}
#[tokio::test]
async fn inherited_provider_proxies_select_the_current_child_execution_catalog() {
    let (runtime, owner, _) = fixture();
    let proxy = runtime.providers().await.remove(0);
    let child = Arc::new(ToolExecutionScope::default());
    let child_owner = NativeOwner {
        child: true,
        ..owner
    };
    child
        .run(request(
            &runtime,
            child_owner,
            json!({"action":"publish","providerId":"test","models":[model("child")]}),
        ))
        .await
        .unwrap();
    for (scope, expected) in [
        (Some(child.clone()), "captured provider"),
        (None, "unknown model"),
    ] {
        let stream = rpi_agent::tool_execution_scope::inherit(
            scope,
            proxy.stream_simple(
                &model("child"),
                &Context::default(),
                &SimpleStreamOptions::default(),
            ),
        )
        .await;
        assert_eq!(
            stream.result().await.unwrap().error_message.as_deref(),
            Some(expected)
        );
    }
    child.close_runtime();
    let stream = child
        .run(proxy.stream_simple(
            &model("child"),
            &Context::default(),
            &SimpleStreamOptions::default(),
        ))
        .await;
    assert!(stream
        .result()
        .await
        .unwrap()
        .error_message
        .unwrap()
        .contains("closing"));
}
#[tokio::test]
async fn child_runtime_capacity_is_reclaimed_only_after_child_closes() {
    let (runtime, owner, _) = fixture();
    let owner = NativeOwner {
        child: true,
        ..owner
    };
    let mut scopes = Vec::new();
    for _ in 0..64 {
        let scope = Arc::new(ToolExecutionScope::default());
        scope.run(models(&runtime, &owner)).await;
        scopes.push(scope);
    }
    let next = Arc::new(ToolExecutionScope::default());
    assert!(next
        .run(request(
            &runtime,
            owner.clone(),
            json!({"action":"inspect"})
        ))
        .await
        .unwrap_err()
        .contains("capacity"));
    scopes[0].close_runtime();
    next.run(models(&runtime, &owner)).await;
    assert_eq!(runtime.children.lock().await.len(), 64);
}

#[tokio::test]
async fn child_login_continuations_cannot_be_consumed_by_parent_or_sibling() {
    extern "C" fn login(
        _: rpi_plugin_sdk::StbStringRef,
        _: *const rpi_plugin_sdk::AuthRequestControl,
        output: *mut rpi_plugin_sdk::StbString,
        _: *mut std::ffi::c_void,
    ) -> i32 {
        unsafe {
            *output = rpi_plugin_sdk::StbString::from_string(json!({"version":1,"kind":"interaction",
            "payload":{"kind":"input","title":"Fixture key","secret":true,"continuation":"private"}}).to_string());
        }
        0
    }
    let (runtime, owner, _) = fixture_with(|registry| {
        registry.register_provider_auth(rpi_extensions::provider_auth::RegisteredAuthProvider {
            action_bridge: None,
            plugin: "test-plugin".into(),
            manifest: serde_json::from_value(
                json!({"version":1,"id":"test","displayName":"Test","apiKey":true,"oauth":false,
                "methods":[{"id":"key","name":"Key","kind":"api_key"}]}),
            )
            .unwrap(),
            request_fn: login,
            plugin_free_string: rpi_plugin_sdk::callbacks::free_string,
            user_data: std::ptr::null_mut(),
        });
    });
    let first = Arc::new(ToolExecutionScope::default());
    let second = Arc::new(ToolExecutionScope::default());
    let child_owner = NativeOwner {
        child: true,
        ..owner.clone()
    };
    let response = first
        .run(runtime.request(
            child_owner.clone(),
            json!({"port":"auth","action":"login","providerId":"test","methodId":"key"}),
            CancellationToken::new(),
        ))
        .await
        .unwrap();
    assert!(response["interaction"].get("continuation").is_none());
    let cancel = json!({"port":"auth","action":"cancelLogin","loginId":response["loginId"]});
    assert!(runtime
        .request(owner, cancel.clone(), CancellationToken::new())
        .await
        .is_err());
    assert!(second
        .run(runtime.request(
            child_owner.clone(),
            cancel.clone(),
            CancellationToken::new()
        ))
        .await
        .is_err());
    assert_eq!(
        first
            .run(runtime.request(child_owner, cancel, CancellationToken::new()))
            .await
            .unwrap()["kind"],
        "cancelled"
    );
}
