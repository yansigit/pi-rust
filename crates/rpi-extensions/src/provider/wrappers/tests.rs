use super::*;
use crate::ExtensionRegistry;
use std::sync::atomic::AtomicUsize;

struct Wrapper {
    name: &'static str,
    trace: Arc<Mutex<Vec<String>>>,
    saved: Mutex<Value>,
    replace: bool,
}

struct Forward {
    emit: ProviderStreamEmitFn,
    context: *mut c_void,
    receipts: usize,
}
extern "C" fn forward(chunk: StbString, data: *mut c_void) -> i32 {
    rpi_plugin_sdk::guard_or(1, || {
        let state = unsafe { &mut *data.cast::<Forward>() };
        let owned = unsafe { OwnedString::from_raw(chunk, Some(crate::host_free_string)) };
        let json = owned.to_string_lossy();
        if serde_json::from_str::<Value>(&json).unwrap()["type"] == "response" {
            state.receipts += 1;
        }
        (state.emit)(StbString::from_string(json), state.context)
    })
}
extern "C" fn wrapper(
    request: StbStringRef,
    emit: Option<ProviderStreamEmitFn>,
    data: *mut c_void,
    control: *const ProviderStreamControl,
    _: Option<rpi_plugin_sdk::ProviderStreamCompletionFn>,
    _: *mut c_void,
    out: *mut StbString,
    context: *mut c_void,
) -> i32 {
    rpi_plugin_sdk::guard_or(1, || {
        let state = unsafe { &*context.cast::<Wrapper>() };
        let request: Value = serde_json::from_str(unsafe { request.as_str() }).unwrap();
        state
            .trace
            .lock()
            .unwrap()
            .push(format!("{}:before", state.name));
        let descriptor = &request["__rpiContinuation"];
        *state.saved.lock().unwrap() = descriptor.clone();
        let mut delegated = json!({"version":1,"continuation":descriptor,"options":{"maxRetries":0,"headers":{"request-id":"kept"}}});
        if state.replace {
            let request = json!({"version":1,"continuation":descriptor,"action":"resolveAuth","storeId":"pool","accountId":"second"}).to_string();
            let mut auth = StbString::empty();
            let rc = continuation_action(
                StbStringRef::from_str(&request),
                std::ptr::null(),
                &mut auth,
                std::ptr::null_mut(),
            );
            let auth = unsafe { OwnedString::from_raw(auth, Some(crate::host_free_string)) };
            assert_eq!(rc, 0, "auth port failed");
            let auth: Value = serde_json::from_str(&auth.to_string_lossy()).unwrap();
            assert!(!auth.to_string().contains("private"));
            delegated["authReceipt"] = auth["receipt"].clone();
        }
        let mut forward_state = Forward {
            emit: emit.unwrap(),
            context: data,
            receipts: 0,
        };
        let mut result = StbString::empty();
        let status = delegate_stream(
            StbStringRef::from_str(&delegated.to_string()),
            Some(forward),
            (&mut forward_state as *mut Forward).cast(),
            control,
            None,
            std::ptr::null_mut(),
            &mut result,
            std::ptr::null_mut(),
        );
        let result = unsafe { OwnedString::from_raw(result, Some(crate::host_free_string)) };
        state
            .trace
            .lock()
            .unwrap()
            .push(format!("{}:after:{}", state.name, forward_state.receipts));
        unsafe {
            *out = StbString::from_string(result.to_string_lossy());
        }
        status
    })
}
fn register(registry: &mut ExtensionRegistry, wrapper_state: &Wrapper) {
    let record = RegisteredWrapper {
        action_bridge: None,
        plugin: wrapper_state.name.into(),
        manifest: WrapperManifest::parse(
            &json!({"version":1,"id":"pool","providers":["probe"]}).to_string(),
        )
        .unwrap(),
        stream: wrapper,
        free: rpi_plugin_sdk::callbacks::free_string,
        context: (wrapper_state as *const Wrapper).cast_mut().cast(),
    };
    assert!(!registry.register_provider_wrapper(record.clone()));
    assert!(registry.register_provider_wrapper(record));
}
fn session(registry: &ExtensionRegistry) -> ExtensionSession {
    ExtensionSession::from_parts(
        Arc::new(registry.snapshot()),
        PluginKeepalive::empty(),
        vec![],
        None,
    )
}

