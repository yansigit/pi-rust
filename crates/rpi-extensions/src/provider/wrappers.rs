//! Session-owned provider composition. A continuation captures the next provider,
//! never a registry lookup that could recursively re-enter its own wrapper.
use super::*;
use rpi_ai::auth::{AttemptAuth, AuthError, AuthOptions, SystemClock};
use rpi_plugin_sdk::{
    AuthRequestControl, FreeStringFn, ProviderStreamControl, ProviderStreamEmitFn,
    ProviderStreamRequestFn,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{mpsc, OnceLock, Weak};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WrapperManifest {
    pub version: u32,
    pub id: String,
    /// None wraps every provider; an explicit list wraps only those identities.
    pub providers: Option<Vec<String>>,
}
impl WrapperManifest {
    pub fn parse(json: &str) -> Result<Self, String> {
        let value: Self = serde_json::from_str(json).map_err(|_| "invalid wrapper manifest")?;
        let mut seen = std::collections::HashSet::new();
        if value.version != 1
            || value.id.trim().is_empty()
            || value.providers.as_ref().is_some_and(|ids| {
                ids.iter()
                    .any(|id| id.trim().is_empty() || !seen.insert(id))
            })
        {
            return Err("invalid wrapper manifest".into());
        }
        Ok(value)
    }
}

#[derive(Clone)]
pub struct RegisteredWrapper {
    pub plugin: String,
    pub manifest: WrapperManifest,
    pub stream: ProviderStreamRequestFn,
    pub free: FreeStringFn,
    pub context: *mut c_void,
}
// SAFETY: registration requires callbacks and context callable from host workers.
unsafe impl Send for RegisteredWrapper {}
unsafe impl Sync for RegisteredWrapper {}

pub(super) struct WrapperNext {
    next: Arc<dyn Provider>,
    session: ExtensionSession,
    plugin: String,
    generation: String,
}

/// Compose a fresh chain over unwrapped providers. Calling this during catalog
/// publication rebuilds from bases, so each registration attaches exactly once.
pub fn wrap_providers(
    session: &ExtensionSession,
    providers: Vec<Arc<dyn Provider>>,
) -> Vec<Arc<dyn Provider>> {
    let Some(snapshot) = session.snapshot_arc() else {
        return providers;
    };
    providers
        .into_iter()
        .map(|mut next| {
            for wrapper in snapshot.provider_wrappers().iter().rev() {
                if wrapper
                    .manifest
                    .providers
                    .as_ref()
                    .is_some_and(|ids| !ids.iter().any(|id| id == next.id()))
                {
                    continue;
                }
                next = Arc::new(PluggableProvider {
                    record: RegisteredProvider {
                        provider_id: next.id().into(),
                        base_url: String::new(),
                        api_style: String::new(),
                        request_fn: crate::unsupported_one_shot_provider_request,
                        stream_fn: Some(wrapper.stream),
                        plugin_free_string: wrapper.free,
                        user_data: wrapper.context,
                    },
                    snapshot: snapshot.clone(),
                    _keepalive: session.keepalive(),
                    runtime: Handle::current(),
                    models: next.models().to_vec(),
                    wrapper: Some(WrapperNext {
                        next,
                        session: session.clone(),
                        plugin: wrapper.plugin.clone(),
                        generation: uuid::Uuid::new_v4().to_string(),
                    }),
                });
            }
            next
        })
        .collect()
}

static CONTINUATIONS: OnceLock<Mutex<HashMap<String, Weak<Continuation>>>> = OnceLock::new();
fn continuations() -> &'static Mutex<HashMap<String, Weak<Continuation>>> {
    CONTINUATIONS.get_or_init(Mutex::default)
}

