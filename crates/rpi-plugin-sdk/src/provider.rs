//! Provider packages use the same C ABI as tools. This bridge owns a runtime
//! per request by default; opt-in contexts reuse resources until lifecycle shutdown.
//!
//! Available with the optional `provider` feature. Use [`crate::export_provider!`]
//! for single-provider plugins, or [`register`] and [`drive`] when registering
//! multiple providers through one plugin entrypoint.
use crate::{
    PluginApi, ProviderRequestFn, ProviderStreamControl, ProviderStreamEmitFn,
    ProviderStreamRequestFn, StbString, StbStringRef,
};
use rpi_ai::provider_wire::{decode_options as options, encode_event as event_json};
use rpi_ai::{
    AssistantMessage, AssistantMessageEvent, Context, Model, Provider, SimpleStreamOptions,
    StopReason,
};
use std::{ffi::c_void, sync::Arc, time::Duration};

pub mod composition;
mod context;
pub mod native;
pub use context::{drive_with_context, register_with_context, ContextFactory, ProviderContext};

pub type Factory = fn(&Model, &SimpleStreamOptions) -> Arc<dyn Provider>;

/// Reuse the maintained OpenAI-compatible protocol without importing any
/// vendor credentials from the host process environment. Client construction
/// errors propagate without bypassing the configured transport.
pub fn completions(
    model: &Model,
    options: &SimpleStreamOptions,
    anonymous: bool,
) -> Result<Arc<dyn Provider>, String> {
    let client = rpi_ai::http::build_client(&model.base_url, None, None)?;
    if anonymous && options.api_key.is_none() {
        Ok(Arc::new(rpi_ai::providers::openai_completions::OpenAiCompletionsProvider::with_models_without_auth(model.provider.clone(), client, vec![model.clone()])))
    } else {
        Ok(Arc::new(rpi_ai::providers::openai_completions::OpenAiCompletionsProvider::with_models_without_env_api_key(model.provider.clone(), options.api_key.clone(), client, vec![model.clone()])))
    }
}

/// Declare a catalog and register one implementation with a one-shot fallback.
pub fn register(
    api: &PluginApi,
    id: &str,
    base_url: &str,
    style: &str,
    manifest: serde_json::Value,
    request: ProviderRequestFn,
    stream: ProviderStreamRequestFn,
) -> i32 {
    if let Some(declare) = api.declare {
        let json = serde_json::json!({"providers":[manifest]}).to_string();
        let rc = declare(StbStringRef::from_str(&json));
        if rc != 0 {
            return rc;
        }
    }
    let free = crate::callbacks::free_string;
    if let Some(register) = api.register_streaming_provider {
        register(
            StbStringRef::from_str(id),
            StbStringRef::from_str(base_url),
            StbStringRef::from_str(style),
            stream,
            free,
            std::ptr::null_mut(),
        )
    } else if let Some(register) = api.register_provider {
        register(
            StbStringRef::from_str(id),
            StbStringRef::from_str(base_url),
            StbStringRef::from_str(style),
            request,
            free,
            std::ptr::null_mut(),
        )
    } else {
        1
    }
}

#[derive(serde::Deserialize)]
struct Request {
    model: Model,
    context: Context,
    options: serde_json::Value,
}

