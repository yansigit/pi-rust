use super::*;
use rpi_plugin_sdk::{AuthRequestControl, StbString, StbStringRef};
use std::ffi::c_void;
struct Host;
#[async_trait::async_trait]
impl crate::RuntimeActionHost for Host {
    async fn send_message(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn send_user_message(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn append_entry(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn set_session_name(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn get_active_tools(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn set_active_tools(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn set_model(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn get_thinking_level(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn set_thinking_level(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn compact(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn get_system_prompt(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn new_session(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn fork(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn navigate_tree(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn switch_session(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn reload(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
}
struct Fixture {
    root: Arc<ActionBridge>,
    publisher: Arc<ActionBridge>,
    consumer: Arc<ActionBridge>,
    keepalive: Arc<PluginKeepalive>,
}
extern "C" fn echo(
    input: StbStringRef,
    _: *const AuthRequestControl,
    out: *mut StbString,
    _: *mut c_void,
) -> i32 {
    let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
    unsafe {
        *out = StbString::from_string(json!({"version":1,"result":input}).to_string());
    }
    0
}
impl Fixture {
    fn new() -> Self {
        let root = ActionBridge::new(tokio::runtime::Handle::current(), Arc::new(Host));
        let publisher = root.for_plugin("publisher");
        let consumer = root.for_plugin("consumer");
        let keepalive = PluginKeepalive::empty();
        root.services.attach(&[RegisteredService {
            manifest: ServiceManifest::parse(r#"{"version":1,"id":"accounts","serviceVersion":1,"methods":["resolve"],"topics":["changed"]}"#).unwrap(),
            callback: Callback { request: echo, free: rpi_plugin_sdk::callbacks::free_string, context: std::ptr::null_mut() },
            bridge: Arc::downgrade(&publisher), owner: publisher.native_owner().unwrap(),
        }], &keepalive);
        Self {
            root,
            publisher,
            consumer,
            keepalive,
        }
    }
    async fn request(&self, plugin: &ActionBridge, args: Value) -> Result<Value, String> {
        self.root
            .services
            .request(
                plugin.native_owner().unwrap(),
                args,
                CancellationToken::new(),
            )
            .await
    }
    async fn subscribe(&self) -> String {
        self.request(
            &self.consumer,
            json!({"action":"subscribe","service":"accounts","serviceVersion":1,"topic":"changed"}),
        )
        .await
        .unwrap()["subscription"]
            .as_str()
            .unwrap()
            .into()
    }
    fn publication(value: Value) -> Value {
        json!({"action":"publish","service":"accounts","serviceVersion":1,"topic":"changed","value":value})
    }
}
#[test]
fn manifests_are_versioned_and_unambiguous() {
    for invalid in [
        json!({"version":2,"id":"accounts","serviceVersion":1,"methods":[]}),
        json!({"version":1,"id":"accounts","serviceVersion":0,"methods":[]}),
        json!({"version":1,"id":"accounts","serviceVersion":1,"methods":["resolve","resolve"]}),
        json!({"version":1,"id":"accounts","serviceVersion":1,"methods":[],"owner":"forged"}),
    ] {
        assert!(ServiceManifest::parse(&invalid.to_string()).is_err());
    }
}
#[tokio::test]
async fn calls_are_private_versioned_and_retain_the_callers_identity() {
    let f = Fixture::new();
    let id = f.subscribe().await;
    let identity = rpi_ai::identity::RequestIdentity::operation(Some("session".into()), "prompt");
    let result = rpi_ai::identity::scope(identity.clone(), f.request(&f.consumer,
        json!({"action":"call","service":"accounts","serviceVersion":1,"method":"resolve","input":{"private":"credential"}}))).await.unwrap();
    assert_eq!(result["caller"]["plugin"], "consumer");
    assert_eq!(result["identity"]["operationId"], identity.operation_id);
    assert_eq!(result["input"]["private"], "credential");
    assert!(f.root.services.state.lock().unwrap().subscriptions[&id]
        .events
        .is_empty());
    assert!(f
        .request(
            &f.consumer,
            json!({"action":"call","service":"accounts","serviceVersion":2,"method":"resolve"})
        )
        .await
        .is_err());
    assert!(f
        .request(&f.consumer, Fixture::publication(json!("forged")))
        .await
        .is_err());
    assert!(f
        .request(&f.publisher, json!({"action":"next","subscription":id}))
        .await
        .is_err());
}
#[tokio::test]
async fn fanout_backpressure_is_atomic_cancel_safe_and_preserves_order() {
    let f = Fixture::new();
    let first = f.subscribe().await;
    let second = f.subscribe().await;
    for n in 0..MAX_EVENTS {
        f.request(&f.publisher, Fixture::publication(json!(n)))
            .await
            .unwrap();
    }
    f.request(&f.consumer, json!({"action":"next","subscription":first}))
        .await
        .unwrap();
    let signal = CancellationToken::new();
    let blocked = f.root.services.request(
        f.publisher.native_owner().unwrap(),
        Fixture::publication(json!("blocked")),
        signal.clone(),
    );
    tokio::pin!(blocked);
    assert!(futures::poll!(&mut blocked).is_pending());
    assert_eq!(
        f.root.services.state.lock().unwrap().subscriptions[&first]
            .events
            .len(),
        MAX_EVENTS - 1
    );
    signal.cancel();
    assert!(blocked.await.is_err());
    f.request(&f.consumer, json!({"action":"next","subscription":second}))
        .await
        .unwrap();
    assert_eq!(
        f.request(&f.publisher, Fixture::publication(json!("last")))
            .await
            .unwrap()["sequence"],
        MAX_EVENTS + 1
    );
    for id in [first, second] {
        for n in 1..MAX_EVENTS {
            assert_eq!(
                f.request(&f.consumer, json!({"action":"next","subscription":id}))
                    .await
                    .unwrap()["value"],
                n
            );
        }
        assert_eq!(
            f.request(&f.consumer, json!({"action":"next","subscription":id}))
                .await
                .unwrap()["value"],
            "last"
        );
    }
}
#[tokio::test]
async fn byte_bounds_reader_cancellation_and_unload_have_independent_progress() {
    let f = Fixture::new();
    let id = f.subscribe().await;
    assert!(f
        .request(
            &f.publisher,
            Fixture::publication(json!("x".repeat(MAX_BYTES)))
        )
        .await
        .is_err());
    f.request(
        &f.publisher,
        Fixture::publication(json!("x".repeat(MAX_BYTES / 2))),
    )
    .await
    .unwrap();
    let blocked = f.request(
        &f.publisher,
        Fixture::publication(json!("y".repeat(MAX_BYTES / 2))),
    );
    tokio::pin!(blocked);
    assert!(futures::poll!(&mut blocked).is_pending());
    f.request(
        &f.consumer,
        json!({"action":"unsubscribe","subscription":id}),
    )
    .await
    .unwrap();
    blocked.await.unwrap();
    let id = f.subscribe().await;
    let signal = CancellationToken::new();
    let read = f.root.services.request(
        f.consumer.native_owner().unwrap(),
        json!({"action":"next","subscription":id}),
        signal.clone(),
    );
    tokio::pin!(read);
    assert!(futures::poll!(&mut read).is_pending());
    signal.cancel();
    assert!(read.await.is_err());
    let read = f.request(&f.consumer, json!({"action":"next","subscription":id}));
    tokio::pin!(read);
    assert!(futures::poll!(&mut read).is_pending());
    f.root.invalidate();
    assert!(read.await.is_err());
}
#[tokio::test]
async fn child_subscriptions_cannot_be_used_by_siblings_and_do_not_own_parent_services() {
    let f = Fixture::new();
    let a = Arc::new(rpi_agent::tool_execution_scope::ToolExecutionScope::default());
    let b = Arc::new(rpi_agent::tool_execution_scope::ToolExecutionScope::default());
    let child = f.consumer.for_invocation(true, CancellationToken::new());
    let id = a
        .run(f.request(
            &child,
            json!({"action":"subscribe","service":"accounts","serviceVersion":1,"topic":"changed"}),
        ))
        .await
        .unwrap()["subscription"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(b
        .run(f.request(&child, json!({"action":"unsubscribe","subscription":id})))
        .await
        .is_err());
    assert!(f
        .request(
            &f.consumer,
            json!({"action":"unsubscribe","subscription":id})
        )
        .await
        .is_err());
    for n in 0..MAX_EVENTS {
        f.request(&f.publisher, Fixture::publication(json!(n)))
            .await
            .unwrap();
    }
    let blocked = f.request(&f.publisher, Fixture::publication(json!("parent survives")));
    tokio::pin!(blocked);
    assert!(futures::poll!(&mut blocked).is_pending());
    drop(a);
    drop(child);
    tokio::time::timeout(std::time::Duration::from_secs(1), blocked)
        .await
        .unwrap()
        .unwrap();
    assert!(f
        .root
        .services
        .state
        .lock()
        .unwrap()
        .subscriptions
        .is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leased_context_crosses_threads_and_release_cannot_revoke_a_new_lease() {
    let f = Fixture::new();
    let identity = rpi_ai::identity::RequestIdentity::operation(Some("session".into()), "prompt");
    let invocation = rpi_ai::identity::scope(identity.clone(), async {
        f.consumer.for_invocation(false, CancellationToken::new())
    })
    .await;
    let handle = {
        let _invocation = invocation.enter_invocation();
        crate::native::context(0, f.consumer.user_data()) as usize
    };
    let services = f.root.services.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut output = StbString::empty();
        let code = crate::native::request(StbStringRef::from_str(r#"{"version":1,"port":"services","action":"call","service":"accounts","serviceVersion":1,"method":"resolve"}"#), std::ptr::null(), &mut output, handle as *mut c_void);
        let output = unsafe { rpi_plugin_sdk::callbacks::OwnedString::from_raw(output, Some(crate::host_free_string)) };
        assert_eq!(code, 0);
        serde_json::from_str::<Value>(&output.to_string_lossy()).unwrap()
    }).await.unwrap();
    assert_eq!(
        result["result"]["identity"]["operationId"],
        identity.operation_id
    );
    let second = crate::native::context(0, f.consumer.user_data());
    assert_ne!(handle, second as usize);
    crate::native::context(1, handle as *mut c_void);
    crate::native::context(1, handle as *mut c_void);
    assert!(ActionBridge::resolve_handle(second as usize).is_some());
    assert!(ActionBridge::resolve_handle(handle).is_none());
    crate::native::context(1, second);
    assert!(ActionBridge::resolve_handle(f.consumer.user_data() as usize).is_some());
    assert_eq!(Arc::strong_count(&f.keepalive), 1);
    services.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unloading_cancels_waiting_without_unloading_an_outstanding_foreign_callback() {
    struct Blocked {
        entered: std::sync::mpsc::Sender<()>,
        state: Mutex<bool>,
        ready: std::sync::Condvar,
        returned: std::sync::mpsc::Sender<()>,
    }
    extern "C" fn blocked(
        _: StbStringRef,
        _: *const AuthRequestControl,
        out: *mut StbString,
        context: *mut c_void,
    ) -> i32 {
        let state = unsafe { &*context.cast::<Blocked>() };
        state.entered.send(()).unwrap();
        let mut released = state.state.lock().unwrap();
        while !*released {
            released = state.ready.wait(released).unwrap();
        }
        unsafe {
            *out = StbString::from_string(r#"{"version":1,"result":null}"#.into());
        }
        state.returned.send(()).unwrap();
        0
    }
    let f = Fixture::new();
    let (entered, entry) = std::sync::mpsc::channel();
    let (returned, exit) = std::sync::mpsc::channel();
    let probe = Box::new(Blocked {
        entered,
        state: Mutex::new(false),
        ready: std::sync::Condvar::new(),
        returned,
    });
    f.root
        .services
        .state
        .lock()
        .unwrap()
        .services
        .get_mut(&("accounts".into(), 1))
        .unwrap()
        .registration
        .callback = Callback {
        request: blocked,
        free: rpi_plugin_sdk::callbacks::free_string,
        context: (&*probe as *const Blocked).cast_mut().cast(),
    };
    let weak = Arc::downgrade(&f.keepalive);
    let services = f.root.services.clone();
    let owner = f.consumer.native_owner().unwrap();
    let call = tokio::spawn(async move {
        services
            .request(
                owner,
                json!({"action":"call","service":"accounts","serviceVersion":1,"method":"resolve"}),
                CancellationToken::new(),
            )
            .await
    });
    tokio::task::spawn_blocking(move || entry.recv().unwrap())
        .await
        .unwrap();
    f.root.services.close();
    assert!(call.await.unwrap().is_err());
    drop(f.keepalive);
    assert!(weak.upgrade().is_some());
    *probe.state.lock().unwrap() = true;
    probe.ready.notify_all();
    tokio::task::spawn_blocking(move || exit.recv().unwrap())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while weak.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_providers_and_nested_wrappers_keep_child_and_request_context() {
    use rpi_ai::{Context, Model, SimpleStreamOptions};
    use rpi_plugin_sdk::{ProviderStreamCompletionFn, ProviderStreamControl, ProviderStreamEmitFn};
    fn check(input: StbStringRef) {
        let request: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
        let handle = crate::native::context(0, std::ptr::null_mut());
        assert!(!handle.is_null());
        let mut output = StbString::empty();
        let status = crate::native::request(
            StbStringRef::from_str(
                r#"{"version":1,"port":"services","action":"call","service":"accounts","serviceVersion":1,"method":"resolve"}"#,
            ),
            std::ptr::null(),
            &mut output,
            handle,
        );
        let output = unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(output, Some(crate::host_free_string))
        };
        assert_eq!(status, 0);
        let response: Value = serde_json::from_str(&output.to_string_lossy()).unwrap();
        assert_eq!(
            response["result"]["identity"],
            request["options"]["identity"]
        );
        assert_eq!(response["result"]["caller"]["child"], true);
        crate::native::context(1, handle);
    }
    extern "C" fn one(input: StbStringRef, out: *mut StbString, _: *mut c_void) -> i32 {
        rpi_plugin_sdk::guard_or(1, || {
            check(input);
            let mut message =
                rpi_ai::AssistantMessage::empty(rpi_ai::Api::Faux, "probe", "model", 0);
            message.stop_reason = rpi_ai::StopReason::Stop;
            unsafe {
                *out = StbString::from_string(serde_json::to_string(&message).unwrap());
            }
            0
        })
    }
    extern "C" fn stream(
        input: StbStringRef,
        _: Option<ProviderStreamEmitFn>,
        _: *mut c_void,
        _: *const ProviderStreamControl,
        _: Option<ProviderStreamCompletionFn>,
        _: *mut c_void,
        out: *mut StbString,
        data: *mut c_void,
    ) -> i32 {
        one(input, out, data)
    }
    extern "C" fn wrapper(
        input: StbStringRef,
        emit: Option<ProviderStreamEmitFn>,
        emit_data: *mut c_void,
        control: *const ProviderStreamControl,
        _: Option<ProviderStreamCompletionFn>,
        _: *mut c_void,
        out: *mut StbString,
        _: *mut c_void,
    ) -> i32 {
        rpi_plugin_sdk::guard_or(1, || {
            check(input);
            let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
            let request =
                json!({"version":1,"continuation":input["__rpiContinuation"]}).to_string();
            crate::provider::wrappers::delegate_stream(
                StbStringRef::from_str(&request),
                emit,
                emit_data,
                control,
                None,
                std::ptr::null_mut(),
                out,
                std::ptr::null_mut(),
            )
        })
    }
    let f = Fixture::new();
    for streaming in [false, true] {
        let mut registry = crate::ExtensionRegistry::new();
        registry.register_provider(crate::RegisteredProvider {
            action_bridge: Some(f.consumer.clone()),
            provider_id: "probe".into(),
            base_url: String::new(),
            api_style: "faux".into(),
            request_fn: one,
            stream_fn: if streaming { Some(stream) } else { None },
            plugin_free_string: crate::host_free_string,
            user_data: std::ptr::null_mut(),
        });
        for id in ["first", "second"] {
            registry.register_provider_wrapper(crate::provider::wrappers::RegisteredWrapper {
                action_bridge: Some(f.consumer.clone()),
                plugin: "consumer".into(),
                manifest: crate::provider::wrappers::WrapperManifest {
                    version: 1,
                    id: id.into(),
                    providers: None,
                },
                stream: wrapper,
                free: crate::host_free_string,
                context: std::ptr::null_mut(),
            });
        }
        let session = crate::ExtensionSession::from_parts(
            Arc::new(registry.snapshot()),
            f.keepalive.clone(),
            vec![],
            Some(f.root.clone()),
        );
        let providers = crate::wrap_providers(
            &session,
            crate::PluggableProvider::from_session(&session, tokio::runtime::Handle::current()),
        );
        let model = Model::new(
            "model",
            "model",
            rpi_ai::Api::Faux,
            "probe",
            "https://example.invalid",
        );
        let child = Arc::new(rpi_agent::tool_execution_scope::ToolExecutionScope::default());
        let identity =
            rpi_ai::identity::RequestIdentity::operation(Some("child-session".into()), "prompt");
        let options = SimpleStreamOptions {
            identity: Some(identity.clone()),
            ..Default::default()
        };
        let mut stream = child
            .run(rpi_ai::identity::scope(
                identity,
                providers[0].stream_simple(&model, &Context::new(vec![]), &options),
            ))
            .await;
        while stream.next().await.is_some() {}
        let result = stream.result().await.unwrap();
        assert_eq!(result.stop_reason, rpi_ai::StopReason::Stop, "{result:?}");
    }
}
