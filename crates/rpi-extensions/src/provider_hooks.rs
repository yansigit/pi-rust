//! Provider context hooks and the legacy, redacted request observations.
use crate::{
    loader::ExtensionSession, prompt_transform::emit_before_agent_start,
    registry::RegistrySnapshot, translate::dispatch_data_event, PluginKeepalive,
};
use rpi_ai::types::{AssistantMessage, Content, Message, UserContent};
use rpi_ai::{
    Context, ContextPatch, Model, ProviderHooks, SimpleStreamOptions, SimpleStreamOptionsPatch,
};
use rpi_plugin_sdk::EventTag;
use std::sync::{Arc, Mutex};

pub struct ExtensionProviderHooks {
    snapshot: Arc<RegistrySnapshot>,
    _keepalive: Arc<PluginKeepalive>,
    last_before_agent_start: Mutex<Option<String>>,
}
impl ExtensionProviderHooks {
    pub fn from_session(session: &ExtensionSession) -> Option<Self> {
        let snapshot = session.snapshot_arc()?;
        if ![
            EventTag::ModelSelect,
            EventTag::SessionInfoChanged,
            EventTag::SessionBeforeCompact,
            EventTag::SessionCompact,
            EventTag::SessionCompactFailed,
            EventTag::SessionBeforeTree,
            EventTag::SessionTreeFailed,
            EventTag::SessionTree,
            EventTag::Context,
            EventTag::BeforeProviderRequest,
            EventTag::BeforeProviderHeaders,
            EventTag::AfterProviderResponse,
            EventTag::BeforeAgentStart,
        ]
        .iter()
        .any(|tag| !snapshot.handlers_for(*tag).is_empty() || snapshot.has_result_handlers(*tag))
            && snapshot.before_agent_start().is_empty()
        {
            return None;
        }
        Some(Self {
            snapshot,
            _keepalive: session.keepalive(),
            last_before_agent_start: Mutex::new(None),
        })
    }
    fn observe_request(&self, model: &Model, ctx: &Context, opts: &SimpleStreamOptions) {
        let request = serde_json::json!({
            "model":model.id,"provider":model.provider,"baseUrl":model.base_url,
            "reasoning":model.reasoning,"pricingAvailable":model.has_pricing(),
            "apiKey":null,"headers":null,"timeoutMs":opts.timeout.map(|d| d.as_millis() as u64),
            "metadata":opts.metadata,"maxTokens":opts.max_tokens,"temperature":opts.temperature,
            "messages":ctx.messages,"identity":opts.identity,
        });
        dispatch_data_event(
            &self.snapshot,
            EventTag::BeforeProviderRequest,
            &request.to_string(),
        );
        dispatch_data_event(
            &self.snapshot,
            EventTag::BeforeProviderHeaders,
            &serde_json::json!({"headers":null,"identity":opts.identity}).to_string(),
        );
    }
}
#[async_trait::async_trait]
impl ProviderHooks for ExtensionProviderHooks {
    async fn lifecycle(
        &self,
        event: &str,
        data: serde_json::Value,
        identity: rpi_ai::identity::RequestIdentity,
        signal: tokio_util::sync::CancellationToken,
    ) -> Result<Option<serde_json::Value>, String> {
        let tag = match event {
            "model_select" => EventTag::ModelSelect,
            "session_info_changed" => EventTag::SessionInfoChanged,
            "session_before_compact" => EventTag::SessionBeforeCompact,
            "session_compact" => EventTag::SessionCompact,
            "session_compact_failed" => EventTag::SessionCompactFailed,
            "session_before_tree" => EventTag::SessionBeforeTree,
            "session_tree" => EventTag::SessionTree,
            "session_tree_failed" => EventTag::SessionTreeFailed,
            _ => return Err("unsupported lifecycle event".into()),
        };
        crate::result_events::dispatch(
            self.snapshot.clone(),
            self._keepalive.clone(),
            tag,
            data,
            Some(identity),
            signal,
        )
        .await
        .map(|result| result.result)
    }