struct Base {
    models: Vec<Model>,
    calls: AtomicUsize,
    full_calls: AtomicUsize,
    retained: Mutex<Option<rpi_ai::provider::ResponseObserver>>,
    expect_replace: bool,
}
#[async_trait::async_trait]
impl Provider for Base {
    fn id(&self) -> &str {
        "probe"
    }
    fn models(&self) -> &[Model] {
        &self.models
    }
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.full_calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(options.provider_options["transport"], "websocket");
        assert_eq!(
            options.provider_options["samplingParams"],
            json!({"top_p":0.8})
        );
        assert_eq!(
            options.metadata.as_ref().unwrap()["nested"],
            json!([true, 3])
        );
        self.stream_simple(model, context, options).await
    }
    async fn stream_simple(
        &self,
        model: &Model,
        _: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(options.max_retries, Some(0));
        assert_eq!(options.headers.as_ref().unwrap()["request-id"], "kept");
        if self.expect_replace {
            let AttemptAuth::Replace(auth) = &options.authentication else {
                panic!("replacement missing")
            };
            assert_eq!(
                auth.for_provider("probe").unwrap().api_key.as_deref(),
                Some("private-second")
            );
        }
        for status in [429, 200] {
            if let Some(observer) = &options.response_observer {
                observer(rpi_ai::provider::ResponseMetadata {
                    status,
                    headers: vec![("set-cookie".into(), b"private-cookie".to_vec())],
                })
                .await;
            }
        }
        *self.retained.lock().unwrap() = options.response_observer.clone();
        let (mut producer, stream) =
            create_assistant_message_event_stream_with_cancellation(&options.signal);
        let model = model.clone();
        tokio::spawn(async move {
            let mut partial = AssistantMessage::empty(model.api, "probe", model.id, 42);
            producer
                .push(AssistantMessageEvent::Start {
                    partial: Arc::new(partial.clone()),
                })
                .await;
            partial.content.push(Content::text(""));
            producer
                .push(AssistantMessageEvent::TextStart {
                    content_index: 0,
                    partial: Arc::new(partial.clone()),
                })
                .await;
            for delta in ["α", "🐈"] {
                if let Content::Text(text) = &mut partial.content[0] {
                    text.text.push_str(delta);
                }
                producer
                    .push(AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: delta.into(),
                        partial: Arc::new(partial.clone()),
                    })
                    .await;
            }
            if let Content::Text(text) = &mut partial.content[0] {
                text.text_signature = Some("meaningful-signature".into());
            }
            partial.usage.output = 7;
            partial.stop_reason = rpi_ai::StopReason::Stop;
            producer
                .push(AssistantMessageEvent::TextEnd {
                    content_index: 0,
                    content: "α🐈".into(),
                    partial: Arc::new(partial.clone()),
                })
                .await;
            producer
                .push(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: partial,
                })
                .await;
        });
        stream
    }
}
fn base(replace: bool) -> Arc<Base> {
    Arc::new(Base {
        models: vec![Model::new(
            "m",
            "M",
            rpi_ai::Api::Faux,
            "probe",
            "https://bound",
        )],
        calls: AtomicUsize::new(0),
        full_calls: AtomicUsize::new(0),
        retained: Mutex::new(None),
        expect_replace: replace,
    })
}

#[tokio::test]
async fn nested_wrappers_preserve_identity_streaming_metadata_and_rebuild_once() {
    let trace = Arc::new(Mutex::new(vec![]));
    let first = Wrapper {
        name: "first",
        trace: trace.clone(),
        saved: Mutex::new(Value::Null),
        replace: false,
    };
    let second = Wrapper {
        name: "second",
        trace: trace.clone(),
        saved: Mutex::new(Value::Null),
        replace: false,
    };
    let mut registry = ExtensionRegistry::new();
    register(&mut registry, &first);
    register(&mut registry, &second);
    let session = session(&registry);
    let base = base(false);
    let received = Arc::new(Mutex::new(vec![]));
    let options = SimpleStreamOptions {
        provider_options: json!({"transport":"websocket","samplingParams":{"top_p":0.8}})
            .as_object()
            .unwrap()
            .clone(),
        metadata: Some([("nested".into(), json!([true, 3]))].into()),
        response_observer: Some(Arc::new({
            let received = received.clone();
            move |receipt| {
                let received = received.clone();
                Box::pin(async move {
                    received.lock().unwrap().push(receipt.status);
                })
            }
        })),
        ..Default::default()
    };
    for count in 1..=2 {
        let provider = wrap_providers(&session, vec![base.clone()]).pop().unwrap();
        assert_eq!(provider.id(), "probe");
        assert_eq!(provider.models()[0].id, "m");
        let mut stream = if count == 1 {
            provider
                .stream_simple(&base.models[0], &Context::new(vec![]), &options)
                .await
        } else {
            provider
                .stream(&base.models[0], &Context::new(vec![]), &options)
                .await
        };
        let (tags, result) = tokio::time::timeout(Duration::from_secs(3), async {
            let mut tags = vec![];
            while let Some(event) = stream.next().await {
                assert!(!serde_json::to_string(&event)
                    .unwrap()
                    .contains("private-cookie"));
                tags.push(event.type_tag().to_string());
            }
            (tags, stream.result().await.unwrap())
        })
        .await
        .unwrap();
        assert_eq!(
            tags,
            [
                "start",
                "text_start",
                "text_delta",
                "text_delta",
                "text_end",
                "done"
            ]
        );
        let Content::Text(text) = &result.content[0] else {
            panic!()
        };
        assert_eq!(text.text, "α🐈");
        assert_eq!(text.text_signature.as_deref(), Some("meaningful-signature"));
        assert_eq!(result.usage.output, 7);
        assert_eq!(base.calls.load(Ordering::SeqCst), count);
    }
    assert_eq!(*received.lock().unwrap(), [429, 200, 429, 200]);
    assert_eq!(base.full_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *trace.lock().unwrap(),
        [
            "first:before",
            "second:before",
            "second:after:2",
            "first:after:2",
            "first:before",
            "second:before",
            "second:after:2",
            "first:after:2"
        ]
    );
    assert!(Continuation::resolve(&first.saved.lock().unwrap()).is_err());
    let child = ExtensionRegistry::new();
    child.invalidate();
    assert!(session.snapshot().unwrap().is_active());
    registry.invalidate();
    let provider = wrap_providers(&session, vec![base.clone()]).pop().unwrap();
    let result = provider
        .stream_simple(&base.models[0], &Context::new(vec![]), &options)
        .await
        .result()
        .await
        .unwrap();
    assert_eq!(result.stop_reason, rpi_ai::StopReason::Error);
    assert_eq!(base.calls.load(Ordering::SeqCst), 2);
}

