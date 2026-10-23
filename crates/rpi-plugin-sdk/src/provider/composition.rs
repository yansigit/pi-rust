//! Native wrapper adapters. The host owns routing and resolves account credentials;
//! wrappers receive opaque receipts, never an authority-bearing model descriptor.
use super::*;
use crate::callbacks::OwnedString;
use rpi_ai::provider_wire::{encode_options, StreamMapper};
use serde_json::{json, Value};

#[derive(Clone)]
pub struct Continuation {
    api: PluginApi,
    descriptor: Value,
    model: Model,
}

/// Only valid with the continuation that issued it. Deliberately not Debug/Serialize.
#[derive(Clone)]
pub struct AuthReceipt(String);

impl Continuation {
    /// Resolves through the registered provider and the wrapper's account store.
    pub async fn resolve_auth(
        &self,
        store_id: &str,
        account_id: &str,
        options: &SimpleStreamOptions,
    ) -> Result<AuthReceipt, String> {
        let callback = self
            .api
            .provider_continuation
            .ok_or("host lacks private provider continuation operations")?;
        let signal = options.signal.child_token();
        let _cancel = signal.clone().drop_guard();
        let request = json!({"version":1,"continuation":self.descriptor,"action":"resolveAuth",
            "storeId":store_id,"accountId":account_id,"options":{"env":options.env}})
        .to_string();
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let options = SimpleStreamOptions::default().with_signal(signal);
            let control = crate::AuthRequestControl {
                struct_size: std::mem::size_of::<crate::AuthRequestControl>() as u32,
                is_cancelled: auth_cancelled,
                user_data: (&options as *const SimpleStreamOptions).cast_mut().cast(),
            };
            let mut out = StbString::empty();
            let status = callback(
                StbStringRef::from_str(&request),
                &control,
                &mut out,
                this.api.user_data,
            );
            let result = host_result(&this.api, out, status)?;
            if result["version"] != 1 {
                return Err("unsupported auth receipt".into());
            }
            Ok(AuthReceipt(
                result["receipt"]
                    .as_str()
                    .ok_or("missing auth receipt")?
                    .to_owned(),
            ))
        })
        .await
        .map_err(|_| "account authentication task stopped".to_owned())?
    }

    pub async fn release_auth(&self, receipt: AuthReceipt) -> Result<(), String> {
        let callback = self
            .api
            .provider_continuation
            .ok_or("host lacks private provider continuation operations")?;
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let request = json!({"version":1,"continuation":this.descriptor,"action":"releaseAuth","receipt":receipt.0}).to_string();
            let mut out = StbString::empty();
            let status = callback(StbStringRef::from_str(&request), std::ptr::null(), &mut out, this.api.user_data);
            host_result(&this.api, out, status).map(|_| ())
        }).await.map_err(|_| "account release task stopped".to_owned())?
    }

    pub fn provider(&self, receipt: Option<AuthReceipt>) -> Arc<dyn Provider> {
        Arc::new(DelegatedProvider {
            continuation: self.clone(),
            receipt,
            models: vec![self.model.clone()],
        })
    }
}

fn host_result(api: &PluginApi, out: StbString, status: i32) -> Result<Value, String> {
    // SAFETY: every result from these inverted ports is owned by the host.
    let out = unsafe { OwnedString::from_raw(out, Some(api.free_string)) };
    let value: Value = serde_json::from_str(&out.to_string_lossy())
        .map_err(|_| "invalid host provider response")?;
    if status != 0 {
        return Err(value["error"]
            .as_str()
            .unwrap_or("host provider operation failed")
            .to_owned());
    }
    Ok(value)
}

