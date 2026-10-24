use super::*;
use rpi_ai::{AssistantMessageEventStream, Context, SimpleStreamOptions};

struct Base {
    id: String,
    models: Vec<Model>,
}
#[async_trait::async_trait]
impl Provider for Base {
    fn id(&self) -> &str {
        &self.id
    }
    fn models(&self) -> &[Model] {
        &self.models
    }
    async fn stream_simple(
        &self,
        _: &Model,
        _: &Context,
        _: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        panic!("authority tests must not dispatch")
    }
}
pub(super) fn epoch(scope: DelegationScopeId) -> Arc<DelegationEpoch> {
    DelegationEpoch::new(
        scope,
        ["a", "b", "c"].into_iter().map(|id| {
            Arc::new(Base {
                id: id.into(),
                models: vec![Model::new("m", "M", rpi_ai::Api::Faux, id, "https://bound")],
            }) as Arc<dyn Provider>
        }),
    )
}
pub(super) fn caller(epoch: &Arc<DelegationEpoch>) -> DelegationCaller {
    DelegationCaller::new(
        epoch.clone(),
        WrapperInstanceId::fresh(),
        epoch.root("a").unwrap(),
    )
    .unwrap()
}
#[test]
fn provider_cycles_and_retry_branches() {
    let epoch = epoch(DelegationScopeId::fresh());
    let a = caller(&epoch);
    assert_eq!(
        epoch.target(&a, "a", "m").err().unwrap(),
        "provider delegation cycle"
    );
    let b = epoch.target(&a, "b", "m").unwrap();
    let b_caller =
        DelegationCaller::new(epoch.clone(), WrapperInstanceId::fresh(), b.ancestry()).unwrap();
    assert_eq!(
        epoch.target(&b_caller, "a", "m").err().unwrap(),
        "provider delegation cycle"
    );
    assert!(epoch.target(&b_caller, "c", "m").is_ok());
    assert!(epoch.target(&a, "b", "m").is_ok());
    assert!(epoch.target(&caller(&epoch), "b", "m").is_ok());
}
#[test]
fn wrapper_guard_is_instance_local() {
    let epoch = epoch(DelegationScopeId::fresh());
    let a = caller(&epoch);
    assert_eq!(
        a.ancestry().enter_wrapper(a.instance()).err().unwrap(),
        "provider wrapper instance reentry"
    );
    assert!(a
        .ancestry()
        .enter_wrapper(WrapperInstanceId::fresh())
        .is_ok());
    // Independent invocations can enter exactly the same composed node.
    assert!(DelegationCaller::new(epoch.clone(), a.instance(), epoch.root("a").unwrap()).is_ok());
}
#[test]
fn capabilities_are_continuation_target_scope_and_epoch_bound() {
    let scope = DelegationScopeId::fresh();
    let epoch = epoch(scope);
    let a = caller(&epoch);
    let mut table = CapabilityTables::default();
    let b = table
        .insert_target(epoch.target(&a, "b", "m").unwrap())
        .unwrap();
    let c = table
        .insert_target(epoch.target(&a, "c", "m").unwrap())
        .unwrap();
    let receipt = table
        .insert_auth(&b, &a, ReceiptKind::Ambient, AttemptAuth::Ambient)
        .unwrap();
    assert!(table.authentication(&receipt, &b, &a).is_ok());
    assert_eq!(
        table.authentication(&receipt, &c, &a).err().unwrap(),
        "auth receipt target mismatch"
    );
    assert_eq!(
        table.target(&b, &caller(&epoch)).err().unwrap(),
        "delegation target belongs to a different continuation"
    );
    let other = super::tests::epoch(scope);
    assert_eq!(
        table.target(&b, &caller(&other)).err().unwrap(),
        "delegation epoch revoked"
    );
    let child = super::tests::epoch(DelegationScopeId::fresh());
    assert_eq!(
        table.target(&b, &caller(&child)).err().unwrap(),
        "delegation epoch revoked"
    );
    epoch.revoke();
    assert_eq!(
        table.authentication(&receipt, &b, &a).err().unwrap(),
        "delegation epoch revoked"
    );
    assert!(caller(&other).validate().is_ok());
}
#[test]
fn finite_tables_and_release_cascade() {
    let epoch = epoch(DelegationScopeId::fresh());
    let a = caller(&epoch);
    let mut table = CapabilityTables::default();
    let b = table
        .insert_target(epoch.target(&a, "b", "m").unwrap())
        .unwrap();
    for _ in 0..CapabilityTables::LIMIT {
        table
            .insert_auth(&b, &a, ReceiptKind::Replacement, AttemptAuth::Ambient)
            .unwrap();
    }
    assert_eq!(
        table
            .insert_auth(&b, &a, ReceiptKind::Ambient, AttemptAuth::Ambient)
            .err()
            .unwrap(),
        "auth receipt capacity exhausted"
    );
    table.release_target(&b).unwrap();
    assert!(table.receipts.is_empty());
    assert!(table.target(&b, &a).is_err());
    for _ in 0..CapabilityTables::LIMIT {
        table
            .insert_target(epoch.target(&a, "b", "m").unwrap())
            .unwrap();
    }
    assert_eq!(
        table
            .insert_target(epoch.target(&a, "b", "m").unwrap())
            .err()
            .unwrap(),
        "delegation target capacity exhausted"
    );
}
#[test]
fn lookup_errors_preserve_typescript_strings() {
    let epoch = epoch(DelegationScopeId::fresh());
    let a = caller(&epoch);
    assert_eq!(
        epoch.target(&a, "missing", "m").err().unwrap(),
        "multiprovider: virtual backend unavailable: provider \"missing\" is not registered"
    );
    assert_eq!(
        epoch.target(&a, "b", "missing").err().unwrap(),
        "multiprovider: virtual backend unavailable: provider \"b\" has no model \"missing\""
    );
}
