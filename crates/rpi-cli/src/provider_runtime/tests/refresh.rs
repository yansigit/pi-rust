use super::*;
use std::time::Duration;

struct Invocation {
    ticket: Value,
    reply: std::sync::mpsc::Sender<Value>,
    returned: Arc<tokio::sync::Notify>,
    cancelled: Arc<AtomicBool>,
}
struct Gate {
    send: tokio::sync::mpsc::Sender<Invocation>,
    calls: AtomicUsize,
}
fn compare(name: &str, actual: Value) {
    let reference: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/provider-parity/catalog-reference.json"
    )))
    .unwrap();
    assert_eq!(reference["pi"], "1.3.0");
    assert_eq!(actual, reference["checks"][name]);
}
extern "C" fn refresh(
    input: rpi_plugin_sdk::StbStringRef,
    control: *const rpi_plugin_sdk::AuthRequestControl,
    output: *mut rpi_plugin_sdk::StbString,
    context: *mut std::ffi::c_void,
) -> i32 {
    let gate = unsafe { &*context.cast::<Gate>() };
    let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
    let control = unsafe { &*control };
    let (reply, receive) = std::sync::mpsc::channel();
    let returned = Arc::new(tokio::sync::Notify::new());
    let cancelled = Arc::new(AtomicBool::new(false));
    gate.calls.fetch_add(1, Ordering::SeqCst);
    gate.send
        .blocking_send(Invocation {
            ticket: input["input"]["publicationId"].clone(),
            reply,
            returned: returned.clone(),
            cancelled: cancelled.clone(),
        })
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let result = loop {
        if (control.is_cancelled)(control.user_data) != 0 {
            cancelled.store(true, Ordering::SeqCst);
        }
        match receive.recv_timeout(Duration::from_millis(2)) {
            Ok(result) => break result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                if std::time::Instant::now() < deadline => {}
            _ => {
                returned.notify_one();
                return 1;
            }
        }
    };
    unsafe {
        *output = rpi_plugin_sdk::StbString::from_string(
            json!({"version":1,"result":result}).to_string(),
        );
    }
    returned.notify_one();
    0
}
fn setup() -> (
    Arc<ProviderRuntime>,
    NativeOwner,
    Arc<Host>,
    Arc<Gate>,
    tokio::sync::mpsc::Receiver<Invocation>,
) {
    let (send, receive) = tokio::sync::mpsc::channel(4);
    let gate = Arc::new(Gate {
        send,
        calls: AtomicUsize::new(0),
    });
    let (runtime, owner, host) = fixture_with(|registry| {
        registry.register_provider_runtime(
            rpi_extensions::provider_runtime::RegisteredProviderRuntime {
                action_bridge: None,
                plugin: "test-plugin".into(),
                manifest: rpi_extensions::provider_runtime::RuntimeManifest {
                    version: 1,
                    id: "test".into(),
                    actions: vec!["refreshCatalog".into()],
                },
                request_fn: refresh,
                free: rpi_plugin_sdk::callbacks::free_string,
                context: Arc::as_ptr(&gate).cast_mut().cast(),
            },
        );
    });
    (runtime, owner, host, gate, receive)
}
fn start(
    runtime: &Arc<ProviderRuntime>,
    owner: &NativeOwner,
) -> tokio::task::JoinHandle<Result<Value, String>> {
    let runtime = runtime.clone();
    let owner = owner.clone();
    tokio::spawn(async move {
        request(
            &runtime,
            owner,
            json!({"action":"refresh","providerId":"test"}),
        )
        .await
    })
}
fn publish(ticket: &Value, name: &str) -> Value {
    json!({"action":"publish","providerId":"test","models":[model(name)],"publicationId":ticket})
}
async fn returned(invocation: Invocation, result: Value) {
    invocation.reply.send(result).unwrap();
    tokio::time::timeout(Duration::from_secs(5), invocation.returned.notified())
        .await
        .unwrap();
}