struct DelegatedProvider {
    continuation: Continuation,
    receipt: Option<AuthReceipt>,
    models: Vec<Model>,
}
#[async_trait::async_trait]
impl Provider for DelegatedProvider {
    fn id(&self) -> &str {
        &self.continuation.model.provider
    }
    fn models(&self) -> &[Model] {
        &self.models
    }
    async fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> rpi_ai::AssistantMessageEventStream {
        let (producer, stream) =
            rpi_ai::event_stream::create_assistant_message_event_stream_with_cancellation(
                &options.signal,
            );
        let mut options = options.clone();
        options.signal = producer.cancellation_token();
        let mut wire_options = encode_options(&options);
        for private in ["authMode", "responseMetadata", "streamKind", "identity"] {
            wire_options.as_object_mut().unwrap().remove(private);
        }
        let request = json!({"version":1,"continuation":self.continuation.descriptor,
            "authReceipt":self.receipt.as_ref().map(|receipt| &receipt.0),"model":model,"context":context,"options":wire_options}).to_string();
        let mut bridge = StreamBridge {
            api: self.continuation.api,
            runtime: tokio::runtime::Handle::current(),
            options,
            producer,
            mapper: StreamMapper::new(model.api.clone(), model.provider.clone(), model.id.clone()),
            error: None,
        };
        tokio::task::spawn_blocking(move || {
            let cancel_options = bridge.options.clone();
            let control = ProviderStreamControl {
                is_cancelled: stream_cancelled,
                wait_cancelled_ms: wait_cancelled,
                user_data: (&cancel_options as *const SimpleStreamOptions)
                    .cast_mut()
                    .cast(),
            };
            let mut out = StbString::empty();
            let result = if let Some(callback) = bridge.api.delegate_provider_stream {
                let status = callback(
                    StbStringRef::from_str(&request),
                    Some(stream_emit),
                    (&mut bridge as *mut StreamBridge).cast(),
                    &control,
                    None,
                    std::ptr::null_mut(),
                    &mut out,
                    bridge.api.user_data,
                );
                host_result(&bridge.api, out, status).and_then(|value| {
                    serde_json::from_value::<AssistantMessage>(value)
                        .map_err(|_| "invalid delegated message".into())
                })
            } else {
                Err("host lacks delegated provider streaming".into())
            };
            let message = if bridge.options.signal.is_cancelled() {
                bridge.mapper.aborted_message()
            } else {
                match (bridge.error.take(), result) {
                    (None, Ok(message)) => message,
                    (error, result) => {
                        let mut message = bridge.mapper.output.clone();
                        message.stop_reason = StopReason::Error;
                        message.error_message = error.or_else(|| result.err());
                        message
                    }
                }
            };
            bridge.runtime.block_on(async {
                for event in bridge.mapper.chunks_then_message(&message) {
                    bridge.producer.push(event).await;
                }
                let event = match message.stop_reason {
                    StopReason::Error | StopReason::Aborted => AssistantMessageEvent::Error {
                        reason: if message.stop_reason == StopReason::Aborted {
                            rpi_ai::ErrorReason::Aborted
                        } else {
                            rpi_ai::ErrorReason::Error
                        },
                        error: message,
                    },
                    reason => AssistantMessageEvent::Done {
                        reason: match reason {
                            StopReason::Length => rpi_ai::DoneReason::Length,
                            StopReason::ToolUse => rpi_ai::DoneReason::ToolUse,
                            StopReason::Deferred => rpi_ai::DoneReason::Deferred,
                            _ => rpi_ai::DoneReason::Stop,
                        },
                        message,
                    },
                };
                bridge.producer.push(event).await;
            });
        });
        stream
    }
}

