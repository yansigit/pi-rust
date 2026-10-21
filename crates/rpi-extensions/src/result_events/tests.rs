use super::*;
use crate::registry::ExtensionRegistry;
use rpi_plugin_sdk::{AuthRequestControl, StbStringRef};
use std::{ffi::c_void, sync::Mutex};

struct Probe {
    seen: Mutex<Vec<Value>>,
    response: Value,
}
extern "C" fn callback(
    input: StbStringRef,
    _: *const AuthRequestControl,
    out: *mut StbString,
    context: *mut c_void,
) -> i32 {
    rpi_plugin_sdk::guard_or(1, || {
        let probe = unsafe { &*context.cast::<Probe>() };
        let request: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
        probe.seen.lock().unwrap().push(request);
        unsafe {
            *out = StbString::from_string(probe.response.to_string());
        }
        0
    })
}
extern "C" fn observer(event: StablePluginEvent, context: *mut c_void) -> i32 {
    rpi_plugin_sdk::guard_or(1, || {
        let probe = unsafe { &*context.cast::<Probe>() };
        let data = unsafe { event.payload.data.data.to_string_lossy() };
        probe
            .seen
            .lock()
            .unwrap()
            .push(serde_json::from_str(&data).unwrap());
        0
    })
}
fn register(registry: &mut ExtensionRegistry, tag: EventTag, probe: &Probe) {
    registry.register_result_handler(
        "test".into(),
        tag,
        Callback {
            request: callback,
            free: rpi_plugin_sdk::callbacks::free_string,
            context: (probe as *const Probe).cast_mut().cast(),
        },
        None,
    );
}
fn keepalive() -> Arc<PluginKeepalive> {
    Arc::new(PluginKeepalive::new(Vec::new(), None))
}
#[tokio::test]
async fn awaited_results_and_observers_share_registration_order_across_registry_merges() {
    let first = Probe {
        seen: Mutex::new(Vec::new()),
        response: json!({"version":1,"result":{"messages":[{"content":"first"}]}}),
    };
    let observe = Probe {
        seen: Mutex::new(Vec::new()),
        response: Value::Null,
    };
    let last = Probe {
        seen: Mutex::new(Vec::new()),
        response: json!({"version":1,"result":{"messages":[{"content":"last"}]}}),
    };
    let mut registry = ExtensionRegistry::new();
    register(&mut registry, EventTag::Context, &first);
    registry.register_event_handler(
        "observer".into(),
        EventTag::Context,
        observer,
        (&observe as *const Probe).cast_mut().cast(),
    );
    let mut child = ExtensionRegistry::new();
    register(&mut child, EventTag::Context, &last);
    registry.absorb(child);
    let identity = rpi_ai::identity::RequestIdentity::operation(Some("session".into()), "prompt");
    let outcome = dispatch(
        Arc::new(registry.snapshot()),
        keepalive(),
        EventTag::Context,
        json!({"messages":[]}),
        Some(identity.clone()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.event["messages"][0]["content"], "last");
    assert_eq!(
        observe.seen.lock().unwrap()[0]["messages"][0]["content"],
        "first"
    );
    assert_eq!(
        last.seen.lock().unwrap()[0]["event"]["messages"][0]["content"],
        "first"
    );
    assert_eq!(
        last.seen.lock().unwrap()[0]["identity"]["operationId"],
        identity.operation_id
    );
}
#[tokio::test]
async fn cancellation_results_stop_the_remaining_listeners() {
    let first = Probe {
        seen: Mutex::new(Vec::new()),
        response: json!({"version":1,"result":{"cancel":true}}),
    };
    let last = Probe {
        seen: Mutex::new(Vec::new()),
        response: json!({"version":1}),
    };
    let mut registry = ExtensionRegistry::new();
    register(&mut registry, EventTag::SessionBeforeCompact, &first);
    register(&mut registry, EventTag::SessionBeforeCompact, &last);
    let outcome = dispatch(
        Arc::new(registry.snapshot()),
        keepalive(),
        EventTag::SessionBeforeCompact,
        json!({}),
        None,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.result.unwrap()["cancel"], true);
    assert!(last.seen.lock().unwrap().is_empty());
}
#[test]
fn tool_result_content_replacement_drops_stale_structured_content() {
    let mut outcome = Outcome {
        event: json!({"content":[],"structuredContent":{"stale":true},"isError":false}),
        result: None,
        messages: Vec::new(),
    };
    reduce(
        EventTag::ToolResult,
        &mut outcome,
        json!({"content":[{"type":"text","text":"replacement"}]}),
    )
    .unwrap();
    assert!(outcome.event.get("structuredContent").is_none());
    reduce(
        EventTag::ToolResult,
        &mut outcome,
        json!({"isError":true,"usage":{"output":3}}),
    )
    .unwrap();
    assert_eq!(outcome.event["content"][0]["text"], "replacement");
    assert_eq!(outcome.event["isError"], true);
}
#[tokio::test]
async fn explicit_null_payload_is_different_from_an_undefined_result() {
    let null = Probe {
        seen: Mutex::new(Vec::new()),
        response: json!({"version":1,"result":null}),
    };
    let undefined = Probe {
        seen: Mutex::new(Vec::new()),
        response: json!({"version":1}),
    };
    let mut registry = ExtensionRegistry::new();
    register(&mut registry, EventTag::BeforeProviderRequest, &null);
    register(&mut registry, EventTag::BeforeProviderRequest, &undefined);
    let outcome = dispatch(
        Arc::new(registry.snapshot()),
        keepalive(),
        EventTag::BeforeProviderRequest,
        json!({"payload":{"initial":true}}),
        None,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.result, Some(Value::Null));
    assert_eq!(outcome.event["payload"], Value::Null);
    assert_eq!(
        undefined.seen.lock().unwrap()[0]["event"]["payload"],
        Value::Null
    );
}

struct Blocking {
    entered: tokio::sync::Notify,
    released: Mutex<bool>,
    wake: std::sync::Condvar,
    finished: std::sync::atomic::AtomicBool,
}
extern "C" fn blocked(
    _: StbStringRef,
    _: *const AuthRequestControl,
    out: *mut StbString,
    context: *mut c_void,
) -> i32 {
    let state = unsafe { &*context.cast::<Blocking>() };
    state.entered.notify_one();
    let mut released = state.released.lock().unwrap();
    while !*released {
        released = state.wake.wait(released).unwrap();
    }
    unsafe {
        *out = StbString::from_string(json!({"version":1,"result":{"cancel":true}}).to_string());
    }
    state
        .finished
        .store(true, std::sync::atomic::Ordering::Release);
    0
}
#[tokio::test]
async fn cancellation_stops_waiting_but_keeps_the_library_until_foreign_return() {
    let state = Arc::new(Blocking {
        entered: tokio::sync::Notify::new(),
        released: Mutex::new(false),
        wake: std::sync::Condvar::new(),
        finished: std::sync::atomic::AtomicBool::new(false),
    });
    let mut registry = ExtensionRegistry::new();
    registry.register_result_handler(
        "blocking".into(),
        EventTag::SessionBeforeCompact,
        Callback {
            request: blocked,
            free: rpi_plugin_sdk::callbacks::free_string,
            context: Arc::as_ptr(&state).cast_mut().cast(),
        },
        None,
    );
    let keepalive = keepalive();
    let signal = CancellationToken::new();
    let task = tokio::spawn(dispatch(
        Arc::new(registry.snapshot()),
        keepalive.clone(),
        EventTag::SessionBeforeCompact,
        json!({}),
        None,
        signal.clone(),
    ));
    state.entered.notified().await;
    signal.cancel();
    assert_eq!(task.await.unwrap().unwrap_err(), "event cancelled");
    assert!(Arc::strong_count(&keepalive) > 1);
    assert!(!state.finished.load(std::sync::atomic::Ordering::Acquire));
    *state.released.lock().unwrap() = true;
    state.wake.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !state.finished.load(std::sync::atomic::Ordering::Acquire)
            || Arc::strong_count(&keepalive) != 1
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
