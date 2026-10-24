use super::*;
use rpi_ai::identity::{self, RequestIdentity};

struct IdentityProvider {
    models: Vec<Model>,
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
        _: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        // This is the same phase where AuthBinding::prepare captures identity
        // for native requestAuth, after RuntimeProvider's forwarding task hop.
        self.seen.lock().unwrap().push((
            "auth/stream",
            identity::current(),
            identity::turn_index(),
        ));
        identity::next_request();
        error_stream(model, "identity probe complete".into()).await
    }
}

#[tokio::test]
async fn runtime_provider_forwarding_preserves_caller_operation_and_options_fallback() {
    for task_local in [true, false] {
        let (runtime, _, _) = fixture();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        runtime.state.lock().await.chains.insert(
            "test".into(),
            Arc::new(IdentityProvider {
                models: vec![model("old")],
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
            assert_eq!(seen.len(), 2);
            for (phase, observed, turn) in seen.iter() {
                assert_eq!(
                    observed.as_ref(),
                    Some(&expected),
                    "{phase}, task_local={task_local}"
                );
                assert_eq!(*turn, if task_local { Some(0) } else { None });
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
