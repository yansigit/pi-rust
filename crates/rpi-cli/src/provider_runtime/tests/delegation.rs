use super::*;
use rpi_extensions::delegation::{DelegationCaller, InvocationAncestry, WrapperInstanceId};

async fn caller(runtime: &ProviderRuntime) -> DelegationCaller {
    let epoch = runtime.state.lock().await.epoch.clone();
    let ancestry = InvocationAncestry::root(&epoch);
    DelegationCaller::new(epoch, WrapperInstanceId::fresh(), ancestry).unwrap()
}
#[tokio::test]
async fn successful_publication_revokes_but_failed_install_keeps_epoch() {
    let (runtime, owner, host) = fixture();
    let caller = caller(&runtime).await;
    let target = runtime
        .resolve_target(&caller, "test", "old", CancellationToken::new())
        .await
        .unwrap();
    host.fail.store(true, Ordering::SeqCst);
    assert!(request(
        &runtime,
        owner.clone(),
        json!({"action":"publish","providerId":"test","models":[model("new")]})
    )
    .await
    .is_err());
    assert!(target.validate(&caller).is_ok());
    assert!(runtime
        .resolve_target(&caller, "test", "old", CancellationToken::new())
        .await
        .is_ok());
    host.fail.store(false, Ordering::SeqCst);
    request(
        &runtime,
        owner,
        json!({"action":"publish","providerId":"test","models":[model("new")]}),
    )
    .await
    .unwrap();
    assert!(target.signal().is_cancelled());
    assert_eq!(
        target.validate(&caller).err().unwrap(),
        "delegation epoch revoked"
    );
    let current = super::delegation::caller(&runtime).await;
    assert!(runtime
        .resolve_target(&current, "test", "new", CancellationToken::new())
        .await
        .is_ok());
}
#[tokio::test]
async fn retry_targets_accept_selected_scope_and_reject_other_scope_or_revoked_epoch() {
    let (runtime, owner, _) = fixture();
    let selected = caller(&runtime).await;
    for _ in 0..2 {
        let target = runtime
            .resolve_target(&selected, "test", "old", CancellationToken::new())
            .await
            .unwrap();
        target.validate(&selected).unwrap();
    }
    let (other, _, _) = fixture();
    let foreign = caller(&other).await;
    assert!(runtime
        .resolve_target(&foreign, "test", "old", CancellationToken::new())
        .await
        .is_err());
    request(
        &runtime,
        owner,
        json!({"action":"publish","providerId":"test","models":[model("new")]}),
    )
    .await
    .unwrap();
    assert_eq!(
        runtime
            .resolve_target(&selected, "test", "old", CancellationToken::new())
            .await
            .err()
            .unwrap(),
        "delegation epoch revoked"
    );
    let fresh = caller(&runtime).await;
    runtime
        .resolve_target(&fresh, "test", "new", CancellationToken::new())
        .await
        .unwrap();
}
#[tokio::test]
async fn runtime_drop_revokes_retained_targets() {
    let (runtime, _, _) = fixture();
    let caller = caller(&runtime).await;
    let target = runtime
        .resolve_target(&caller, "test", "old", CancellationToken::new())
        .await
        .unwrap();
    drop(runtime);
    assert!(target.signal().is_cancelled());
}
