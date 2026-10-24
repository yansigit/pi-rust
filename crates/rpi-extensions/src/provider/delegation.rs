//! Host-only authority for v2 delegation. None of these values are wire descriptors.
#[cfg(test)]
mod guard_tests;
#[cfg(test)]
mod tests;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use rpi_ai::{auth::AttemptAuth, Model, Provider};
use tokio_util::sync::CancellationToken;

macro_rules! identity {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name(uuid::Uuid);
        impl $name {
            pub fn fresh() -> Self {
                Self(uuid::Uuid::new_v4())
            }
        }
    };
}
identity!(DelegationScopeId);
identity!(DelegationEpochId);
identity!(ProviderInstanceId);
identity!(WrapperInstanceId);

#[derive(Clone)]
struct ProviderFrame {
    provider_id: String,
    instance: ProviderInstanceId,
}

/// Immutable, invocation-local ancestry. Sibling retries never modify one another.
#[derive(Clone)]
pub struct InvocationAncestry {
    invocation_id: uuid::Uuid,
    scope: DelegationScopeId,
    epoch: DelegationEpochId,
    providers: Arc<Vec<ProviderFrame>>,
    instances: Arc<Vec<WrapperInstanceId>>,
}
impl InvocationAncestry {
    pub fn root(epoch: &DelegationEpoch) -> Self {
        Self {
            invocation_id: uuid::Uuid::new_v4(),
            scope: epoch.scope,
            epoch: epoch.id,
            providers: Arc::new(Vec::new()),
            instances: Arc::new(Vec::new()),
        }
    }
    pub fn enter_provider(&self, route: &DelegationRoute) -> Result<Self, String> {
        if self.providers.iter().any(|frame| {
            frame.provider_id == route.provider_id || frame.instance == route.provider_instance
        }) {
            return Err("provider delegation cycle".into());
        }
        let mut next = self.clone();
        Arc::make_mut(&mut next.providers).push(ProviderFrame {
            provider_id: route.provider_id.clone(),
            instance: route.provider_instance,
        });
        Ok(next)
    }
    pub fn enter_wrapper(&self, instance: WrapperInstanceId) -> Result<Self, String> {
        if self.instances.contains(&instance) {
            return Err("provider wrapper instance reentry".into());
        }
        let mut next = self.clone();
        Arc::make_mut(&mut next.instances).push(instance);
        Ok(next)
    }
    pub fn validate(&self, epoch: &DelegationEpoch) -> Result<(), String> {
        if self.scope != epoch.scope || self.epoch != epoch.id || epoch.revoked.is_cancelled() {
            return Err("delegation epoch revoked".into());
        }
        Ok(())
    }
    pub fn same_invocation(&self, other: &Self) -> bool {
        self.invocation_id == other.invocation_id
    }
}

pub struct DelegationRoute {
    provider_id: String,
    provider_instance: ProviderInstanceId,
    chain: Arc<dyn Provider>,
    models: Vec<Model>,
}
impl DelegationRoute {
    pub fn new(chain: Arc<dyn Provider>) -> Self {
        let models = chain
            .models()
            .iter()
            .cloned()
            .map(|mut model| {
                model.headers = None;
                model
            })
            .collect();
        Self {
            provider_id: chain.id().into(),
            provider_instance: ProviderInstanceId::fresh(),
            chain,
            models,
        }
    }
    pub fn chain(&self) -> Arc<dyn Provider> {
        self.chain.clone()
    }
}

pub struct DelegationEpoch {
    id: DelegationEpochId,
    scope: DelegationScopeId,
    revoked: CancellationToken,
    routes: BTreeMap<String, Arc<DelegationRoute>>,
}
impl DelegationEpoch {
    pub fn new(
        scope: DelegationScopeId,
        chains: impl IntoIterator<Item = Arc<dyn Provider>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: DelegationEpochId::fresh(),
            scope,
            revoked: CancellationToken::new(),
            routes: chains
                .into_iter()
                .map(|chain| (chain.id().into(), Arc::new(DelegationRoute::new(chain))))
                .collect(),
        })
    }
    pub fn revoke(&self) {
        self.revoked.cancel();
    }
    pub fn signal(&self) -> CancellationToken {
        self.revoked.clone()
    }
    pub fn target(
        self: &Arc<Self>,
        caller: &DelegationCaller,
        provider: &str,
        model: &str,
    ) -> Result<ResolvedDelegationTarget, String> {
        caller.ancestry.validate(self)?;
        let route = self.routes.get(provider).ok_or_else(|| format!("multiprovider: virtual backend unavailable: provider \"{provider}\" is not registered"))?;
        let model = route.models.iter().find(|candidate| candidate.id == model).cloned().ok_or_else(|| format!("multiprovider: virtual backend unavailable: provider \"{provider}\" has no model \"{model}\""))?;
        let ancestry = caller.ancestry.enter_provider(route)?;
        Ok(ResolvedDelegationTarget {
            epoch: self.clone(),
            route: route.clone(),
            model,
            ancestry,
        })
    }
    pub fn root(self: &Arc<Self>, provider: &str) -> Result<InvocationAncestry, String> {
        let ancestry = InvocationAncestry::root(self);
        ancestry.validate(self)?;
        ancestry.enter_provider(
            self.routes
                .get(provider)
                .ok_or("provider is no longer registered")?,
        )
    }
}
impl Drop for DelegationEpoch {
    fn drop(&mut self) {
        self.revoke();
    }
}

