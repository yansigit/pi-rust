//! Session-owned native services. Replies are private; notifications are bounded
//! and acknowledged only after every current subscriber can accept the event.
use crate::{native::NativeOwner, private_callback::Callback, ActionBridge, PluginKeepalive};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const MAX_EVENTS: usize = 64;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_SUBSCRIPTIONS: usize = 1024;
const MAX_OWNER_SUBSCRIPTIONS: usize = 128;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceManifest {
    pub version: u32,
    pub id: String,
    pub service_version: u32,
    pub methods: Vec<String>,
    #[serde(default)]
    pub topics: Vec<String>,
}
impl ServiceManifest {
    pub fn parse(input: &str) -> Result<Self, String> {
        let value: Self = serde_json::from_str(input).map_err(|_| "invalid service manifest")?;
        fn names(names: &[String]) -> bool {
            let mut seen = std::collections::HashSet::new();
            names.len() <= 128
                && names
                    .iter()
                    .all(|name| valid_name(name) && seen.insert(name))
        }
        if value.version != 1
            || value.service_version == 0
            || !valid_name(&value.id)
            || !names(&value.methods)
            || !names(&value.topics)
        {
            return Err("invalid service manifest".into());
        }
        Ok(value)
    }
}
fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
}

#[derive(Clone)]
pub(crate) struct RegisteredService {
    pub manifest: ServiceManifest,
    pub callback: Callback,
    pub bridge: Weak<ActionBridge>,
    pub owner: NativeOwner,
}
struct Service {
    registration: RegisteredService,
    keepalive: Weak<PluginKeepalive>,
    stopped: CancellationToken,
    sequence: u64,
}
struct Subscription {
    owner: NativeOwner,
    scope: Option<Weak<rpi_agent::tool_execution_scope::ToolExecutionScope>>,
    stopped: Option<CancellationToken>,
    service: (String, u32),
    topic: String,
    events: VecDeque<(Arc<Value>, usize)>,
    bytes: usize,
    waiting: bool,
}
#[derive(Default)]
struct State {
    services: BTreeMap<(String, u32), Service>,
    subscriptions: BTreeMap<String, Subscription>,
    closed: bool,
}
#[derive(Default)]
pub(crate) struct Services {
    state: Mutex<State>,
    changed: Notify,
}
impl Services {
    pub(crate) fn attach(&self, records: &[RegisteredService], keepalive: &Arc<PluginKeepalive>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return;
        }
        for record in records {
            state
                .services
                .entry((record.manifest.id.clone(), record.manifest.service_version))
                .or_insert_with(|| Service {
                    registration: record.clone(),
                    keepalive: Arc::downgrade(keepalive),
                    stopped: CancellationToken::new(),
                    sequence: 0,
                });
        }
    }
    fn prune(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let before = state.subscriptions.len();
        state.subscriptions.retain(|_, sub| {
            sub.scope
                .as_ref()
                .map_or(true, |scope| scope.strong_count() > 0)
        });
        if state.subscriptions.len() != before {
            self.changed.notify_waiters();
        }
    }
    pub(crate) fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        for service in state.services.values() {
            service.stopped.cancel();
        }
        state.services.clear();
        state.subscriptions.clear();
        self.changed.notify_waiters();
    }
    pub(crate) fn remove_owner(&self, owner: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.services.retain(|_, service| {
            if service.registration.owner.id == owner {
                service.stopped.cancel();
                false
            } else {
                true
            }
        });
        let State {
            services,
            subscriptions,
            ..
        } = &mut *state;
        subscriptions.retain(|_, sub| sub.owner.id != owner && services.contains_key(&sub.service));
        self.changed.notify_waiters();
    }
    pub(crate) async fn request(
        self: &Arc<Self>,
        owner: NativeOwner,
        args: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        if signal.is_cancelled() {
            return Err("service request cancelled".into());
        }
        self.prune();
        let action = args["action"].as_str().ok_or("missing service action")?;
        if action == "next" {
            return self
                .next(&owner, string(&args, "subscription")?, signal)
                .await;
        }
        if action == "publish" {
            return self.publish(&owner, &args, signal).await;
        }
        if action == "call" {
            return self.call(owner, args, signal).await;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err("service runtime closed".into());
        }
        match action {
            "list" => Ok(json!(state
                .services
                .values()
                .map(|s| &s.registration.manifest)
                .collect::<Vec<_>>())),
            "subscribe" => {
                let key = service_key(&args)?;
                let topic = string(&args, "topic")?;
                let service = state.services.get(&key).ok_or("service unavailable")?;
                if !service
                    .registration
                    .manifest
                    .topics
                    .iter()
                    .any(|known| known == topic)
                {
                    return Err("unknown service topic".into());
                }
                if state.subscriptions.len() >= MAX_SUBSCRIPTIONS
                    || state
                        .subscriptions
                        .values()
                        .filter(|s| same_owner(&s.owner, &owner))
                        .count()
                        >= MAX_OWNER_SUBSCRIPTIONS
                {
                    return Err("service subscription limit reached".into());
                }
                let id = uuid::Uuid::new_v4().to_string();
                state.subscriptions.insert(
                    id.clone(),
                    Subscription {
                        owner,
                        stopped: rpi_agent::tool_execution_scope::current_scope()
                            .map(|scope| scope.stopped()),
                        scope: rpi_agent::tool_execution_scope::current_scope()
                            .as_ref()
                            .map(Arc::downgrade),
                        service: key,
                        topic: topic.into(),
                        events: VecDeque::new(),
                        bytes: 0,
                        waiting: false,
                    },
                );
                Ok(json!({"subscription":id}))
            }
            "unsubscribe" => {
                let id = string(&args, "subscription")?;
                let sub = state
                    .subscriptions
                    .get(id)
                    .ok_or("subscription unavailable")?;
                if !subscription_owner(sub, &owner) {
                    return Err("subscription owner mismatch".into());
                }
                state.subscriptions.remove(id);
                self.changed.notify_waiters();
                Ok(Value::Null)
            }
            _ => Err("unsupported service action".into()),
        }
    }
    async fn call(
        &self,
        owner: NativeOwner,
        args: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        let key = service_key(&args)?;
        let method = string(&args, "method")?;
        let (record, keepalive, stopped, bridge) = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let service = state.services.get(&key).ok_or("service unavailable")?;
            if !service
                .registration
                .manifest
                .methods
                .iter()
                .any(|known| known == method)
            {
                return Err("unknown service method".into());
            }
            let bridge = service
                .registration
                .bridge
                .upgrade()
                .ok_or("service owner expired")?;
            if !bridge.is_active() {
                return Err("service owner inactive".into());
            }
            (
                service.registration.clone(),
                service.keepalive.upgrade().ok_or("service owner expired")?,
                service.stopped.clone(),
                bridge,
            )
        };
        let invocation = bridge.for_invocation(owner.child, signal.clone());
        let input = json!({"version":1,"service":key.0,"serviceVersion":key.1,"method":method,
            "input":args.get("input").cloned().unwrap_or(Value::Null),"caller":owner,
            "identity":rpi_ai::identity::current()})
        .to_string();
        let response = tokio::select! {
            biased;
            _ = stopped.cancelled() => return Err("service owner unloaded".into()),
            response = crate::private_callback::invoke_scoped(record.callback, keepalive, input, signal, false, Some(invocation)) => response.map_err(|_| "service callback failed or cancelled")?,
        };
        if response["version"] != 1 {
            return Err("unsupported service response".into());
        }
        if let Some(error) = response["error"].as_str() {
            return Err(error.into());
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| "service callback omitted its result".into())
    }
    async fn publish(
        &self,
        owner: &NativeOwner,
        args: &Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        let key = service_key(args)?;
        let topic = string(args, "topic")?;
        let payload = args
            .get("value")
            .ok_or("missing service notification value")?;
        // Include the complete envelope, including the largest sequence representation.
        let bytes = json!({"sequence":u64::MAX,"topic":topic,"value":payload})
            .to_string()
            .len();
        if bytes > MAX_BYTES {
            return Err("service notification exceeds byte limit".into());
        }
        loop {
            self.prune();
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let stopped;
            {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if signal.is_cancelled() {
                    return Err("service publication cancelled".into());
                }
                let service = state.services.get(&key).ok_or("service unavailable")?;
                if !same_registration(&service.registration.owner, owner) {
                    return Err("service owner mismatch".into());
                }
                if !service
                    .registration
                    .manifest
                    .topics
                    .iter()
                    .any(|known| known == topic)
                {
                    return Err("unknown service topic".into());
                }
                let matches = |sub: &&mut Subscription| sub.service == key && sub.topic == topic;
                let full = state.subscriptions.values().any(|sub| {
                    sub.service == key
                        && sub.topic == topic
                        && (sub.events.len() >= MAX_EVENTS || bytes > MAX_BYTES - sub.bytes)
                });
                stopped = state
                    .subscriptions
                    .values()
                    .filter(|sub| sub.service == key && sub.topic == topic)
                    .filter_map(|sub| sub.stopped.clone())
                    .collect::<Vec<_>>();
                if !full {
                    let service = state.services.get_mut(&key).unwrap();
                    service.sequence = service
                        .sequence
                        .checked_add(1)
                        .ok_or("service sequence exhausted")?;
                    let sequence = service.sequence;
                    let event =
                        Arc::new(json!({"sequence":sequence,"topic":topic,"value":payload}));
                    for sub in state.subscriptions.values_mut().filter(matches) {
                        sub.events.push_back((event.clone(), bytes));
                        sub.bytes += bytes;
                    }
                    self.changed.notify_waiters();
                    return Ok(json!({"sequence":sequence}));
                }
            }
            tokio::select! {
                _ = signal.cancelled() => return Err("service publication cancelled".into()),
                _ = changed => {},
                _ = wait_stopped(stopped) => {},
            }
        }
    }
    async fn next(
        self: &Arc<Self>,
        owner: &NativeOwner,
        id: &str,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let sub = state
                .subscriptions
                .get_mut(id)
                .ok_or("subscription unavailable")?;
            if !subscription_owner(sub, owner) {
                return Err("subscription owner mismatch".into());
            }
            if sub.waiting {
                return Err("subscription already has a waiting reader".into());
            }
            sub.waiting = true;
        }
        let _reader = Reader {
            services: self.clone(),
            id: id.into(),
        };
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if signal.is_cancelled() {
                    return Err("service subscription cancelled".into());
                }
                let sub = state
                    .subscriptions
                    .get_mut(id)
                    .ok_or("subscription unavailable")?;
                if let Some((event, bytes)) = sub.events.pop_front() {
                    sub.bytes -= bytes;
                    self.changed.notify_waiters();
                    return Ok((*event).clone());
                }
            }
            tokio::select! {
                _ = signal.cancelled() => return Err("service subscription cancelled".into()),
                _ = changed => {},
            }
        }
    }
}
struct Reader {
    services: Arc<Services>,
    id: String,
}
impl Drop for Reader {
    fn drop(&mut self) {
        if let Some(sub) = self
            .services
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .subscriptions
            .get_mut(&self.id)
        {
            sub.waiting = false;
        }
    }
}
async fn wait_stopped(tokens: Vec<CancellationToken>) {
    let mut pending: futures::stream::FuturesUnordered<_> = tokens
        .into_iter()
        .map(|token| token.cancelled_owned())
        .collect();
    if pending.is_empty() {
        std::future::pending::<()>().await;
    }
    pending.next().await;
}
fn subscription_owner(sub: &Subscription, owner: &NativeOwner) -> bool {
    if !same_owner(&sub.owner, owner) {
        return false;
    }
    match (&sub.scope, rpi_agent::tool_execution_scope::current_scope()) {
        (None, None) => true,
        (Some(expected), Some(actual)) => expected.ptr_eq(&Arc::downgrade(&actual)),
        _ => false,
    }
}
fn same_registration(left: &NativeOwner, right: &NativeOwner) -> bool {
    left.generation == right.generation && left.id == right.id && left.plugin == right.plugin
}
fn same_owner(left: &NativeOwner, right: &NativeOwner) -> bool {
    same_registration(left, right) && left.child == right.child
}
fn string<'a>(args: &'a Value, field: &str) -> Result<&'a str, String> {
    args[field]
        .as_str()
        .ok_or_else(|| format!("missing service {field}"))
}
fn service_key(args: &Value) -> Result<(String, u32), String> {
    let version = args["serviceVersion"]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| *v > 0)
        .ok_or("invalid service version")?;
    Ok((string(args, "service")?.into(), version))
}

#[cfg(test)]
mod tests;
