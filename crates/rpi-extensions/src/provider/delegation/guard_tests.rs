use super::tests::{caller, epoch};
use super::*;

#[test]
fn revoked_epoch_guard() {
    let epoch = epoch(DelegationScopeId::fresh());
    let a = caller(&epoch);
    epoch.revoke();
    assert_eq!(a.validate().err().unwrap(), "delegation epoch revoked");
}
#[test]
fn epoch_identity_guard() {
    let scope = DelegationScopeId::fresh();
    let first = epoch(scope);
    let second = epoch(scope);
    assert_eq!(
        caller(&first).ancestry().validate(&second).err().unwrap(),
        "delegation epoch revoked"
    );
}
#[test]
fn scope_identity_guard() {
    let first = epoch(DelegationScopeId::fresh());
    let mut second = epoch(DelegationScopeId::fresh());
    // Isolate scope checking from epoch identity checking.
    Arc::get_mut(&mut second).unwrap().id = first.id;
    assert_eq!(
        caller(&first).ancestry().validate(&second).err().unwrap(),
        "delegation epoch revoked"
    );
}
#[test]
fn invocation_identity_guard() {
    let epoch = epoch(DelegationScopeId::fresh());
    let a = caller(&epoch);
    let b = epoch.target(&a, "b", "m").unwrap();
    assert_eq!(
        b.validate(&caller(&epoch)).err().unwrap(),
        "delegation target belongs to a different continuation"
    );
}
#[test]
fn receipt_capacity_guard() {
    let epoch = epoch(DelegationScopeId::fresh());
    let a = caller(&epoch);
    let mut table = CapabilityTables::default();
    let b = table
        .insert_target(epoch.target(&a, "b", "m").unwrap())
        .unwrap();
    for _ in 0..CapabilityTables::LIMIT {
        table
            .insert_auth(&b, &a, ReceiptKind::Ambient, AttemptAuth::Ambient)
            .unwrap();
    }
    assert_eq!(
        table
            .insert_auth(&b, &a, ReceiptKind::Ambient, AttemptAuth::Ambient)
            .err()
            .unwrap(),
        "auth receipt capacity exhausted"
    );
}