#[derive(Clone)]
pub struct DelegationCaller {
    epoch: Arc<DelegationEpoch>,
    instance: WrapperInstanceId,
    ancestry: InvocationAncestry,
}
impl DelegationCaller {
    pub fn new(
        epoch: Arc<DelegationEpoch>,
        instance: WrapperInstanceId,
        ancestry: InvocationAncestry,
    ) -> Result<Self, String> {
        ancestry.validate(&epoch)?;
        let ancestry = ancestry.enter_wrapper(instance)?;
        Ok(Self {
            epoch,
            instance,
            ancestry,
        })
    }
    pub fn validate(&self) -> Result<(), String> {
        self.ancestry.validate(&self.epoch)
    }
    pub fn epoch(&self) -> &Arc<DelegationEpoch> {
        &self.epoch
    }
    pub fn instance(&self) -> WrapperInstanceId {
        self.instance
    }
    pub fn ancestry(&self) -> InvocationAncestry {
        self.ancestry.clone()
    }
}

#[derive(Clone)]
pub struct ResolvedDelegationTarget {
    epoch: Arc<DelegationEpoch>,
    route: Arc<DelegationRoute>,
    model: Model,
    ancestry: InvocationAncestry,
}
impl ResolvedDelegationTarget {
    pub fn model(&self) -> &Model {
        &self.model
    }
    pub fn chain(&self) -> Arc<dyn Provider> {
        self.route.chain()
    }
    pub fn ancestry(&self) -> InvocationAncestry {
        self.ancestry.clone()
    }
    pub fn validate(&self, caller: &DelegationCaller) -> Result<(), String> {
        caller.validate()?;
        self.ancestry.validate(&caller.epoch)?;
        if !self.ancestry.same_invocation(&caller.ancestry) {
            return Err("delegation target belongs to a different continuation".into());
        }
        Ok(())
    }
    pub fn signal(&self) -> CancellationToken {
        self.epoch.signal()
    }
}

#[derive(Clone)]
pub struct ResolvedAmbientAuth(AttemptAuth);
impl ResolvedAmbientAuth {
    pub fn new(authentication: AttemptAuth) -> Self {
        Self(authentication)
    }
    pub fn authentication(&self) -> AttemptAuth {
        self.0.clone()
    }
}

#[async_trait::async_trait]
pub trait DelegationResolver: Send + Sync {
    async fn resolve_target(
        &self,
        caller: &DelegationCaller,
        provider_id: &str,
        model_id: &str,
        signal: CancellationToken,
    ) -> Result<ResolvedDelegationTarget, String>;
    async fn resolve_ambient(
        &self,
        caller: &DelegationCaller,
        target: &ResolvedDelegationTarget,
        signal: CancellationToken,
    ) -> Result<ResolvedAmbientAuth, String>;
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReceiptKind {
    Ambient,
    Replacement,
}
struct AuthReceiptRecord {
    target: String,
    kind: ReceiptKind,
    authentication: AttemptAuth,
}
/// Owned by exactly one continuation; never installed in the global weak registry.
#[derive(Default)]
pub struct CapabilityTables {
    targets: HashMap<String, ResolvedDelegationTarget>,
    receipts: HashMap<String, AuthReceiptRecord>,
}
impl CapabilityTables {
    pub const LIMIT: usize = 64;
    pub fn insert_target(&mut self, target: ResolvedDelegationTarget) -> Result<String, String> {
        if self.targets.len() >= Self::LIMIT {
            return Err("delegation target capacity exhausted".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.targets.insert(id.clone(), target);
        Ok(id)
    }
    pub fn target(
        &self,
        id: &str,
        caller: &DelegationCaller,
    ) -> Result<ResolvedDelegationTarget, String> {
        let target = self
            .targets
            .get(id)
            .ok_or("delegation target belongs to a different continuation")?;
        target.validate(caller)?;
        Ok(target.clone())
    }
    pub fn insert_auth(
        &mut self,
        target: &str,
        caller: &DelegationCaller,
        kind: ReceiptKind,
        authentication: AttemptAuth,
    ) -> Result<String, String> {
        self.target(target, caller)?;
        if self.receipts.len() >= Self::LIMIT {
            return Err("auth receipt capacity exhausted".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.receipts.insert(
            id.clone(),
            AuthReceiptRecord {
                target: target.into(),
                kind,
                authentication,
            },
        );
        Ok(id)
    }
    pub fn authentication(
        &self,
        receipt: &str,
        target: &str,
        caller: &DelegationCaller,
    ) -> Result<(ReceiptKind, AttemptAuth), String> {
        self.target(target, caller)?;
        let receipt = self.receipts.get(receipt).ok_or("unknown auth receipt")?;
        if receipt.target != target {
            return Err("auth receipt target mismatch".into());
        }
        Ok((receipt.kind, receipt.authentication.clone()))
    }
    pub fn release_target(&mut self, target: &str) -> Result<(), String> {
        self.targets
            .remove(target)
            .ok_or("delegation target belongs to a different continuation")?;
        self.receipts.retain(|_, receipt| receipt.target != target);
        Ok(())
    }
    pub fn release_auth(&mut self, receipt: &str) -> Result<(), String> {
        self.receipts
            .remove(receipt)
            .ok_or("unknown auth receipt")?;
        Ok(())
    }
}