async fn drive_stream(
    provider: Arc<dyn Provider>,
    request: &Request,
    options: &SimpleStreamOptions,
    emit: Option<ProviderStreamEmitFn>,
    emit_data: *mut c_void,
    control: Option<&ProviderStreamControl>,
) -> AssistantMessage {
    let mut receipts = None;
    let mut private_options = None;
    if request.options["responseMetadata"].as_bool() == Some(true) {
        if emit.is_none() {
            return AssistantMessage::terminal(
                request.model.api.clone(),
                request.model.provider.clone(),
                request.model.id.clone(),
                StopReason::Error,
                "private response metadata requires streaming callbacks",
                0,
            );
        }
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        receipts = Some(receiver);
        let mut derived = options.clone();
        let signal = options.signal.clone();
        derived.response_observer = Some(Arc::new(move |receipt| {
            let sender = sender.clone();
            let signal = signal.clone();
            Box::pin(async move {
                tokio::select! {
                    biased;
                    _ = signal.cancelled() => {},
                    _ = sender.send(receipt) => {},
                }
            })
        }));
        private_options = Some(derived);
    }
    let options = private_options.as_ref().unwrap_or(options);
    let mut poll = tokio::time::interval(Duration::from_millis(10));
    // Some adapters wait for HTTP headers before returning their stream. Keep
    // draining the receipt port during that wait, including delegated retries.
    let pending_stream = provider.stream_simple(&request.model, &request.context, options);
    tokio::pin!(pending_stream);
    let mut stream = loop {
        if options.signal.is_cancelled()
            || control.is_some_and(|control| (control.is_cancelled)(control.user_data, 0) != 0)
        {
            options.signal.cancel();
            return AssistantMessage::terminal(
                request.model.api.clone(),
                request.model.provider.clone(),
                request.model.id.clone(),
                StopReason::Aborted,
                "provider request was cancelled",
                0,
            );
        }
        tokio::select! {
            biased;
            _ = poll.tick() => {},
            receipt = next_receipt(&mut receipts) => emit_receipt(receipt, emit, emit_data, options),
            stream = &mut pending_stream => break stream,
        }
    };
    let mut partial = Arc::new(AssistantMessage::empty(
        request.model.api.clone(),
        request.model.provider.clone(),
        request.model.id.clone(),
        0,
    ));
    let mut previous_text = None;
    loop {
        // Check synchronously too: an already-buffered provider can
        // finish before the interval's initial tick becomes ready.
        if options.signal.is_cancelled()
            || control.is_some_and(|control| (control.is_cancelled)(control.user_data, 0) != 0)
        {
            options.signal.cancel();
            let mut partial = (*partial).clone();
            partial.stop_reason = StopReason::Aborted;
            partial.error_message = Some("provider request was cancelled".into());
            break partial;
        }
        tokio::select! {
            biased;
            _ = poll.tick() => {}
            receipt = next_receipt(&mut receipts) => emit_receipt(receipt, emit, emit_data, options),
            event = stream.next() => {
                let Some(event) = event else {
                    break stream.result().await.unwrap_or_else(|error| AssistantMessage::terminal(
                        request.model.api.clone(), request.model.provider.clone(), request.model.id.clone(), StopReason::Error, error.to_string(), 0));
                };
                match &event {
                    AssistantMessageEvent::Done { message, .. } => break message.clone(),
                    AssistantMessageEvent::Error { error, .. } => break error.clone(),
                    _ => {},
                }
                partial = match &event {
                    AssistantMessageEvent::Start { partial }
                    | AssistantMessageEvent::TextStart { partial, .. }
                    | AssistantMessageEvent::TextDelta { partial, .. }
                    | AssistantMessageEvent::TextEnd { partial, .. }
                    | AssistantMessageEvent::ThinkingStart { partial, .. }
                    | AssistantMessageEvent::ThinkingDelta { partial, .. }
                    | AssistantMessageEvent::ThinkingEnd { partial, .. }
                    | AssistantMessageEvent::ToolCallStart { partial, .. }
                    | AssistantMessageEvent::ToolCallDelta { partial, .. }
                    | AssistantMessageEvent::ToolCallEnd { partial, .. } => Arc::clone(partial),
                    AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => unreachable!(),
                };
                let Some(emit) = emit else { continue; };
                let chunk = event_json(&event, &mut previous_text);
                if emit(StbString::from_string(chunk.to_string()), emit_data) != 0 {
                    options.signal.cancel();
                    let mut partial = (*partial).clone();
                    partial.stop_reason = StopReason::Aborted;
                    break partial;
                }
            }
        }
    }
}

async fn next_receipt(
    receiver: &mut Option<tokio::sync::mpsc::Receiver<rpi_ai::provider::ResponseMetadata>>,
) -> rpi_ai::provider::ResponseMetadata {
    if let Some(receiver) = receiver {
        if let Some(receipt) = receiver.recv().await {
            return receipt;
        }
    }
    std::future::pending().await
}

fn emit_receipt(
    receipt: rpi_ai::provider::ResponseMetadata,
    emit: Option<ProviderStreamEmitFn>,
    data: *mut c_void,
    options: &SimpleStreamOptions,
) {
    if let Some(emit) = emit {
        let frame = serde_json::json!({"type":"response","response":receipt}).to_string();
        if emit(StbString::from_string(frame), data) != 0 {
            options.signal.cancel();
        }
    }
}