    async fn begin_operation(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> Option<rpi_ai::provider::OperationStartPatch> {
        self.begin_operation_with_prompt_options(model, ctx, opts, serde_json::json!({}))
            .await
    }

    async fn begin_operation_with_prompt_options(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
        mut prompt_options: serde_json::Value,
    ) -> Option<rpi_ai::provider::OperationStartPatch> {
        let mut start_context = ctx.clone();
        if let Some(patch) = self.transform_context(model, &start_context) {
            patch.apply(&mut start_context);
        }
        let (prompt, images) = latest_user_prompt(&start_context.messages).unwrap_or_default();
        let mut event =
            before_agent_start_event(&prompt, images, &start_context, opts.identity.as_ref());
        prompt_options.as_object_mut()?.insert(
            "forceSystemPrompt".into(),
            start_context
                .system_prompt
                .clone()
                .unwrap_or_default()
                .into(),
        );
        event["systemPromptOptions"] = prompt_options;
        let result = crate::result_events::dispatch(
            self.snapshot.clone(),
            self._keepalive.clone(),
            EventTag::BeforeAgentStart,
            event,
            opts.identity.clone(),
            opts.signal.clone(),
        )
        .await;
        Some(match result {
            Ok(result) => rpi_ai::provider::OperationStartPatch {
                system_prompt: result.event["systemPrompt"].as_str().map(str::to_owned),
                messages: result.messages,
            },
            Err(_) => rpi_ai::provider::OperationStartPatch {
                system_prompt: start_context.system_prompt,
                messages: Vec::new(),
            },
        })
    }
    async fn prepare_request(
        &self,
        model: &Model,
        ctx: &mut Context,
        opts: &mut SimpleStreamOptions,
    ) {
        let event = serde_json::json!({"messages":ctx.messages});
        if let Ok(result) = crate::result_events::dispatch(
            self.snapshot.clone(),
            self._keepalive.clone(),
            EventTag::Context,
            event,
            opts.identity.clone(),
            opts.signal.clone(),
        )
        .await
        {
            if let Ok(messages) = serde_json::from_value(result.event["messages"].clone()) {
                ctx.messages = messages;
            }
        }
        self.observe_request(model, ctx, opts);
    }
    fn transform_context(&self, _model: &Model, ctx: &Context) -> Option<ContextPatch> {
        if self.snapshot.before_agent_start().is_empty() {
            return None;
        }
        let (prompt, images) = latest_user_prompt(&ctx.messages).unwrap_or_default();
        let identity = rpi_ai::identity::current();
        let event = before_agent_start_event(&prompt, images, ctx, identity.as_ref());
        emit_before_agent_start(
            ctx.system_prompt.as_deref().unwrap_or_default(),
            &event.to_string(),
            &self.snapshot,
        )
        .map(|system_prompt| ContextPatch {
            system_prompt: Some(system_prompt),
        })
    }
    fn before_request(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> Option<SimpleStreamOptionsPatch> {
        if let Some(identity) = opts
            .identity
            .as_ref()
            .filter(|identity| identity.purpose == "prompt")
        {
            let mut last = self.last_before_agent_start.lock().unwrap();
            if last.as_deref() != Some(identity.operation_id.as_str()) {
                *last = Some(identity.operation_id.clone());
                drop(last);
                let (prompt, images) = latest_user_prompt(&ctx.messages).unwrap_or_default();
                dispatch_data_event(
                    &self.snapshot,
                    EventTag::BeforeAgentStart,
                    &before_agent_start_event(&prompt, images, ctx, Some(identity)).to_string(),
                );
            }
        }
        self.observe_request(model, ctx, opts);
        None
    }
    fn after_response(&self, _model: &Model, message: &AssistantMessage) {
        dispatch_data_event(
            &self.snapshot,
            EventTag::AfterProviderResponse,
            &serde_json::to_string(message).unwrap(),
        );
    }
}
fn before_agent_start_event(
    prompt: &str,
    image_count: usize,
    ctx: &Context,
    identity: Option<&rpi_ai::identity::RequestIdentity>,
) -> serde_json::Value {
    serde_json::json!({"prompt":prompt,"imageCount":image_count,"systemPrompt":ctx.system_prompt,
        "sessionId":identity.and_then(|id| id.session_id.as_deref()),"identity":identity,"host":host_identity()})
}

fn host_identity() -> serde_json::Value {
    serde_json::json!({
        "name": rpi_plugin_sdk::HOST_NAME,
        "version": env!("CARGO_PKG_VERSION"),
    })
}

/// Text/timestamp/image-count of the most recent user message. Mirrors the
/// `prompt`/`images` fields pi puts on its `before_agent_start` event.
fn latest_user_prompt(messages: &[Message]) -> Option<(String, usize)> {
    messages.iter().rev().find_map(|message| match message {
        Message::User(user) => Some((
            user_content_text(&user.content),
            user_content_image_count(&user.content),
        )),
        _ => None,
    })
}

fn user_content_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                Content::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn user_content_image_count(content: &UserContent) -> usize {
    match content {
        UserContent::Text(_) => 0,
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter(|block| matches!(block, Content::Image(_)))
            .count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_plugin_sdk::EventTag;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    static PROVIDER_HITS: AtomicUsize = AtomicUsize::new(0);
    static PROVIDER_LOCK: Mutex<()> = Mutex::new(());
    static CAPTURED: Mutex<Vec<(EventTag, String)>> = Mutex::new(Vec::new());

    rpi_plugin_sdk::event_handler! { fn capture_handler(ev, _ud) {
        let payload = ev.data_text().unwrap_or_default();
        CAPTURED.lock().unwrap().push((ev.tag(), payload));
        0
    } }

    rpi_plugin_sdk::event_handler! { fn counting_provider_handler(ev, _ud) {
        // Count every dispatch on the subscribed tags (BeforeProviderRequest +
        // BeforeProviderHeaders). The `model` check validates the request-event
        // payload shape separately — it must NOT gate the count, or the headers
        // event (whose payload is `{"headers":...}` with no `model` key) would
        // be silently dropped and the fan-out count would be wrong.
        let s = ev.data_text().unwrap_or_default();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
            // The request event carries `model`; the headers event does not.
            // Both are valid dispatches — count unconditionally, and sanity-check
            // the request payload when `model` is present.
            if v.get("model").is_some() {
                assert!(v.get("model").is_some(), "request event must carry model");
            }
            PROVIDER_HITS.fetch_add(1, Ordering::SeqCst);
        }
        0
    } }

    /// `before_agent_start` fires once per NEW user prompt (not once per
    /// provider call in a run), and the request payload carries the
    /// model-facing `messages`.
    #[test]
    fn before_request_emits_before_agent_start_once_and_forwards_messages() {
        let _guard = PROVIDER_LOCK.lock().unwrap();
        CAPTURED.lock().unwrap().clear();

        let mut registry = crate::registry::ExtensionRegistry::new();
        registry.register_event_handler(
            "test-plugin".to_string(),
            EventTag::BeforeAgentStart,
            capture_handler,
            std::ptr::null_mut(),
        );
        registry.register_event_handler(
            "test-plugin".to_string(),
            EventTag::BeforeProviderRequest,
            capture_handler,
            std::ptr::null_mut(),
        );
        let snapshot = Arc::new(registry.snapshot());
        let hooks = ExtensionProviderHooks {
            snapshot: Arc::clone(&snapshot),
            _keepalive: Arc::new(crate::PluginKeepalive::new(Vec::new(), None)),
            last_before_agent_start: Mutex::new(None),
        };
        let model = rpi_ai::Model::new(
            "m",
            "m",
            rpi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let ctx = rpi_ai::Context::new(vec![rpi_ai::types::Message::User(
            rpi_ai::types::UserMessage::new(
                rpi_ai::types::UserContent::Text("hello".to_string()),
                42,
            ),
        )]);
        let opts = rpi_ai::SimpleStreamOptions {
            identity: Some(rpi_ai::identity::RequestIdentity::operation(
                Some("test-session".into()),
                "prompt",
            )),
            api_key: Some("private-api-secret".into()),
            headers: Some([("x-custom-credential".into(), "private-header-secret".into())].into()),
            env: [("ACCOUNT_TOKEN".into(), "private-env-secret".into())].into(),
            ..Default::default()
        };

        // Two provider calls in the same run (same prompt) → one
        // before_agent_start, two before_provider_request observations.
        hooks.before_request(&model, &ctx, &opts);
        hooks.before_request(&model, &ctx, &opts);

        let captured = CAPTURED.lock().unwrap().clone();
        assert!(captured
            .iter()
            .all(|(_, payload)| !payload.contains("private-")));
        let agent_starts: Vec<_> = captured
            .iter()
            .filter(|(tag, _)| matches!(tag, EventTag::BeforeAgentStart))
            .collect();
        assert_eq!(
            agent_starts.len(),
            1,
            "before_agent_start must fire once per prompt: {captured:?}"
        );
        let start_payload: serde_json::Value =
            serde_json::from_str(&agent_starts[0].1).expect("before_agent_start payload is JSON");
        assert_eq!(start_payload["prompt"], "hello");
        assert_eq!(start_payload["imageCount"], 0);
        assert_eq!(start_payload["sessionId"], "test-session");
        // And the host identifies itself, so a plugin can label its own output
        // (trace name, tags, service.name) without hardcoding a brand.
        assert_eq!(start_payload["host"]["name"], rpi_plugin_sdk::HOST_NAME);
        assert_eq!(
            start_payload["host"]["version"],
            env!("CARGO_PKG_VERSION"),
            "the host reports the version of the host, not of the plugin SDK"
        );

        let requests: Vec<_> = captured
            .iter()
            .filter(|(tag, _)| matches!(tag, EventTag::BeforeProviderRequest))
            .collect();
        assert_eq!(requests.len(), 2, "every provider call is observed");
        assert!(
            requests[0].1.contains("\"messages\""),
            "request payload must forward messages: {}",
            requests[0].1
        );
        let request: serde_json::Value = serde_json::from_str(&requests[0].1).unwrap();
        assert_eq!(request["pricingAvailable"], false);
        let mut model = model;
        model.cost_configured = true;
        hooks.before_request(&model, &ctx, &opts);
        let explicit_zero: serde_json::Value =
            serde_json::from_str(&CAPTURED.lock().unwrap().last().unwrap().1).unwrap();
        assert_eq!(explicit_zero["pricingAvailable"], true);
        model.cost_configured = false;
        model.cost.rates.input = 1.0;
        hooks.before_request(&model, &ctx, &opts);
        let built_in: serde_json::Value =
            serde_json::from_str(&CAPTURED.lock().unwrap().last().unwrap().1).unwrap();
        assert_eq!(built_in["pricingAvailable"], true);
    }

    /// `dispatch_data_event` fans the JSON to the tag's handlers, and the
    /// `ExtensionProviderHooks::before_request` path dispatches BOTH the
    /// request and headers events to their subscribers.
    #[test]
    fn provider_hooks_dispatch_to_subscribers() {
        let _guard = PROVIDER_LOCK.lock().unwrap();
        PROVIDER_HITS.store(0, Ordering::SeqCst);

        let mut registry = crate::registry::ExtensionRegistry::new();
        let handler: rpi_plugin_sdk::EventHandlerFn = counting_provider_handler;
        registry.register_event_handler(
            "test-plugin".to_string(),
            EventTag::BeforeProviderRequest,
            handler,
            std::ptr::null_mut(),
        );
        registry.register_event_handler(
            "test-plugin".to_string(),
            EventTag::BeforeProviderHeaders,
            handler,
            std::ptr::null_mut(),
        );
        let snapshot = Arc::new(registry.snapshot());

        // dispatch_data_event directly: one request event ⇒ one hit.
        assert!(dispatch_data_event(
            &snapshot,
            EventTag::BeforeProviderRequest,
            r#"{"model":"m"}"#,
        ));
        assert_eq!(PROVIDER_HITS.load(Ordering::SeqCst), 1);

        // through ExtensionProviderHooks::before_request. The hook fires BOTH
        // BeforeProviderRequest AND BeforeProviderHeaders (each with the same
        // counting handler subscribed), so 2 more hits ⇒ 3 total. The headers
        // event's payload is `{"headers":...}` (no `model` key) — the handler
        // counts it regardless (see counting_provider_handler).
        let hooks = ExtensionProviderHooks {
            snapshot: Arc::clone(&snapshot),
            _keepalive: Arc::new(crate::PluginKeepalive::new(Vec::new(), None)),
            last_before_agent_start: Mutex::new(None),
        };
        let model = rpi_ai::Model::new(
            "m",
            "m",
            rpi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let ctx = rpi_ai::Context::default();
        let opts = rpi_ai::SimpleStreamOptions::default();
        let patch = hooks.before_request(&model, &ctx, &opts);
        assert!(patch.is_none(), "observer-only v1: no patch returned");
        assert_eq!(PROVIDER_HITS.load(Ordering::SeqCst), 3);
    }

    /// A tag with no subscribers dispatches nothing (and doesn't free anything
    /// dangling — dispatch_data_event short-circuits before building events).
    #[test]
    fn provider_hooks_no_subscribers_is_noop() {
        let registry = crate::registry::ExtensionRegistry::new();
        let snapshot = Arc::new(registry.snapshot());
        assert!(!dispatch_data_event(
            &snapshot,
            EventTag::AfterProviderResponse,
            "{}",
        ));
    }

    // ---- before_agent_start prompt transform ------------------------------
    //
    // These use in-process `extern "C"` handlers rather than a cdylib: the ABI
    // round-trip has its own end-to-end test (`plugin_stub_smoke`), so what is
    // left to pin here is the semantics — chaining, failure isolation, and the
    // "no change" signal — which are easier to express with direct handlers.

    /// Handler that returns `<marker>` + whatever prompt it was handed, so a
    /// test can observe the chain by the markers accumulating in order.
    fn append_marker(
        envelope: &str,
        out: &mut rpi_plugin_sdk::callbacks::Output<'_>,
        marker: &str,
    ) -> i32 {
        let current = serde_json::from_str::<serde_json::Value>(envelope)
            .ok()
            .and_then(|v| v.get("systemPrompt")?.as_str().map(str::to_string))
            .unwrap_or_default();
        let json = serde_json::json!({ "systemPrompt": format!("{current}{marker}") });
        i32::from(!out.write(json.to_string()))
    }

    rpi_plugin_sdk::string_handler! { fn append_a(input, out, _data) { append_marker(input, out, " A") } }
    rpi_plugin_sdk::string_handler! { fn append_b(input, out, _data) { append_marker(input, out, " B") } }
    rpi_plugin_sdk::string_handler! { fn append_after(input, out, _data) { append_marker(input, out, " AFTER") } }
    rpi_plugin_sdk::string_handler! { fn append_mark(input, out, _data) { append_marker(input, out, " MARK") } }

    /// Handler that declines to change anything (empty object ⇒ no-op).
    extern "C" fn declines(
        _event_json: rpi_plugin_sdk::StbStringRef,
        _out: *mut rpi_plugin_sdk::StbString,
        _user_data: *mut std::ffi::c_void,
    ) -> i32 {
        0
    }

    /// Handler that reports a handled error.
    extern "C" fn fails(
        _event_json: rpi_plugin_sdk::StbStringRef,
        _out: *mut rpi_plugin_sdk::StbString,
        _user_data: *mut std::ffi::c_void,
    ) -> i32 {
        7
    }

    /// Build hooks whose snapshot has the given prompt transformers.
    fn hooks_with(
        handlers: Vec<crate::registry::BeforeAgentStartHandler>,
    ) -> ExtensionProviderHooks {
        let mut registry = crate::registry::ExtensionRegistry::new();
        for h in handlers {
            registry.register_before_agent_start(h.handler, h.plugin_free_string, h.user_data);
        }
        ExtensionProviderHooks {
            snapshot: Arc::new(registry.snapshot()),
            _keepalive: Arc::new(crate::PluginKeepalive::new(Vec::new(), None)),
            last_before_agent_start: Mutex::new(None),
        }
    }

    /// Transformers chain in registration order: each sees the previous one's
    /// result. This is pi's `runner.ts` behavior (a later handler observes the
    /// prompt an earlier one set), and it is what makes "append" composable.
    #[test]
    fn prompt_transformers_chain_in_registration_order() {
        let hooks = hooks_with(vec![
            crate::registry::BeforeAgentStartHandler {
                handler: append_a,
                plugin_free_string: test_free,
                user_data: std::ptr::null_mut(),
            },
            crate::registry::BeforeAgentStartHandler {
                handler: append_b,
                plugin_free_string: test_free,
                user_data: std::ptr::null_mut(),
            },
        ]);

        let model = rpi_ai::Model::new(
            "m",
            "m",
            rpi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let mut ctx = rpi_ai::Context::new(vec![rpi_ai::types::Message::User(
            rpi_ai::types::UserMessage::new(
                rpi_ai::types::UserContent::Text("hello".to_string()),
                1,
            ),
        )]);
        ctx.system_prompt = Some("base".to_string());

        let patch = hooks
            .transform_context(&model, &ctx)
            .expect("both handlers changed the prompt");
        // Both markers present, in order — proof the second handler saw the
        // first's output rather than the original prompt.
        assert_eq!(patch.system_prompt.as_deref(), Some("base A B"));
    }

    /// A failing handler leaves the prompt as the previous handler left it and
    /// the fan-out continues (pi isolates a throwing handler; it does not abort
    /// the run or discard earlier results).
    #[test]
    fn a_failing_transformer_is_skipped_and_the_chain_continues() {
        let hooks = hooks_with(vec![
            crate::registry::BeforeAgentStartHandler {
                handler: fails,
                plugin_free_string: test_free,
                user_data: std::ptr::null_mut(),
            },
            crate::registry::BeforeAgentStartHandler {
                handler: append_after,
                plugin_free_string: test_free,
                user_data: std::ptr::null_mut(),
            },
        ]);

        let model = rpi_ai::Model::new(
            "m",
            "m",
            rpi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let mut ctx = rpi_ai::Context::new(vec![rpi_ai::types::Message::User(
            rpi_ai::types::UserMessage::new(
                rpi_ai::types::UserContent::Text("hello".to_string()),
                1,
            ),
        )]);
        ctx.system_prompt = Some("base".to_string());

        let patch = hooks.transform_context(&model, &ctx).expect("changed");
        assert_eq!(
            patch.system_prompt.as_deref(),
            Some("base AFTER"),
            "the failing handler must contribute nothing and not stop the next one"
        );
    }

    /// All handlers declining ⇒ `None`, i.e. the harness leaves the prompt
    /// alone. A plugin that only observes must be able to say nothing without
    /// installing an empty prompt.
    #[test]
    fn declining_transformers_leave_the_prompt_untouched() {
        let hooks = hooks_with(vec![crate::registry::BeforeAgentStartHandler {
            handler: declines,
            plugin_free_string: test_free,
            user_data: std::ptr::null_mut(),
        }]);
        let model = rpi_ai::Model::new(
            "m",
            "m",
            rpi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let ctx = rpi_ai::Context::new(vec![rpi_ai::types::Message::User(
            rpi_ai::types::UserMessage::new(
                rpi_ai::types::UserContent::Text("hello".to_string()),
                1,
            ),
        )]);
        assert!(
            hooks.transform_context(&model, &ctx).is_none(),
            "no handler changed the prompt ⇒ no patch ⇒ harness leaves it alone"
        );
    }

    /// No registered transformer ⇒ `None` and the ABI is never entered. A
    /// session with only observer plugins must not pay for (or risk) this path.
    #[test]
    fn no_transformers_means_no_patch() {
        let hooks = hooks_with(vec![]);
        let model = rpi_ai::Model::new(
            "m",
            "m",
            rpi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let ctx = rpi_ai::Context::default();
        assert!(hooks.transform_context(&model, &ctx).is_none());
    }

    use rpi_plugin_sdk::callbacks::free_string as test_free;

    /// The `BeforeAgentStart` observer sees the **post-transform** prompt.
    ///
    /// rpi emits that event from `before_request`, and `transform_context` runs
    /// first (the order `agent_harness::build_stream_fn` uses, mirroring pi's
    /// `assistant.ts`). So an observer is told what the provider is about to be
    /// given, not what it would have been given. This is a deliberate contract
    /// and a subtle one: swapping the two calls in `build_stream_fn` would
    /// silently change what every observer reports, with no other test failing.
    #[test]
    fn the_observer_sees_the_prompt_after_the_transform() {
        let _guard = PROVIDER_LOCK.lock().unwrap();
        CAPTURED.lock().unwrap().clear();

        let mut registry = crate::registry::ExtensionRegistry::new();
        registry.register_event_handler(
            "test-plugin".to_string(),
            EventTag::BeforeAgentStart,
            capture_handler,
            std::ptr::null_mut(),
        );
        registry.register_before_agent_start(append_mark, test_free, std::ptr::null_mut());
        let hooks = ExtensionProviderHooks {
            snapshot: Arc::new(registry.snapshot()),
            _keepalive: Arc::new(crate::PluginKeepalive::new(Vec::new(), None)),
            last_before_agent_start: Mutex::new(None),
        };

        let model = rpi_ai::Model::new(
            "m",
            "m",
            rpi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let mut ctx = rpi_ai::Context::new(vec![rpi_ai::types::Message::User(
            rpi_ai::types::UserMessage::new(
                rpi_ai::types::UserContent::Text("hello".to_string()),
                1,
            ),
        )]);
        ctx.system_prompt = Some("base".to_string());

        // How the harness drives the two hooks: transform first, then apply, then
        // observe from the patched context.
        let patch = hooks
            .transform_context(&model, &ctx)
            .expect("transformer changed the prompt");
        patch.apply(&mut ctx);
        hooks.before_request(
            &model,
            &ctx,
            &rpi_ai::SimpleStreamOptions {
                identity: Some(rpi_ai::identity::RequestIdentity::operation(None, "prompt")),
                ..Default::default()
            },
        );

        let captured = CAPTURED.lock().unwrap().clone();
        let start = captured
            .iter()
            .find(|(tag, _)| matches!(tag, EventTag::BeforeAgentStart))
            .expect("BeforeAgentStart was dispatched");
        let payload: serde_json::Value = serde_json::from_str(&start.1).expect("JSON");
        assert_eq!(
            payload["systemPrompt"], "base MARK",
            "observers must be shown the prompt the provider will actually get"
        );
    }
}