struct StreamBridge {
    api: PluginApi,
    runtime: tokio::runtime::Handle,
    options: SimpleStreamOptions,
    producer: rpi_ai::AssistantMessageEventStreamProducer,
    mapper: StreamMapper,
    error: Option<String>,
}
extern "C" fn auth_cancelled(data: *mut c_void) -> u8 {
    u8::from(
        unsafe { &*data.cast::<SimpleStreamOptions>() }
            .signal
            .is_cancelled(),
    )
}
extern "C" fn stream_cancelled(data: *mut c_void, _: u32) -> i32 {
    i32::from(auth_cancelled(data))
}
extern "C" fn wait_cancelled(data: *mut c_void, timeout_ms: u32) -> i32 {
    let options = unsafe { &*data.cast::<SimpleStreamOptions>() };
    let cancelled = tokio::runtime::Handle::current().block_on(async {
        tokio::time::timeout(
            Duration::from_millis(u64::from(timeout_ms)),
            options.signal.cancelled(),
        )
        .await
        .is_ok()
    });
    i32::from(cancelled)
}
extern "C" fn stream_emit(chunk: StbString, data: *mut c_void) -> i32 {
    crate::guard_or(1, || {
        let bridge = unsafe { &mut *data.cast::<StreamBridge>() };
        let owned = unsafe { OwnedString::from_raw(chunk, Some(bridge.api.free_string)) };
        let frame: Value = match serde_json::from_str(&owned.to_string_lossy()) {
            Ok(frame) => frame,
            Err(_) => {
                bridge.error = Some("invalid delegated stream frame".into());
                return 1;
            }
        };
        if frame["type"] == "response" {
            let receipt = match serde_json::from_value(frame["response"].clone()) {
                Ok(receipt) => receipt,
                Err(_) => {
                    bridge.error = Some("invalid delegated response metadata".into());
                    return 1;
                }
            };
            if let Some(observer) = &bridge.options.response_observer {
                bridge.runtime.block_on(async {
                    tokio::select! { biased; _ = bridge.options.signal.cancelled() => {}, _ = observer(receipt) => {} }
                });
            }
        } else {
            let mut frame = Some(frame);
            while let Some(next) = frame {
                if let Some(event) = bridge.mapper.apply(next) {
                    if !bridge.runtime.block_on(bridge.producer.push(event)) {
                        return 1;
                    }
                }
                frame = bridge.mapper.take_deferred();
            }
        }
        i32::from(bridge.options.signal.is_cancelled())
    })
}

pub type WrapperFactory = fn(
    &ProviderContext,
    &Model,
    &SimpleStreamOptions,
    Continuation,
) -> Result<Arc<dyn Provider>, String>;
type OwnedFactory = dyn Fn(
        &ProviderContext,
        &Model,
        &SimpleStreamOptions,
        Continuation,
    ) -> Result<Arc<dyn Provider>, String>
    + Send
    + Sync;
struct WrapperContext {
    resources: ProviderContext,
    api: PluginApi,
    factory: Box<OwnedFactory>,
}
extern "C" fn begin(data: *mut c_void) {
    crate::guard_or((), || {
        unsafe { &*data.cast::<WrapperContext>() }
            .resources
            .begin_shutdown()
    });
}
extern "C" fn finish(data: *mut c_void) -> i32 {
    crate::guard_or(1, || {
        let context = unsafe { Box::from_raw(data.cast::<WrapperContext>()) };
        context.resources.finish_shutdown();
        0
    })
}
extern "C" fn invoke(
    request: StbStringRef,
    emit: Option<ProviderStreamEmitFn>,
    data: *mut c_void,
    control: *const ProviderStreamControl,
    _: Option<crate::ProviderStreamCompletionFn>,
    _: *mut c_void,
    out: *mut StbString,
    context: *mut c_void,
) -> i32 {
    crate::guard_or(1, || {
        if out.is_null() {
            return 2;
        }
        let context = unsafe { &*context.cast::<WrapperContext>() };
        let Ok(value) = serde_json::from_str::<Value>(unsafe { request.as_str() }) else {
            return 2;
        };
        let descriptor = value["__rpiContinuation"].clone();
        if descriptor["version"] != 1
            || !descriptor["id"].is_string()
            || !descriptor["generation"].is_string()
        {
            return 2;
        }
        let Ok(request) = serde_json::from_value::<super::Request>(value) else {
            return 2;
        };
        let Ok(options) = super::options(&request.options, &request.model) else {
            return 2;
        };
        let continuation = Continuation {
            api: context.api,
            descriptor,
            model: request.model.clone(),
        };
        let Ok((runtime, _request)) = context.resources.enter(&options.signal) else {
            return 1;
        };
        let message = runtime.block_on(async {
            match (context.factory)(&context.resources, &request.model, &options, continuation) {
                Ok(provider) => {
                    super::drive_stream(provider, &request, &options, emit, data, unsafe {
                        control.as_ref()
                    })
                    .await
                }
                Err(error) => AssistantMessage::terminal(
                    request.model.api.clone(),
                    request.model.provider.clone(),
                    request.model.id.clone(),
                    StopReason::Error,
                    error,
                    0,
                ),
            }
        });
        unsafe {
            *out = StbString::from_string(
                serde_json::to_value(message)
                    .expect("assistant message")
                    .to_string(),
            );
        }
        0
    })
}