/// Drive a synchronous plugin request without retaining any borrowed ABI data.
///
/// # Safety
/// All arguments must satisfy the SDK request/callback lifetime contract. `out`
/// must be writable; any `control` and callback context must outlive this call.
pub unsafe fn drive(
    factory: Factory,
    request: StbStringRef,
    emit: Option<ProviderStreamEmitFn>,
    emit_data: *mut c_void,
    control: *const ProviderStreamControl,
    out: *mut StbString,
) -> i32 {
    crate::guard_or(1, || {
        if out.is_null() {
            return 2;
        }
        let Ok(request) = serde_json::from_str::<Request>(unsafe { request.as_str() }) else {
            return 2;
        };
        let Ok(options) = options(&request.options, &request.model) else {
            return 2;
        };
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return 1;
        };
        let control = unsafe { control.as_ref() };
        let message = runtime.block_on(async {
            let provider = factory(&request.model, &options);
            drive_stream(provider, &request, &options, emit, emit_data, control).await
        });
        // Convert via Value to normalize tagged Content's field representation.
        let json = serde_json::to_value(message)
            .expect("assistant message JSON")
            .to_string();
        unsafe {
            *out = StbString::from_string(json);
        }
        0
    })
}

/// Export a provider package using an ordinary Rust Provider factory.
///
/// Requires the `provider` feature. For a multi-provider package, use one
/// [`crate::export_plugin!`] entrypoint and call [`register`] for each provider
/// instead of exporting this macro multiple times.
#[macro_export]
macro_rules! export_provider {
    ($id:expr, $base:expr, $api:expr, $manifest:expr, $factory:path) => {
        extern "C" fn request(
            req: $crate::StbStringRef,
            out: *mut $crate::StbString,
            _: *mut std::ffi::c_void,
        ) -> i32 {
            unsafe {
                $crate::provider::drive(
                    $factory,
                    req,
                    None,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    out,
                )
            }
        }
        extern "C" fn stream(
            req: $crate::StbStringRef,
            emit: Option<$crate::ProviderStreamEmitFn>,
            data: *mut std::ffi::c_void,
            control: *const $crate::ProviderStreamControl,
            _: Option<$crate::ProviderStreamCompletionFn>,
            _: *mut std::ffi::c_void,
            out: *mut $crate::StbString,
            _: *mut std::ffi::c_void,
        ) -> i32 {
            unsafe { $crate::provider::drive($factory, req, emit, data, control, out) }
        }
        $crate::export_plugin!(|api| $crate::provider::register(
            api, $id, $base, $api, $manifest, request, stream
        ));
    };
}

#[cfg(test)]
mod bridge_tests;

#[cfg(test)]
mod tests {
    use super::*;
    fn factory(_: &Model, _: &SimpleStreamOptions) -> Arc<dyn Provider> {
        rpi_ai::providers::faux::FauxProvider::new(
            rpi_ai::providers::faux::FauxScript::new()
                .with_tool_call("echo", serde_json::json!({"value":"hello"})),
        )
    }
    extern "C" fn collect(raw: StbString, data: *mut c_void) -> i32 {
        let string = unsafe {
            crate::callbacks::OwnedString::from_raw(raw, Some(crate::callbacks::free_string))
        };
        let value = serde_json::from_str(&string.to_string_lossy()).unwrap();
        // The bridge calls emit synchronously on this request thread only.
        unsafe { &mut *data.cast::<Vec<serde_json::Value>>() }.push(value);
        0
    }
    fn request() -> String {
        serde_json::json!({"model":Model::new("m","M",rpi_ai::Api::Faux,"probe","http://localhost"),
            "context":Context::new(vec![]),"options":{}})
        .to_string()
    }
    #[test]
    fn transport_construction_errors_return_terminal_output_with_or_without_emit() {
        fn factory(
            _: &ProviderContext,
            model: &Model,
            options: &SimpleStreamOptions,
        ) -> Result<Arc<dyn Provider>, String> {
            completions(model, options, false)
        }
        let model = Model::new(
            "m",
            "M",
            rpi_ai::Api::OpenaiCompletions,
            "probe",
            ":invalid",
        );
        assert!(completions(&model, &SimpleStreamOptions::default(), false).is_err());
        let request =
            serde_json::json!({"model":model, "context":Context::new(vec![]), "options":{}})
                .to_string();
        for with_emit in [false, true] {
            let mut chunks = Vec::<serde_json::Value>::new();
            let mut out = StbString::empty();
            assert_eq!(
                unsafe {
                    drive_with_context(
                        factory,
                        std::ptr::null_mut(),
                        StbStringRef::from_str(&request),
                        with_emit.then_some(collect as ProviderStreamEmitFn),
                        (&mut chunks as *mut Vec<serde_json::Value>).cast(),
                        std::ptr::null(),
                        &mut out,
                    )
                },
                0
            );
            let output = unsafe {
                crate::callbacks::OwnedString::from_raw(out, Some(crate::callbacks::free_string))
            };
            let message: AssistantMessage =
                serde_json::from_str(&output.to_string_lossy()).unwrap();
            assert_eq!(message.stop_reason, StopReason::Error);
            assert!(message
                .error_message
                .as_deref()
                .unwrap()
                .contains("invalid target URL"));
            assert!(chunks.is_empty(), "terminal errors use the output slot");
        }
    }