extern "C" fn auth(
    input: StbStringRef,
    _: *const AuthRequestControl,
    out: *mut StbString,
    _: *mut c_void,
) -> i32 {
    let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
    assert_eq!(input["action"], "resolveApiKey");
    assert_eq!(input["input"]["credential"]["key"], "private-second");
    unsafe {
        *out = StbString::from_string(
            json!({"version":1,"kind":"auth","payload":{"apiKey":"private-second"}}).to_string(),
        );
    }
    0
}
extern "C" fn store(
    input: StbStringRef,
    _: *const AuthRequestControl,
    out: *mut StbString,
    _: *mut c_void,
) -> i32 {
    let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
    assert_eq!(input["action"], "read");
    assert_eq!(input["providerId"], "probe");
    assert_eq!(input["accountId"], "second");
    unsafe {
        *out = StbString::from_string(json!({"version":1,"kind":"credential","payload":{"type":"api_key","key":"private-second"}}).to_string());
    }
    0
}
extern "C" fn release(_: StbStringRef, _: *mut c_void) {
    panic!("API key read must not acquire a lock");
}

#[tokio::test]
async fn forged_receipts_generations_models_and_auth_payloads_are_rejected_before_dispatch() {
    let registry = ExtensionRegistry::new();
    let session = session(&registry);
    let base = base(false);
    let next = WrapperNext {
        next: base.clone(),
        session,
        plugin: "pool".into(),
        generation: "generation".into(),
    };
    let continuation = next.continuation(
        &base.models[0],
        &Context::new(vec![]),
        &SimpleStreamOptions::default(),
    );
    let valid = json!({"version":1,"continuation":continuation.descriptor()});
    let mut requests = Vec::new();
    for (field, value) in [
        ("authReceipt", json!("forged")),
        ("auth", json!({"apiKey":"must-never-be-accepted"})),
        ("options", json!({"authMode":"replace"})),
        ("options", json!({"streamKind":"stream"})),
        ("options", json!({"env":{"KEY":false}})),
    ] {
        let mut request = valid.clone();
        request[field] = value;
        requests.push(request);
    }
    let mut request = valid.clone();
    request["continuation"]["generation"] = json!("stale");
    requests.push(request);
    let mut model = base.models[0].clone();
    model.provider = "other".into();
    let mut request = valid;
    request["model"] = serde_json::to_value(model).unwrap();
    requests.push(request);
    for request in requests {
        let mut output = StbString::empty();
        assert_eq!(
            delegate_stream(
                StbStringRef::from_str(&request.to_string()),
                None,
                std::ptr::null_mut(),
                std::ptr::null(),
                None,
                std::ptr::null_mut(),
                &mut output,
                std::ptr::null_mut()
            ),
            1
        );
        let output = unsafe { OwnedString::from_raw(output, Some(crate::host_free_string)) };
        assert!(!output.to_string_lossy().contains("must-never-be-accepted"));
    }
    let short = 4_u32;
    let mut output = StbString::empty();
    assert_eq!(
        continuation_action(
            StbStringRef::from_str("{}"),
            (&short as *const u32).cast(),
            &mut output,
            std::ptr::null_mut()
        ),
        1
    );
    let _output = unsafe { OwnedString::from_raw(output, Some(crate::host_free_string)) };
    assert_eq!(base.calls.load(Ordering::SeqCst), 0);
}