/// Required capabilities fail explicitly; there is no one-shot or unwrapped fallback.
pub fn register(api: &PluginApi, manifest: Value, factory: WrapperFactory) -> Result<(), String> {
    register_with_factory(api, manifest, factory)
}

/// Capture session-owned routing state without a process-global account registry.
/// The factory and its captures remain owned until all wrapper requests finish.
pub fn register_with_factory<F>(api: &PluginApi, manifest: Value, factory: F) -> Result<(), String>
where
    F: Fn(
            &ProviderContext,
            &Model,
            &SimpleStreamOptions,
            Continuation,
        ) -> Result<Arc<dyn Provider>, String>
        + Send
        + Sync
        + 'static,
{
    let register = api
        .register_provider_wrapper
        .ok_or("host lacks register_provider_wrapper")?;
    let lifecycle = api
        .register_plugin_lifecycle
        .ok_or("host lacks provider lifecycle")?;
    api.delegate_provider_stream
        .ok_or("host lacks delegated provider streaming")?;
    api.provider_continuation
        .ok_or("host lacks private provider continuation operations")?;
    let context = Box::into_raw(Box::new(WrapperContext {
        resources: ProviderContext::new(),
        api: *api,
        factory: Box::new(factory),
    }));
    let status = lifecycle(begin, finish, context.cast());
    if status != 0 {
        unsafe {
            drop(Box::from_raw(context));
        }
        return Err("wrapper lifecycle registration failed".into());
    }
    let status = register(
        StbStringRef::from_str(&manifest.to_string()),
        invoke,
        crate::callbacks::free_string,
        context.cast(),
    );
    if status != 0 {
        return Err("provider wrapper registration failed".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" fn unused_action(_: u32, _: StbStringRef, _: *mut StbString, _: *mut c_void) -> i32 {
        1
    }

    fn continuation() -> Continuation {
        Continuation {
            api: PluginApi {
                abi_version: crate::RPI_PLUGIN_ABI_VERSION_UNIFIED,
                struct_size: std::mem::size_of::<PluginApi>() as u32,
                free_string: crate::callbacks::free_string,
                runtime_action: unused_action,
                user_data: std::ptr::null_mut(),
                register_tool: None,
                register_command: None,
                register_shortcut: None,
                register_flag: None,
                register_provider: None,
                register_message_renderer: None,
                register_markdown_transformer: None,
                register_entry_renderer: None,
                register_event_handler: None,
                register_resources_discover: None,
                dispatch_event: None,
                declare: None,
                register_before_agent_start: None,
                register_oauth_provider: None,
                register_streaming_provider: None,
                register_tool_with_wake: None,
                register_plugin_lifecycle: None,
                register_provider_auth: None,
                register_credential_store: None,
                register_provider_wrapper: None,
                delegate_provider_stream: None,
                provider_continuation: None,
                native_request: None,
                register_provider_runtime: None,
                register_event_result_handler: None,
                register_service: None,
                native_context: None,
                register_native_command: None,
                register_owned_tool_with_wake: None,
            },
            descriptor: json!({"version":1,"id":"fixture","generation":"fixture"}),
            model: Model::new("model", "Model", rpi_ai::Api::Faux, "fixture", ""),
        }
    }

    #[tokio::test]
    async fn missing_auth_callback_returns_errors_without_panicking() {
        let continuation = continuation();
        let result = continuation
            .resolve_auth("store", "account", &SimpleStreamOptions::default())
            .await;
        assert!(
            matches!(result, Err(error) if error == "host lacks private provider continuation operations")
        );
        assert_eq!(
            continuation
                .release_auth(AuthReceipt("opaque".into()))
                .await,
            Err("host lacks private provider continuation operations".into())
        );
    }

    #[tokio::test]
    async fn missing_stream_callback_emits_terminal_error_instead_of_hanging() {
        let continuation = continuation();
        let provider = continuation.provider(None);
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            provider
                .stream_simple(
                    &continuation.model,
                    &Context::default(),
                    &SimpleStreamOptions::default(),
                )
                .await
                .result()
                .await
        })
        .await
        .expect("missing callback must not leave the consumer waiting")
        .unwrap();
        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(
            result.error_message.as_deref(),
            Some("host lacks delegated provider streaming")
        );
    }
}