#[tokio::test]
async fn unrelated_provider_publication_does_not_invalidate_an_awaited_refresh() {
    let (runtime, owner, _, _gate, mut incoming) = setup();
    let mut other = model("other-initial");
    other.provider = "other".into();
    request(&runtime, owner.clone(), json!({"action":"register","providerId":"other","implementationId":"test","models":[other]})).await.unwrap();
    let pending = start(&runtime, &owner);
    let invocation = incoming.recv().await.unwrap();
    other.id = "other-updated".into();
    request(
        &runtime,
        owner.clone(),
        json!({"action":"publish","providerId":"other","models":[other]}),
    )
    .await
    .unwrap();
    let committed = request(
        &runtime,
        owner.clone(),
        publish(&invocation.ticket, "refreshed"),
    )
    .await
    .unwrap();
    compare(
        "unrelatedProvider",
        json!({"cancelled":invocation.cancelled.load(Ordering::SeqCst),"accepted":committed["revision"].is_u64(),"applied":runtime.providers().await.iter().find(|p| p.id() == "test").unwrap().models()[0].id == "refreshed"}),
    );
    returned(invocation, json!({"published":true})).await;
    assert_eq!(pending.await.unwrap().unwrap(), committed);
    let state = runtime.state.lock().await;
    assert_eq!(
        state.catalog.get_provider("test").unwrap().models[0].id,
        "refreshed"
    );
    assert_eq!(
        state.catalog.get_provider("other").unwrap().models[0].id,
        "other-updated"
    );
}

#[tokio::test]
async fn newer_refresh_cancels_older_callback_and_refuses_its_late_catalog() {
    let (runtime, owner, _, _gate, mut incoming) = setup();
    let older = start(&runtime, &owner);
    let old = incoming.recv().await.unwrap();
    let newer = start(&runtime, &owner);
    let new = incoming.recv().await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(5), older)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err()
        .contains("catalog changed"));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !old.cancelled.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        request(&runtime, owner.clone(), publish(&old.ticket, "stale"))
            .await
            .is_err()
    );
    let committed = request(&runtime, owner.clone(), publish(&new.ticket, "latest"))
        .await
        .unwrap();
    compare(
        "supersededRefresh",
        json!({"cancelled":old.cancelled.load(Ordering::SeqCst),"oldApplied":runtime.providers().await[0].models()[0].id == "stale","accepted":committed["revision"].is_u64(),"newApplied":runtime.providers().await[0].models()[0].id == "latest"}),
    );
    returned(new, json!({"published":true})).await;
    assert_eq!(newer.await.unwrap().unwrap(), committed);
    returned(old, json!({"models":[model("late-uncooperative")]})).await;
    assert_eq!(runtime.providers().await[0].models()[0].id, "latest");
}

#[tokio::test]
async fn failed_installation_preserves_pending_refresh_and_child_cannot_supersede_it() {
    let (runtime, owner, host, _gate, mut incoming) = setup();
    let pending = start(&runtime, &owner);
    let invocation = incoming.recv().await.unwrap();
    host.fail.store(true, Ordering::SeqCst);
    assert!(request(
        &runtime,
        owner.clone(),
        json!({"action":"publish","providerId":"test","models":[model("failed")]})
    )
    .await
    .is_err());
    host.fail.store(false, Ordering::SeqCst);
    assert!(request(
        &runtime,
        NativeOwner {
            child: true,
            ..owner.clone()
        },
        json!({"action":"refresh","providerId":"test"})
    )
    .await
    .unwrap_err()
    .contains("child session"));
    let committed = request(
        &runtime,
        owner.clone(),
        publish(&invocation.ticket, "accepted"),
    )
    .await
    .unwrap();
    assert!(!invocation.cancelled.load(Ordering::SeqCst));
    returned(invocation, json!({"published":true})).await;
    assert_eq!(pending.await.unwrap().unwrap(), committed);
}