struct Flood {
    produced: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl Provider for Flood {
    fn id(&self) -> &str {
        "probe"
    }
    fn models(&self) -> &[Model] {
        &[]
    }
    async fn stream_simple(
        &self,
        model: &Model,
        _: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let (mut sender, stream) =
            create_assistant_message_event_stream_with_cancellation(&options.signal);
        let model = model.clone();
        let produced = self.produced.clone();
        tokio::spawn(async move {
            let mut message = AssistantMessage::empty(model.api, model.provider, model.id, 0);
            sender
                .push(AssistantMessageEvent::Start {
                    partial: Arc::new(message.clone()),
                })
                .await;
            message.content.push(Content::text(""));
            for _ in 0..10000 {
                if let Content::Text(text) = &mut message.content[0] {
                    text.text.push('x');
                }
                if !sender
                    .push(AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: "x".into(),
                        partial: Arc::new(message.clone()),
                    })
                    .await
                {
                    break;
                }
                produced.fetch_add(1, Ordering::Release);
            }
        });
        stream
    }
}

#[tokio::test]
async fn dropping_a_stopped_consumer_cancels_nested_queues_and_releases_the_next_provider() {
    let fixture = Wrapper {
        name: "pool",
        trace: Default::default(),
        saved: Mutex::new(Value::Null),
        replace: false,
    };
    let mut registry = ExtensionRegistry::new();
    register(&mut registry, &fixture);
    let session = session(&registry);
    let library = Arc::downgrade(&session.keepalive());
    let produced = Arc::new(AtomicUsize::new(0));
    let base = Arc::new(Flood {
        produced: produced.clone(),
    });
    let next = Arc::downgrade(&base);
    let provider = wrap_providers(&session, vec![base]).pop().unwrap();
    let model = Model::new("m", "M", rpi_ai::Api::Faux, "probe", "https://bound");
    let mut stream = provider
        .stream_simple(
            &model,
            &Context::new(vec![]),
            &SimpleStreamOptions::default(),
        )
        .await;
    stream.next().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while produced.load(Ordering::Acquire) < 130 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(stream);
    drop(provider);
    drop(session);
    tokio::time::timeout(Duration::from_secs(2), async {
        while next.upgrade().is_some() || library.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        fixture.trace.lock().unwrap().last().unwrap(),
        "pool:after:0"
    );
    assert!(Continuation::resolve(&fixture.saved.lock().unwrap()).is_err());
}

#[tokio::test]
async fn account_auth_is_provider_owned_and_only_an_opaque_receipt_crosses_the_wrapper_port() {
    let wrapper = Wrapper {
        name: "pool",
        trace: Default::default(),
        saved: Mutex::new(Value::Null),
        replace: true,
    };
    let mut registry = ExtensionRegistry::new();
    register(&mut registry, &wrapper);
    registry.register_provider_auth(crate::provider_auth::RegisteredAuthProvider {
        plugin:"provider".into(),manifest:crate::provider_auth::AuthManifest::parse(r#"{"version":1,"id":"probe","displayName":"Probe","apiKey":true,"oauth":false,"methods":[]}"#).unwrap(),
        request_fn:auth,plugin_free_string:rpi_plugin_sdk::callbacks::free_string,user_data:std::ptr::null_mut(),
    });
    registry.register_credential_store(crate::credential_store::RegisteredCredentialStore {
        plugin: "pool".into(),
        manifest: crate::credential_store::StoreManifest::parse(r#"{"version":1,"id":"pool"}"#)
            .unwrap(),
        request: store,
        release,
        free: rpi_plugin_sdk::callbacks::free_string,
        context: std::ptr::null_mut(),
    });
    let session = session(&registry);
    let base = base(true);
    let provider = wrap_providers(&session, vec![base.clone()]).pop().unwrap();
    let mut stream = provider
        .stream_simple(
            &base.models[0],
            &Context::new(vec![]),
            &SimpleStreamOptions::default(),
        )
        .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while stream.next().await.is_some() {}
    })
    .await
    .unwrap();
    assert_eq!(
        stream.result().await.unwrap().stop_reason,
        rpi_ai::StopReason::Stop
    );
    assert_eq!(base.calls.load(Ordering::SeqCst), 1);
    session.auth_runtime().shutdown().await;
}