    #[test]
    fn bridge_emits_tool_identity_and_valid_terminal_json() {
        let mut chunks = Vec::<serde_json::Value>::new();
        let mut out = StbString::empty();
        let rc = unsafe {
            drive(
                factory,
                StbStringRef::from_str(&request()),
                Some(collect),
                (&mut chunks as *mut Vec<serde_json::Value>).cast(),
                std::ptr::null(),
                &mut out,
            )
        };
        assert_eq!(rc, 0);
        let output = unsafe {
            crate::callbacks::OwnedString::from_raw(out, Some(crate::callbacks::free_string))
        };
        let message: AssistantMessage = serde_json::from_str(&output.to_string_lossy()).unwrap();
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        for kind in ["toolcall_start", "toolcall_end"] {
            let chunk = chunks
                .iter()
                .find(|chunk| chunk["type"] == kind)
                .expect(kind);
            assert_eq!(chunk["name"], "echo");
            assert!(!chunk["id"].as_str().unwrap().is_empty());
        }
        let end = chunks
            .iter()
            .find(|chunk| chunk["type"] == "toolcall_end")
            .unwrap();
        assert_eq!(end["arguments"]["value"], "hello");
        assert!(!chunks
            .iter()
            .any(|chunk| chunk["type"] == "done" || chunk["type"] == "error"));
    }
    #[test]
    fn bridge_honors_the_host_control_surface() {
        extern "C" fn cancelled(_: *mut c_void, _: u32) -> i32 {
            1
        }
        let control = ProviderStreamControl {
            is_cancelled: cancelled,
            wait_cancelled_ms: cancelled,
            user_data: std::ptr::null_mut(),
        };
        let mut out = StbString::empty();
        assert_eq!(
            unsafe {
                drive(
                    factory,
                    StbStringRef::from_str(&request()),
                    None,
                    std::ptr::null_mut(),
                    &control,
                    &mut out,
                )
            },
            0
        );
        let output = unsafe {
            crate::callbacks::OwnedString::from_raw(out, Some(crate::callbacks::free_string))
        };
        let message: AssistantMessage = serde_json::from_str(&output.to_string_lossy()).unwrap();
        assert_eq!(message.stop_reason, StopReason::Aborted);
    }
    #[test]
    fn options_preserve_provider_controls() {
        let options = options(
            &serde_json::json!({"apiKey":"key","timeoutMs":42,
            "maxRetries":3,"maxRetryDelayMs":17,"reasoning":"Xhigh","maxTokens":80,
            "headers":{"x-custom":"value"},"cacheRetention":"Long","sessionId":"s"}),
            &Model::new("m", "M", rpi_ai::Api::Faux, "probe", "http://localhost"),
        )
        .unwrap();
        assert_eq!(options.api_key.as_deref(), Some("key"));
        assert_eq!(options.timeout, Some(Duration::from_millis(42)));
        assert_eq!(options.max_retries, Some(3));
        assert_eq!(options.max_retry_delay, Some(Duration::from_millis(17)));
        assert_eq!(options.reasoning, Some(rpi_ai::ThinkingLevel::Xhigh));
        assert_eq!(options.max_tokens, Some(80));
        assert_eq!(options.session_id.as_deref(), Some("s"));
        assert_eq!(options.headers.unwrap()["x-custom"], "value");
    }

    #[test]
    fn private_request_options_fail_closed_and_preserve_replacement_isolation() {
        let model = Model::new("m", "M", rpi_ai::Api::Faux, "probe", "http://localhost");
        for value in [
            serde_json::json!({"env":{"KEY":42}}),
            serde_json::json!({"authMode":"unknown"}),
            serde_json::json!({"authMode":null}),
        ] {
            assert!(options(&value, &model).is_err());
        }
        let options = options(&serde_json::json!({"authMode":"replace","env":{"ACCOUNT":"private-env"},"headers":{"Authorization":"private-header"}}), &model).unwrap();
        let rpi_ai::auth::AttemptAuth::Replace(replacement) = &options.authentication else {
            panic!("replacement mode lost")
        };
        let auth = replacement.for_provider("probe").unwrap();
        assert!(auth.api_key.is_none());
        assert_eq!(auth.headers["Authorization"], "private-header");
        assert_eq!(auth.env["ACCOUNT"], "private-env");
        assert!(replacement.for_provider("other").is_none());
        assert!(!format!("{options:?}").contains("private-"));
    }
}
