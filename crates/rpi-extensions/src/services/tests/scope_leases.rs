use super::*;
fn invocation(f: &Fixture, child: bool, epoch: &CancellationToken) -> Arc<ActionBridge> {
    let mut bridge = f.consumer.for_invocation(child, CancellationToken::new());
    Arc::get_mut(&mut bridge).unwrap().provider_epoch = Some(epoch.clone());
    bridge
}
fn acquire(f: &Fixture, bridge: &Arc<ActionBridge>, methods: Value) -> Result<String, String> {
    f.root.services.acquire_scope_lease(bridge, &json!({"service":"accounts","serviceVersion":1,"methods":methods,"capability":"services.scopeLease.v1"}))
        .map(|value| value["lease"].as_str().unwrap().to_owned())
}
async fn call(f: &Fixture, id: &str, method: &str) -> Result<Value, String> {
    f.root
        .services
        .scope_lease_request(
            f.consumer.native_owner().unwrap(),
            json!({"action":"call","lease":id,"method":method}),
            CancellationToken::new(),
        )
        .await
}
#[tokio::test]
async fn scope_lease_authority_is_provider_only_narrow_and_bounded() {
    let f = Fixture::new();
    assert_eq!(
        acquire(&f, &f.consumer, json!(["resolve"])).unwrap_err(),
        "scope service lease requires a live provider invocation"
    );
    let epoch = CancellationToken::new();
    let bridge = invocation(&f, false, &epoch);
    assert_eq!(
        acquire(&f, &bridge, json!(["publish"])).unwrap_err(),
        "scope service lease method not permitted"
    );
    let id = acquire(&f, &bridge, json!(["resolve"])).unwrap();
    assert_eq!(
        call(&f, "forged", "resolve").await.unwrap_err(),
        "unknown scope service lease"
    );
    f.root
        .services
        .state
        .lock()
        .unwrap()
        .services
        .values_mut()
        .next()
        .unwrap()
        .registration
        .manifest
        .methods
        .push("other".into());
    assert_eq!(
        call(&f, &id, "other").await.unwrap_err(),
        "scope service lease method not permitted"
    );
    assert_eq!(
        f.root
            .services
            .scope_lease_request(
                f.publisher.native_owner().unwrap(),
                json!({"action":"call","lease":id,"method":"resolve"}),
                CancellationToken::new()
            )
            .await
            .unwrap_err(),
        "scope service lease owner mismatch"
    );
    let mut wrong_generation = f.consumer.native_owner().unwrap();
    wrong_generation.generation = "different-generation".into();
    assert_eq!(
        f.root
            .services
            .scope_lease_request(
                wrong_generation,
                json!({"action":"call","lease":id,"method":"resolve"}),
                CancellationToken::new()
            )
            .await
            .unwrap_err(),
        "scope service lease owner mismatch"
    );
    for _ in 1..32 {
        acquire(&f, &bridge, json!(["resolve"])).unwrap();
    }
    assert_eq!(
        acquire(&f, &bridge, json!(["resolve"])).unwrap_err(),
        "scope service lease capacity exhausted"
    );
    f.root
        .services
        .scope_lease_request(
            f.consumer.native_owner().unwrap(),
            json!({"action":"release","lease":id}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        call(&f, &id, "resolve").await.unwrap_err(),
        "unknown scope service lease"
    );
    acquire(&f, &bridge, json!(["resolve"])).unwrap();
}
#[tokio::test]
async fn scope_lease_survives_callback_but_not_epoch_scope_or_service() {
    let f = Fixture::new();
    let epoch = CancellationToken::new();
    let scope = Arc::new(rpi_agent::tool_execution_scope::ToolExecutionScope::default());
    let completed = CancellationToken::new();
    let bridge = scope
        .run(async {
            let mut bridge = f.consumer.for_invocation(true, completed.clone());
            Arc::get_mut(&mut bridge).unwrap().provider_epoch = Some(epoch.clone());
            bridge
        })
        .await;
    let id = acquire(&f, &bridge, json!(["resolve"])).unwrap();
    completed.cancel(); // The completed callback must not own the grant's lifetime.
    drop(bridge);
    let result = call(&f, &id, "resolve").await.unwrap();
    assert_eq!(result["caller"]["child"], true);
    scope.close_runtime();
    assert!(call(&f, &id, "resolve")
        .await
        .unwrap_err()
        .contains("scope service lease"));
    let bridge = invocation(&f, false, &epoch);
    let id = acquire(&f, &bridge, json!(["resolve"])).unwrap();
    epoch.cancel();
    assert_eq!(
        call(&f, &id, "resolve").await.unwrap_err(),
        "scope service lease revoked"
    );
    assert!(acquire(&f, &bridge, json!(["resolve"])).is_err());
    let epoch = CancellationToken::new();
    let bridge = invocation(&f, false, &epoch);
    let id = acquire(&f, &bridge, json!(["resolve"])).unwrap();
    let record = f
        .root
        .services
        .state
        .lock()
        .unwrap()
        .services
        .values()
        .next()
        .unwrap()
        .registration
        .clone();
    f.root
        .services
        .remove_owner(f.publisher.native_owner().unwrap().id);
    f.root.services.attach(&[record], &f.keepalive);
    assert!(call(&f, &id, "resolve").await.is_err());
    let id = acquire(&f, &bridge, json!(["resolve"])).unwrap();
    // Simulate replacement between grant lookup and callback lookup: even if
    // cleanup hasn't cancelled the old token yet, exact instance matching wins.
    f.root
        .services
        .state
        .lock()
        .unwrap()
        .services
        .values_mut()
        .next()
        .unwrap()
        .instance = "replacement".into();
    assert_eq!(
        call(&f, &id, "resolve").await.unwrap_err(),
        "scope service lease revoked"
    );
    f.root
        .services
        .remove_owner(f.consumer.native_owner().unwrap().id);
    assert!(call(&f, &id, "resolve").await.is_err());
}
#[tokio::test]
async fn scope_lease_cancellation_does_not_revoke_other_calls_and_closure_reclaims() {
    let f = Fixture::new();
    let epoch = CancellationToken::new();
    let bridge = invocation(&f, false, &epoch);
    let id = acquire(&f, &bridge, json!(["resolve"])).unwrap();
    let signal = CancellationToken::new();
    signal.cancel();
    assert_eq!(
        f.root
            .services
            .scope_lease_request(
                f.consumer.native_owner().unwrap(),
                json!({"action":"call","lease":id,"method":"resolve"}),
                signal
            )
            .await
            .unwrap_err(),
        "service request cancelled"
    );
    call(&f, &id, "resolve").await.unwrap();
    epoch.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if f.root.services.state.lock().unwrap().grants.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn scope_lease_scope_epoch_isolation_and_session_invalidation() {
    let f = Fixture::new();
    let first = Arc::new(rpi_agent::tool_execution_scope::ToolExecutionScope::default());
    let second = Arc::new(rpi_agent::tool_execution_scope::ToolExecutionScope::default());
    let first_epoch = CancellationToken::new();
    let second_epoch = CancellationToken::new();
    let a = first
        .run(async { invocation(&f, true, &first_epoch) })
        .await;
    let b = second
        .run(async { invocation(&f, true, &second_epoch) })
        .await;
    let a = acquire(&f, &a, json!(["resolve"])).unwrap();
    let b = acquire(&f, &b, json!(["resolve"])).unwrap();
    first_epoch.cancel();
    first.close_runtime();
    assert!(call(&f, &a, "resolve").await.is_err());
    // An unrelated closed ambient scope cannot change the grant's selection.
    let result = first.run(call(&f, &b, "resolve")).await.unwrap();
    assert_eq!(result["caller"]["child"], true);
    f.root.invalidate();
    assert!(call(&f, &b, "resolve").await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scope_lease_bounds_inflight_calls_and_release_cancels_each_callback() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    extern "C" fn held(
        _: StbStringRef,
        control: *const AuthRequestControl,
        _: *mut StbString,
        context: *mut c_void,
    ) -> i32 {
        let count = unsafe { &*(context as *const AtomicUsize) };
        let control = unsafe { &*control };
        count.fetch_add(1, Ordering::SeqCst);
        while (control.is_cancelled)(control.user_data) == 0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        count.fetch_sub(1, Ordering::SeqCst);
        1
    }
    let f = Fixture::new();
    let count = Arc::new(AtomicUsize::new(0));
    {
        let mut state = f.root.services.state.lock().unwrap();
        let callback = &mut state
            .services
            .values_mut()
            .next()
            .unwrap()
            .registration
            .callback;
        callback.request = held;
        callback.context = Arc::as_ptr(&count) as *mut c_void;
    }
    let epoch = CancellationToken::new();
    let bridge = invocation(&f, false, &epoch);
    let id = acquire(&f, &bridge, json!(["resolve"])).unwrap();
    let mut calls = Vec::new();
    for _ in 0..8 {
        let services = f.root.services.clone();
        let owner = f.consumer.native_owner().unwrap();
        let id = id.clone();
        calls.push(tokio::spawn(async move {
            services
                .scope_lease_request(
                    owner,
                    json!({"action":"call","lease":id,"method":"resolve"}),
                    CancellationToken::new(),
                )
                .await
        }));
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while count.load(Ordering::SeqCst) != 8 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        call(&f, &id, "resolve").await.unwrap_err(),
        "scope service lease capacity exhausted"
    );
    f.root
        .services
        .release_scope_lease(&f.consumer.native_owner().unwrap(), &json!({"lease":id}))
        .unwrap();
    for call in calls {
        assert_eq!(
            call.await.unwrap().unwrap_err(),
            "scope service lease revoked"
        );
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while count.load(Ordering::SeqCst) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scope_lease_acquire_racing_scope_closing_cannot_resurrect_authority() {
    let f = Fixture::new();
    for _ in 0..32 {
        let epoch = CancellationToken::new();
        let scope = Arc::new(rpi_agent::tool_execution_scope::ToolExecutionScope::default());
        let bridge = scope.run(async { invocation(&f, true, &epoch) }).await;
        let closing = scope.clone();
        let closer = tokio::spawn(async move {
            closing.close_runtime();
        });
        let grant = acquire(&f, &bridge, json!(["resolve"]));
        closer.await.unwrap();
        if let Ok(id) = grant {
            assert!(call(&f, &id, "resolve").await.is_err());
        }
        assert_eq!(
            acquire(&f, &bridge, json!(["resolve"])).unwrap_err(),
            "scope service lease requires a live provider invocation"
        );
    }
}
