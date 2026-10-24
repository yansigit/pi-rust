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
    assert_eq!(
        call(&f, &id, "publish").await.unwrap_err(),
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
    let bridge = scope.run(async { invocation(&f, true, &epoch) }).await;
    let id = acquire(&f, &bridge, json!(["resolve"])).unwrap();
    bridge.native_signal().cancel(); // Only a child token, not the grant's lifetime.
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
