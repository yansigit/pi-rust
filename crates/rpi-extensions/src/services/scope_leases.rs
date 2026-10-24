//! Narrow post-invocation authority. No entry is an ActionBridge handle.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Semaphore;

const MAX_GRANTS: usize = 1024;
const MAX_OWNER_GRANTS: usize = 32;
const MAX_CALLS: usize = 256;

pub(super) struct Grant {
    pub(super) owner: NativeOwner,
    scope: Option<Weak<rpi_agent::tool_execution_scope::ToolExecutionScope>>,
    closing: Option<CancellationToken>,
    epoch: CancellationToken,
    active_session: Arc<AtomicBool>,
    operation: Option<rpi_ai::identity::RequestIdentity>,
    service: (String, u32),
    instance: String,
    service_stopped: CancellationToken,
    methods: Vec<String>,
    pub(super) revoked: CancellationToken,
    calls: Arc<Semaphore>,
}
impl Grant {
    pub(super) fn active(&self) -> bool {
        !self.revoked.is_cancelled()
            && !self.epoch.is_cancelled()
            && !self.service_stopped.is_cancelled()
            && self.active_session.load(Ordering::SeqCst)
            && !self
                .closing
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            && self
                .scope
                .as_ref()
                .map_or(true, |scope| scope.strong_count() > 0)
    }
}
impl Drop for Grant {
    fn drop(&mut self) {
        self.revoked.cancel();
    }
}
impl Services {
    pub(crate) fn release_scope_lease(
        &self,
        owner: &NativeOwner,
        args: &Value,
    ) -> Result<Value, String> {
        let id = string(args, "lease")?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let grant = state.grants.get(id).ok_or("unknown scope service lease")?;
        if !same_registration(&grant.owner, owner) {
            return Err("scope service lease owner mismatch".into());
        }
        grant.revoked.cancel();
        state.grants.remove(id);
        Ok(Value::Null)
    }
    pub(crate) fn acquire_scope_lease(
        self: &Arc<Self>,
        bridge: &Arc<ActionBridge>,
        args: &Value,
    ) -> Result<Value, String> {
        let epoch = bridge
            .provider_epoch
            .clone()
            .ok_or("scope service lease requires a live provider invocation")?;
        let owner = bridge
            .native_owner()
            .ok_or("scope service lease requires a live provider invocation")?;
        let key = service_key(args)?;
        let methods: Vec<String> = serde_json::from_value(args["methods"].clone())
            .map_err(|_| "scope service lease method not permitted")?;
        // A capability receipt is explicit, not inferred from a tool name.
        if args["capability"] != "services.scopeLease.v1" {
            return Err("scope service lease capability not negotiated".into());
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.grants.retain(|_, grant| {
            let active = grant.active();
            if !active {
                grant.revoked.cancel();
            }
            active
        });
        if state.closed
            || state.retired_owners.contains(&owner.id)
            || !bridge.is_active()
            || bridge.invocation_key().is_none()
            || bridge.native_signal().is_cancelled()
            || epoch.is_cancelled()
            || bridge
                .tool_scope
                .as_ref()
                .is_some_and(|scope| scope.runtime_closing().is_cancelled())
            || (owner.child && bridge.tool_scope.is_none())
        {
            return Err("scope service lease requires a live provider invocation".into());
        }
        if state.grants.len() >= MAX_GRANTS
            || state
                .grants
                .values()
                .filter(|g| same_registration(&g.owner, &owner))
                .count()
                >= MAX_OWNER_GRANTS
        {
            return Err("scope service lease capacity exhausted".into());
        }
        let service = state.services.get(&key).ok_or("service unavailable")?;
        if methods.is_empty()
            || methods.len() > 128
            || methods
                .iter()
                .any(|method| !service.registration.manifest.methods.contains(method))
        {
            return Err("scope service lease method not permitted".into());
        }
        let grant = Arc::new(Grant {
            owner,
            scope: bridge.tool_scope.as_ref().map(Arc::downgrade),
            closing: bridge
                .tool_scope
                .as_ref()
                .map(|scope| scope.runtime_closing()),
            epoch,
            active_session: bridge.active.clone(),
            operation: bridge.operation.clone(),
            service: key,
            instance: service.instance.clone(),
            service_stopped: service.stopped.clone(),
            methods,
            revoked: CancellationToken::new(),
            calls: Arc::new(Semaphore::new(8)),
        });
        let id = uuid::Uuid::new_v4().to_string();
        state.grants.insert(id.clone(), grant);
        drop(state);
        let weak = Arc::downgrade(self);
        let watch_id = id.clone();
        bridge.runtime.spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                let Some(services) = weak.upgrade() else {
                    break;
                };
                let mut state = services.state.lock().unwrap_or_else(|e| e.into_inner());
                match state.grants.get(&watch_id) {
                    Some(grant) if grant.active() => {}
                    _ => {
                        if let Some(grant) = state.grants.remove(&watch_id) {
                            grant.revoked.cancel();
                        }
                        break;
                    }
                }
            }
        });
        Ok(json!({"lease":id}))
    }

    pub(crate) async fn scope_lease_request(
        self: &Arc<Self>,
        owner: NativeOwner,
        args: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        let id = string(&args, "lease")?;
        let grant = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let grant = state
                .grants
                .get(id)
                .ok_or("unknown scope service lease")?
                .clone();
            if !same_registration(&grant.owner, &owner) {
                return Err("scope service lease owner mismatch".into());
            }
            if !grant.active() {
                grant.revoked.cancel();
                state.grants.remove(id);
                return Err("scope service lease revoked".into());
            }
            if args["action"] == "release" {
                grant.revoked.cancel();
                state.grants.remove(id);
                return Ok(Value::Null);
            }
            if args["action"] != "call" {
                return Err("unsupported scope service lease action".into());
            }
            if !grant
                .methods
                .iter()
                .any(|method| Some(method.as_str()) == args["method"].as_str())
            {
                return Err("scope service lease method not permitted".into());
            }
            grant
        };
        static CALLS: std::sync::OnceLock<Arc<Semaphore>> = std::sync::OnceLock::new();
        let global = CALLS
            .get_or_init(|| Arc::new(Semaphore::new(MAX_CALLS)))
            .clone()
            .try_acquire_owned()
            .map_err(|_| "scope service lease capacity exhausted")?;
        let local = grant
            .calls
            .clone()
            .try_acquire_owned()
            .map_err(|_| "scope service lease capacity exhausted")?;
        let scope = match &grant.scope {
            Some(scope) => Some(scope.upgrade().ok_or("scope service lease revoked")?),
            None => None,
        };
        let closing = grant.closing.clone().unwrap_or_default();
        let call_signal = signal.child_token();
        let _cancel = call_signal.clone().drop_guard();
        let work = self.call_instance(grant.owner.clone(), json!({"service":grant.service.0,"serviceVersion":grant.service.1,"method":args["method"],"input":args["input"]}), call_signal.clone(), Some(&grant.instance), Some(Box::new((global, local))));
        let work = rpi_agent::tool_execution_scope::inherit(scope, async {
            match grant.operation.clone() {
                Some(identity) => rpi_ai::identity::scope(identity, work).await,
                None => work.await,
            }
        });
        tokio::select! {
            biased;
            _ = grant.revoked.cancelled() => Err("scope service lease revoked".into()),
            _ = grant.epoch.cancelled() => Err("scope service lease revoked".into()),
            _ = closing.cancelled() => Err("scope service lease revoked".into()),
            _ = grant.service_stopped.cancelled() => Err("scope service lease revoked".into()),
            _ = signal.cancelled() => Err("service request cancelled".into()),
            result = work => result,
        }
    }
}