pub(super) struct Continuation {
    id: String,
    generation: String,
    plugin: String,
    next: Arc<dyn Provider>,
    session: ExtensionSession,
    model: Model,
    context: Context,
    options: SimpleStreamOptions,
    runtime: Handle,
    receipts: Mutex<HashMap<String, AttemptAuth>>,
}
impl WrapperNext {
    pub(super) fn continuation(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> Arc<Continuation> {
        let value = Arc::new(Continuation {
            id: uuid::Uuid::new_v4().to_string(),
            generation: self.generation.clone(),
            plugin: self.plugin.clone(),
            next: self.next.clone(),
            session: self.session.clone(),
            model: model.clone(),
            context: context.clone(),
            options: options.clone(),
            runtime: Handle::current(),
            receipts: Mutex::new(HashMap::new()),
        });
        continuations()
            .lock()
            .unwrap()
            .insert(value.id.clone(), Arc::downgrade(&value));
        value
    }
}
impl Drop for Continuation {
    fn drop(&mut self) {
        continuations().lock().unwrap().remove(&self.id);
    }
}
impl Continuation {
    pub(super) fn descriptor(&self) -> Value {
        json!({"version":1,"id":self.id,"generation":self.generation})
    }
    fn active(&self) -> bool {
        !self.options.signal.is_cancelled()
            && self
                .session
                .snapshot()
                .is_some_and(|snapshot| snapshot.is_active())
    }
    fn resolve(descriptor: &Value) -> Result<Arc<Self>, String> {
        let id = descriptor["id"]
            .as_str()
            .ok_or("missing provider continuation")?;
        let value = continuations()
            .lock()
            .unwrap()
            .get(id)
            .and_then(Weak::upgrade)
            .ok_or("expired provider continuation")?;
        if descriptor["version"] != 1
            || descriptor["generation"] != value.generation
            || !value.active()
        {
            return Err("inactive provider continuation".into());
        }
        Ok(value)
    }
    async fn authenticate(
        &self,
        input: Value,
        signal: CancellationToken,
    ) -> Result<Value, AuthError> {
        let store_id = input["storeId"].as_str().ok_or(AuthError::Store)?;
        let account_id = input["accountId"].as_str().ok_or(AuthError::Store)?;
        let owned = self.session.snapshot().is_some_and(|snapshot| {
            snapshot
                .credential_stores()
                .iter()
                .any(|store| store.manifest.id == store_id && store.plugin == self.plugin)
        });
        if !owned {
            return Err(AuthError::Store);
        }
        let provider_id = &self.model.provider;
        let provider =
            crate::provider_auth::NativeProviderAuth::from_session(&self.session, provider_id)
                .ok_or(AuthError::Resolve)?;
        let store = crate::credential_store::NativeCredentialStore::from_session(
            &self.session,
            store_id,
            provider_id,
            account_id,
        )
        .ok_or(AuthError::Store)?;
        let options = if input["options"].is_null() {
            AuthOptions::default()
        } else {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields, default)]
            struct Options {
                api_key: Option<String>,
                env: std::collections::BTreeMap<String, String>,
                minimum_validity_ms: Option<i64>,
            }
            impl Default for Options {
                fn default() -> Self {
                    Self {
                        api_key: None,
                        env: Default::default(),
                        minimum_validity_ms: None,
                    }
                }
            }
            let opts: Options =
                serde_json::from_value(input["options"].clone()).map_err(|_| AuthError::Resolve)?;
            AuthOptions {
                api_key: opts.api_key,
                env: opts.env,
                minimum_validity_ms: opts.minimum_validity_ms,
                force_refresh: false,
            }
        };
        let auth = self
            .session
            .auth_runtime()
            .resolve_replacement(
                provider_id.clone(),
                provider,
                Arc::new(crate::credential_store::NativeStoreHandle(store)),
                options,
                signal,
                Arc::new(SystemClock),
            )
            .await?;
        let id = uuid::Uuid::new_v4().to_string();
        self.receipts.lock().unwrap().insert(id.clone(), auth);
        Ok(json!({"version":1,"receipt":id}))
    }
}

fn write_result(out: *mut StbString, result: Result<Value, String>) -> i32 {
    if out.is_null() {
        return 2;
    }
    let (status, value) = match result {
        Ok(value) => (0, value),
        Err(error) => (1, json!({"error":error})),
    };
    // SAFETY: callers provide a writable out slot and use the host deallocator.
    unsafe {
        *out = StbString::from_string(value.to_string());
    }
    status
}

