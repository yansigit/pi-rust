use super::v2_tests::Resolver;
use super::*;
use crate::delegation::{DelegationEpoch, DelegationResolver, DelegationScopeId};

struct Trace {
    edges: std::collections::BTreeMap<String, String>,
    events: Mutex<Vec<String>>,
}
struct Physical {
    id: String,
    models: Vec<Model>,
    trace: Arc<Trace>,
}
#[async_trait::async_trait]
impl Provider for Physical {
    fn id(&self) -> &str {
        &self.id
    }
    fn models(&self) -> &[Model] {
        &self.models
    }
    async fn stream_simple(
        &self,
        model: &Model,
        _: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.trace
            .events
            .lock()
            .unwrap()
            .push(format!("base:{}", self.id));
        let AttemptAuth::Replace(auth) = &options.authentication else {
            panic!("unsealed delegation")
        };
        assert_eq!(
            auth.for_provider(&self.id).unwrap().api_key.as_deref(),
            Some("target-secret")
        );
        assert!(options.api_key.is_none());
        assert!(options.env.is_empty());
        assert!(options.headers.is_none());
        let (mut producer, stream) = rpi_ai::event_stream::create_assistant_message_event_stream();
        producer
            .push(AssistantMessageEvent::Done {
                reason: rpi_ai::DoneReason::Stop,
                message: {
                    let mut message =
                        AssistantMessage::empty(model.api.clone(), &model.provider, &model.id, 0);
                    message.stop_reason = rpi_ai::StopReason::Stop;
                    message
                },
            })
            .await;
        stream
    }
}
fn action(input: Value) -> Result<Value, String> {
    let mut out = StbString::empty();
    let status = continuation_action(
        StbStringRef::from_str(&input.to_string()),
        std::ptr::null(),
        &mut out,
        std::ptr::null_mut(),
    );
    let out = unsafe { OwnedString::from_raw(out, Some(crate::host_free_string)) };
    let value: Value = serde_json::from_str(&out.to_string_lossy()).unwrap();
    if status == 0 {
        Ok(value)
    } else {
        Err(value["error"].as_str().unwrap().into())
    }
}
extern "C" fn wrapper(
    request: StbStringRef,
    emit: Option<ProviderStreamEmitFn>,
    data: *mut c_void,
    control: *const ProviderStreamControl,
    _: Option<rpi_plugin_sdk::ProviderStreamCompletionFn>,
    _: *mut c_void,
    out: *mut StbString,
    state: *mut c_void,
) -> i32 {
    let state = unsafe { &*state.cast::<Trace>() };
    let input: Value = serde_json::from_str(unsafe { request.as_str() }).unwrap();
    let provider = input["model"]["provider"].as_str().unwrap();
    state
        .events
        .lock()
        .unwrap()
        .push(format!("wrapper:{provider}"));
    let continuation = input["__rpiContinuation"].clone();
    assert!(!continuation.is_null());
    let request = if let Some(target_provider) = state.edges.get(provider) {
        let resolved = (|| {
            let target = action(
                json!({"version":2,"action":"resolveTarget","continuation":continuation,"provider":target_provider,"model":"m"}),
            )?;
            let auth = action(
                json!({"version":2,"action":"auth.resolveAmbient","continuation":continuation,"provider":target_provider,"model":"m","target":target["target"]}),
            )?;
            Ok::<_, String>(
                json!({"version":2,"continuation":continuation,"target":target["target"],"authReceipt":auth["receipt"]}),
            )
        })();
        match resolved {
            Ok(request) => request,
            Err(error) => {
                return write_result(
                    out,
                    Ok(serde_json::to_value(AssistantMessage::terminal(
                        rpi_ai::Api::Faux,
                        provider,
                        "m",
                        rpi_ai::StopReason::Error,
                        error,
                        0,
                    ))
                    .unwrap()),
                )
            }
        }
    } else {
        json!({"version":1,"continuation":continuation})
    };
    delegate_stream(
        StbStringRef::from_str(&request.to_string()),
        emit,
        data,
        control,
        None,
        std::ptr::null_mut(),
        out,
        std::ptr::null_mut(),
    )
}
async fn chain(edges: &[(&str, &str)], concurrent: bool) -> (Vec<String>, Vec<AssistantMessage>) {
    let trace = Arc::new(Trace {
        edges: edges
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect(),
        events: Mutex::new(Vec::new()),
    });
    let mut registry = crate::ExtensionRegistry::new();
    assert!(!registry.register_provider_wrapper(RegisteredWrapper {
        action_bridge: None,
        plugin: "global".into(),
        manifest: WrapperManifest {
            version: 1,
            id: "global".into(),
            providers: None
        },
        stream: wrapper,
        free: crate::host_free_string,
        context: Arc::as_ptr(&trace).cast_mut().cast(),
    }));
    let session = super::tests::session(&registry);
    let bases: Vec<Arc<dyn Provider>> = ["a", "b", "c"]
        .into_iter()
        .map(|id| {
            Arc::new(Physical {
                id: id.into(),
                models: vec![Model::new(
                    "m",
                    "M",
                    rpi_ai::Api::Faux,
                    id,
                    "https://bound.invalid",
                )],
                trace: trace.clone(),
            }) as Arc<dyn Provider>
        })
        .collect();
    let chains = wrap_providers(&session, bases);
    let epoch = DelegationEpoch::new(DelegationScopeId::fresh(), chains.clone());
    let resolver = Arc::new(Resolver(epoch.clone()));
    let port: Arc<dyn DelegationResolver> = resolver;
    let invoke = || async {
        let binding = DelegationBinding::root(Arc::downgrade(&port), epoch.clone(), "a").unwrap();
        let mut options = SimpleStreamOptions::default();
        options.api_key = Some("source-secret".into());
        options.env.insert("SECRET".into(), "source-env".into());
        binding
            .scope(async {
                chains[0]
                    .stream_simple(&chains[0].models()[0], &Context::default(), &options)
                    .await
                    .result()
                    .await
                    .unwrap()
            })
            .await
    };
    let messages = if concurrent {
        let (a, b) = tokio::join!(invoke(), invoke());
        vec![a, b]
    } else {
        vec![invoke().await]
    };
    let events = trace.events.lock().unwrap().clone();
    (events, messages)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn global_registration_runs_distinct_target_instances_through_nested_virtuals() {
    let (events, messages) = chain(&[("a", "b"), ("b", "c")], false).await;
    assert_eq!(events, ["wrapper:a", "wrapper:b", "wrapper:c", "base:c"]);
    assert_eq!(messages[0].stop_reason, rpi_ai::StopReason::Stop);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_invocations_have_no_global_active_ban() {
    let (events, messages) = chain(&[("a", "b")], true).await;
    for event in ["wrapper:a", "wrapper:b", "base:b"] {
        assert_eq!(
            events
                .iter()
                .filter(|actual| actual.as_str() == event)
                .count(),
            2
        );
    }
    assert!(messages
        .iter()
        .all(|message| message.stop_reason == rpi_ai::StopReason::Stop));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn virtual_cycle_is_refused_before_reentering_a() {
    let (events, messages) = chain(&[("a", "b"), ("b", "a")], false).await;
    assert_eq!(events, ["wrapper:a", "wrapper:b"]);
    assert_eq!(messages[0].stop_reason, rpi_ai::StopReason::Error);
    assert!(messages[0]
        .error_message
        .as_ref()
        .unwrap()
        .contains("provider delegation cycle"));
}