#[tokio::test]
async fn manual_replacement_cancels_an_outstanding_refresh() {
    let (runtime, owner, _, _gate, mut incoming) = setup();
    let pending = start(&runtime, &owner);
    let invocation = incoming.recv().await.unwrap();
    request(
        &runtime,
        owner.clone(),
        json!({"action":"publish","providerId":"test","models":[model("manual")]}),
    )
    .await
    .unwrap();
    assert!(pending.await.unwrap().is_err());
    tokio::time::timeout(Duration::from_secs(5), async {
        while !invocation.cancelled.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    compare(
        "manualReplacement",
        json!({"cancelled":invocation.cancelled.load(Ordering::SeqCst)}),
    );
    returned(invocation, json!({"models":[model("late")]})).await;
    assert_eq!(runtime.providers().await[0].models()[0].id, "manual");
}

#[tokio::test]
async fn cancellation_before_or_while_waiting_for_admission_does_not_supersede_existing_work() {
    let (runtime, owner, _, gate, mut incoming) = setup();
    let pending = start(&runtime, &owner);
    let invocation = incoming.recv().await.unwrap();
    for before in [true, false] {
        let signal = CancellationToken::new();
        let state = runtime.state.lock().await;
        if before {
            signal.cancel();
        }
        let cancelled = tokio::spawn({
            let runtime = runtime.clone();
            let owner = owner.clone();
            let signal = signal.clone();
            async move {
                runtime
                    .request(
                        owner,
                        json!({"port":"providers","action":"refresh","providerId":"test"}),
                        signal,
                    )
                    .await
            }
        });
        tokio::task::yield_now().await;
        signal.cancel();
        drop(state);
        let result = cancelled.await.unwrap();
        let previous_cancelled = runtime
            .publications
            .lock()
            .unwrap()
            .get(invocation.ticket.as_str().unwrap())
            .unwrap()
            .upgrade()
            .unwrap()
            .signal
            .is_cancelled();
        compare(
            "cancelledBeforeStart",
            json!({"aborted":result.unwrap_err().contains("cancelled"),"previousCancelled":previous_cancelled,"calls":gate.calls.load(Ordering::SeqCst)}),
        );
    }
    returned(invocation, json!({"models":[model("original-refresh")]})).await;
    pending.await.unwrap().unwrap();
}

#[tokio::test]
async fn child_refresh_tickets_and_generations_are_isolated_from_parent_and_sibling() {
    use rpi_agent::tool_execution_scope::ToolExecutionScope;
    let (runtime, owner, _, _gate, mut incoming) = setup();
    let child_owner = NativeOwner {
        child: true,
        ..owner.clone()
    };
    let first = Arc::new(ToolExecutionScope::default());
    let second = Arc::new(ToolExecutionScope::default());
    let parent_call = start(&runtime, &owner);
    let parent = incoming.recv().await.unwrap();
    let child_call = tokio::spawn({
        let (runtime, owner, first) = (runtime.clone(), child_owner.clone(), first.clone());
        async move {
            first
                .run(request(
                    &runtime,
                    owner,
                    json!({"action":"refresh","providerId":"test"}),
                ))
                .await
        }
    });
    let child = incoming.recv().await.unwrap();
    assert!(request(
        &runtime,
        owner.clone(),
        publish(&child.ticket, "forged-parent")
    )
    .await
    .is_err());
    assert!(second
        .run(request(
            &runtime,
            child_owner.clone(),
            publish(&child.ticket, "forged-sibling")
        ))
        .await
        .is_err());
    assert!(first
        .run(request(
            &runtime,
            child_owner.clone(),
            publish(&parent.ticket, "forged-child")
        ))
        .await
        .is_err());
    let committed = first
        .run(request(
            &runtime,
            child_owner.clone(),
            publish(&child.ticket, "child"),
        ))
        .await
        .unwrap();
    assert!(!parent.cancelled.load(Ordering::SeqCst));
    returned(child, json!({"published":true})).await;
    assert_eq!(child_call.await.unwrap().unwrap(), committed);
    returned(parent, json!({"models":[model("parent")]})).await;
    parent_call.await.unwrap().unwrap();
    assert_eq!(runtime.providers().await[0].models()[0].id, "parent");
    let snapshot = first
        .run(request(
            &runtime,
            child_owner.clone(),
            json!({"action":"inspect"}),
        ))
        .await
        .unwrap();
    assert_eq!(snapshot["providers"][0]["models"][0]["id"], "child");

    let late_call = tokio::spawn({
        let (runtime, owner, first) = (runtime.clone(), child_owner.clone(), first.clone());
        async move {
            first
                .run(request(
                    &runtime,
                    owner,
                    json!({"action":"refresh","providerId":"test"}),
                ))
                .await
        }
    });
    let late = incoming.recv().await.unwrap();
    first.close_runtime();
    assert!(first
        .run(request(
            &runtime,
            child_owner,
            publish(&late.ticket, "late")
        ))
        .await
        .is_err());
    returned(late, json!({"models":[model("late")]})).await;
    assert!(late_call.await.unwrap().is_err());
    assert_eq!(runtime.providers().await[0].models()[0].id, "parent");
}