pub(crate) extern "C" fn continuation_action(
    request: StbStringRef,
    control: *const AuthRequestControl,
    out: *mut StbString,
    _: *mut c_void,
) -> i32 {
    rpi_plugin_sdk::guard_or(1, || {
        let result = (|| {
            if !control.is_null()
                && unsafe { control.cast::<u32>().read() }
                    < std::mem::size_of::<AuthRequestControl>() as u32
            {
                return Err("short authentication cancellation control".into());
            }
            let input: Value = serde_json::from_str(unsafe { request.as_str() })
                .map_err(|_| "invalid continuation request")?;
            if input["version"] != 1 {
                return Err("unsupported continuation version".into());
            }
            let continuation = Continuation::resolve(&input["continuation"])?;
            match input["action"].as_str() {
                Some("releaseAuth") => {
                    let id = input["receipt"].as_str().ok_or("missing auth receipt")?;
                    if continuation.receipts.lock().unwrap().remove(id).is_none() {
                        return Err("unknown auth receipt".into());
                    }
                    return Ok(json!({"version":1}));
                }
                Some("resolveAuth") => {}
                _ => return Err("unknown continuation action".into()),
            }
            let signal = continuation.options.signal.child_token();
            let _cancel = signal.clone().drop_guard();
            let (sender, receiver) = mpsc::sync_channel(1);
            let pending = continuation.clone();
            let task_signal = signal.clone();
            continuation.runtime.spawn(async move {
                let result = pending
                    .authenticate(input, task_signal)
                    .await
                    .map_err(|error| error.to_string());
                let _ = sender.send(result);
            });
            loop {
                let cancelled = unsafe { control.as_ref() }
                    .is_some_and(|control| (control.is_cancelled)(control.user_data) != 0);
                if cancelled || !continuation.active() {
                    return Err("authentication cancelled".into());
                }
                match receiver.recv_timeout(Duration::from_millis(10)) {
                    Ok(result) => return result,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => return Err("authentication task stopped".into()),
                }
            }
        })();
        write_result(out, result)
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DelegateRequest {
    version: u32,
    continuation: Value,
    auth_receipt: Option<String>,
    model: Option<Model>,
    context: Option<Context>,
    options: Option<serde_json::Map<String, Value>>,
}

/// Run on the foreign caller's thread. The host runtime drives the provider,
/// while this reader stays available for private receipts, cancellation and completion.
pub(crate) extern "C" fn delegate_stream(
    request: StbStringRef,
    emit: Option<ProviderStreamEmitFn>,
    emit_data: *mut c_void,
    control: *const ProviderStreamControl,
    _: Option<rpi_plugin_sdk::ProviderStreamCompletionFn>,
    _: *mut c_void,
    out: *mut StbString,
    _: *mut c_void,
) -> i32 {
    rpi_plugin_sdk::guard_or(1, || {
        let result = (|| {
            if out.is_null() {
                return Err("missing delegated result slot".into());
            }
            let input: DelegateRequest = serde_json::from_str(unsafe { request.as_str() })
                .map_err(|_| "invalid delegated request")?;
            if input.version != 1 {
                return Err("unsupported delegation version".into());
            }
            let continuation = Continuation::resolve(&input.continuation)?;
            let model = input.model.unwrap_or_else(|| continuation.model.clone());
            if model.provider != continuation.model.provider
                || model.api != continuation.model.api
                || model.id != continuation.model.id
            {
                return Err("continuation model identity mismatch".into());
            }
            let context = input
                .context
                .unwrap_or_else(|| continuation.context.clone());
            let mut options_json = options_json(&continuation.options);
            if let Some(patch) = input.options {
                for (key, value) in patch {
                    if !options_json.as_object().unwrap().contains_key(&key)
                        || matches!(key.as_str(), "authMode" | "responseMetadata")
                    {
                        return Err("unsupported delegated option".into());
                    }
                    options_json[&key] = value;
                }
            }
            options_json["authMode"] = json!("ambient");
            let mut options = rpi_ai::provider_wire::decode_options(&options_json, &model)
                .map_err(|_| "invalid delegated options")?;
            options.authentication = if let Some(id) = input.auth_receipt {
                continuation
                    .receipts
                    .lock()
                    .unwrap()
                    .get(&id)
                    .cloned()
                    .ok_or("unknown auth receipt")?
            } else {
                continuation.options.authentication.clone()
            };
            let signal = continuation.options.signal.child_token();
            let _cancel = signal.clone().drop_guard();
            options.signal = signal.clone();
            let state = Arc::new(ProviderControlState::default());
            let (sender, mut receiver) = queue::channel(64, 4 * 1024 * 1024, state);
            let sender = Arc::new(sender);
            if emit.is_some() {
                let sender = sender.clone();
                let signal = signal.clone();
                options.response_observer = Some(Arc::new(move |receipt| {
                    let sender = sender.clone();
                    let signal = signal.clone();
                    Box::pin(async move {
                        let frame = json!({"type":"response","response":receipt});
                        let bytes = frame.to_string().len();
                        tokio::select! { biased; _ = signal.cancelled() => return, sent = sender.send_async(frame, bytes) => if sent.is_err() { return; } }
                    })
                }));
            } else {
                options.response_observer = continuation.options.response_observer.clone();
            }
            let (completed, completion) = mpsc::sync_channel(1);
            let pending = continuation.clone();
            let task_model = model.clone();
            continuation.runtime.spawn(async move {
                let message =
                    drive_delegate(&pending.next, &task_model, &context, &options, &sender).await;
                let _ = completed.send(message);
            });
            let mut mapper =
                StreamMapper::new(model.api.clone(), model.provider.clone(), model.id.clone());
            let mut finished = None;
            loop {
                let cancelled = unsafe { control.as_ref() }
                    .is_some_and(|control| (control.is_cancelled)(control.user_data, 0) != 0);
                if cancelled || !continuation.active() {
                    return serde_json::to_value(mapper.aborted_message())
                        .map_err(|_| "invalid cancelled response".into());
                }
                if finished.is_none() {
                    match completion.try_recv() {
                        Ok(message) => finished = Some(message),
                        Err(mpsc::TryRecvError::Empty) => {}
                        Err(_) => return Err("delegated provider task stopped".into()),
                    }
                }
                let frame = if finished.is_some() {
                    match receiver.try_recv() {
                        Ok(frame) => Ok(Some(frame)),
                        Err(()) => break,
                    }
                } else {
                    receiver.recv_timeout(Duration::from_millis(10))
                };
                match frame {
                    Err(()) => continue,
                    Ok(None) => break,
                    Ok(Some(frame)) => {
                        if frame["type"] != "response" {
                            mapper.apply(frame.clone());
                            while let Some(deferred) = mapper.take_deferred() {
                                mapper.apply(deferred);
                            }
                        }
                        if let Some(emit) = emit {
                            if emit(StbString::from_string(frame.to_string()), emit_data) != 0 {
                                return serde_json::to_value(mapper.aborted_message())
                                    .map_err(|_| "invalid cancelled response".into());
                            }
                        }
                    }
                }
            }
            let message = match finished {
                Some(message) => message,
                None => completion
                    .recv()
                    .map_err(|_| "delegated provider task stopped")?,
            };
            serde_json::to_value(message).map_err(|_| "invalid delegated response".into())
        })();
        write_result(out, result)
    })
}

async fn drive_delegate(
    provider: &Arc<dyn Provider>,
    model: &Model,
    context: &Context,
    options: &SimpleStreamOptions,
    sender: &queue::Sender,
) -> AssistantMessage {
    let mut partial = AssistantMessage::empty(
        model.api.clone(),
        model.provider.clone(),
        model.id.clone(),
        0,
    );
    let pending = provider.stream_simple(model, context, options);
    let mut stream = tokio::select! { biased; _ = options.signal.cancelled() => return aborted(partial), stream = pending => stream };
    let mut previous = None;
    loop {
        let event = tokio::select! { biased; _ = options.signal.cancelled() => return aborted(partial), event = stream.next() => event };
        let Some(event) = event else {
            partial.stop_reason = rpi_ai::StopReason::Error;
            partial.error_message =
                Some("delegated provider ended without a terminal event".into());
            return partial;
        };
        match event {
            AssistantMessageEvent::Done { message, .. } => return message,
            AssistantMessageEvent::Error { error, .. } => return error,
            _ => {}
        }
        let frame = rpi_ai::provider_wire::encode_event(&event, &mut previous);
        let bytes = frame.to_string().len();
        let sent = tokio::select! { biased; _ = options.signal.cancelled() => false, result = sender.send_async(frame, bytes) => result.is_ok() };
        if !sent {
            return aborted(partial);
        }
        partial = event.partial().clone();
    }
}
fn aborted(mut partial: AssistantMessage) -> AssistantMessage {
    partial.stop_reason = rpi_ai::StopReason::Aborted;
    partial.error_message = Some("provider request was cancelled".into());
    partial
}

#[cfg(test)]
mod tests;
