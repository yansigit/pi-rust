use super::*;
use rpi_ai::identity::{self, RequestIdentity};

struct IdentityProvider {
    models: Vec<Model>,
    auth: Arc<crate::provider::auth::AuthBinding>,
    seen: Arc<std::sync::Mutex<Vec<(&'static str, Option<RequestIdentity>, Option<u64>)>>>,
}
#[async_trait::async_trait]
impl Provider for IdentityProvider {
    fn id(&self) -> &str {
        "test"
    }
    fn models(&self) -> &[Model] {
        &self.models
    }
    async fn preflight(&self, _: &Model, _: CancellationToken) -> Result<(), String> {
        self.seen
            .lock()
            .unwrap()
            .push(("preflight", identity::current(), identity::turn_index()));
        Ok(())
    }
    async fn stream_simple(
        &self,
        model: &Model,
        _: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let mut model = model.clone();
        let mut options = options.clone();
        self.auth.prepare(&mut model, &mut options).await.unwrap();
        // This is the same phase where AuthBinding::prepare captures identity
        // for native requestAuth, after RuntimeProvider's forwarding task hop.
        self.seen.lock().unwrap().push((
            "auth/stream",
            identity::current(),
            identity::turn_index(),
        ));
        identity::next_request();
        self.seen.lock().unwrap().push((
            "request-created",
            identity::current(),
            identity::turn_index(),
        ));
        let mut stream = error_stream(&model, "identity probe complete".into()).await;
        let seen = self.seen.clone();
        stream.observe_polls(move || {
            seen.lock()
                .unwrap()
                .push(("source-poll", identity::current(), identity::turn_index()))
        });
        stream
    }
}

#[tokio::test]
async fn runtime_provider_forwarding_preserves_caller_operation_and_options_fallback() {
    for task_local in [true, false] {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        extern "C" fn native_auth(
            input: rpi_plugin_sdk::StbStringRef,
            _: *const rpi_plugin_sdk::AuthRequestControl,
            output: *mut rpi_plugin_sdk::StbString,
            data: *mut std::ffi::c_void,
        ) -> i32 {
            let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
            assert_eq!(input["action"], "requestAuth");
            let seen = unsafe {
                &*data.cast::<std::sync::Mutex<Vec<(&'static str, Option<RequestIdentity>, Option<u64>)>>>()
            };
            seen.lock().unwrap().push((
                "native-requestAuth",
                rpi_extensions::delegation_test_hooks::native_identity(),
                None,
            ));
            unsafe {
                *output = rpi_plugin_sdk::StbString::from_string(
                    json!({"version":1,"result":{"apiKey":"fixture-only"}}).to_string(),
                );
            }
            0
        }
        let bridge = rpi_extensions::ActionBridge::new(
            tokio::runtime::Handle::current(),
            Arc::new(Host::default()),
        );
        let plugin = bridge.clone();
        let (runtime, _, _) = fixture_with(|registry| {
            registry.register_provider_auth(rpi_extensions::provider_auth::RegisteredAuthProvider {
                action_bridge: None, plugin: "test-plugin".into(),
                manifest: serde_json::from_value(json!({"version":1,"id":"test","displayName":"Test","apiKey":true,"oauth":false,"methods":[]})).unwrap(),
                request_fn: native_auth, plugin_free_string: rpi_plugin_sdk::callbacks::free_string,
                user_data: Arc::as_ptr(&seen).cast_mut().cast(),
            });
            registry.register_provider_runtime(
                rpi_extensions::provider_runtime::RegisteredProviderRuntime {
                    action_bridge: Some(Arc::downgrade(&plugin)),
                    plugin: "test-plugin".into(),
                    manifest: rpi_extensions::provider_runtime::RuntimeManifest {
                        version: 1,
                        id: "test".into(),
                        actions: vec!["requestAuth".into()],
                    },
                    request_fn: native_auth,
                    free: rpi_plugin_sdk::callbacks::free_string,
                    context: Arc::as_ptr(&seen).cast_mut().cast(),
                },
            );
        });
        let auth = runtime.auth.get("test").unwrap().clone();
        runtime.state.lock().await.chains.insert(
            "test".into(),
            Arc::new(IdentityProvider {
                models: vec![model("old")],
                auth,
                seen: seen.clone(),
            }),
        );
        // Use the published RuntimeProvider entry rather than composing wrappers
        // or calling forward_scoped directly.
        let provider = runtime
            .providers()
            .await
            .into_iter()
            .find(|p| p.id() == "test")
            .unwrap();
        let caller = RequestIdentity::operation(Some("isolated-session".into()), "forwarding-test");
        let options_identity = RequestIdentity::operation(None, "options-fallback");
        let options = SimpleStreamOptions {
            identity: Some(options_identity.clone()),
            ..Default::default()
        };
        let run = async {
            let expected = if task_local {
                identity::next_turn();
                identity::current().unwrap()
            } else {
                options_identity.clone()
            };
            let mut stream = provider
                .stream_simple(&model("old"), &Context::default(), &options)
                .await;
            while stream.next().await.is_some() {}
            let seen = seen.lock().unwrap();
            for phase in [
                "preflight",
                "native-requestAuth",
                "auth/stream",
                "source-poll",
            ] {
                assert!(
                    seen.iter().any(|(p, _, _)| *p == phase),
                    "missing {phase}: {seen:?}"
                );
            }
            for (phase, observed, turn) in seen.iter() {
                let mut expected = expected.clone();
                if matches!(*phase, "source-poll" | "request-created") {
                    expected.request_id = seen
                        .iter()
                        .find(|(phase, _, _)| *phase == "request-created")
                        .unwrap()
                        .1
                        .as_ref()
                        .unwrap()
                        .request_id
                        .clone();
                    assert!(expected.request_id.is_some());
                }
                assert_eq!(
                    observed.as_ref(),
                    Some(&expected),
                    "{phase}, task_local={task_local}"
                );
                // Native callbacks run on blocking threads; their captured invocation
                // carries the request identity, not Tokio's turn task-local.
                if *phase != "native-requestAuth" {
                    assert_eq!(*turn, if task_local { Some(0) } else { None });
                }
            }
            if task_local {
                // Inherit the actual operation Arc, so request state changes made
                // in auth/stream remain visible to the caller as well.
                assert!(identity::current().unwrap().request_id.is_some());
            } else {
                assert!(identity::current().is_none());
            }
        };
        if task_local {
            identity::scope(caller, run).await;
        } else {
            run.await;
        }
    }
}
