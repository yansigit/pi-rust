//! Real SDK adapters across a DLL boundary; the target lives in another DLL.
use rpi_ai::{AssistantMessageEventStream, Context, Model, Provider, SimpleStreamOptions};
use rpi_plugin_sdk::{
    self as sdk,
    provider::composition::{self, AmbientTarget, Continuation},
};
use std::sync::Arc;

struct Wrapper {
    models: Vec<Model>,
    continuation: Continuation,
}
#[async_trait::async_trait]
impl Provider for Wrapper {
    fn id(&self) -> &str {
        &self.models[0].provider
    }
    fn models(&self) -> &[Model] {
        &self.models
    }
    async fn stream_simple(
        &self,
        _: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let c = &self.continuation;
        assert!(c.supports_delegation_v2().await.unwrap());
        let target: AmbientTarget = c
            .resolve_target("c", "m", options.signal.clone())
            .await
            .unwrap();
        assert_eq!(target.model().provider, "c");
        let receipt = c
            .resolve_ambient(&target, options.signal.clone())
            .await
            .unwrap();
        let provider = c.delegate(&target, Some(receipt.clone())).unwrap();
        let mut upstream = provider
            .stream_simple(target.model(), context, options)
            .await;
        let (mut producer, stream) =
            rpi_ai::event_stream::create_assistant_message_event_stream_with_cancellation(
                &options.signal,
            );
        let c = c.clone();
        let marker = options
            .metadata
            .as_ref()
            .unwrap()
            .get("marker")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();
        tokio::spawn(async move {
            let mut terminal = None;
            while let Some(event) = upstream.next().await {
                if event.is_terminal() {
                    terminal = Some(event);
                    break;
                }
                if !producer.push(event).await {
                    break;
                }
            }
            drop(upstream);
            if producer.cancellation_token().is_cancelled() {
                std::fs::write(format!("{marker}.cancelled"), "cancelled").unwrap();
                return;
            }
            // Clone only to prove both capabilities really became unusable at the host.
            c.release_auth(receipt.clone()).await.unwrap();
            assert!(
                c.release_auth(receipt).await.is_err(),
                "receipt remained live"
            );
            c.release_target(target.clone()).await.unwrap();
            assert!(
                c.resolve_ambient(&target, Default::default())
                    .await
                    .is_err(),
                "target remained live"
            );
            std::fs::write(format!("{marker}.released"), "released").unwrap();
            producer.push(terminal.expect("target terminal")).await;
        });
        stream
    }
}
extern "C" fn unused(
    _: sdk::StbStringRef,
    _: *mut sdk::StbString,
    _: *mut std::ffi::c_void,
) -> i32 {
    1
}
sdk::export_plugin!(|api| {
    let model = Model::new(
        "m",
        "SDK source",
        rpi_ai::Api::Faux,
        "sdk-source",
        "https://unused.invalid",
    );
    assert_eq!(
        api.declare.unwrap()(sdk::StbStringRef::from_str(
            &serde_json::json!({"providers":[{"id":"sdk-source","models":[model]}]}).to_string()
        )),
        0
    );
    assert_eq!(
        api.register_provider.unwrap()(
            sdk::StbStringRef::from_str("sdk-source"),
            sdk::StbStringRef::from_str("https://unused.invalid"),
            sdk::StbStringRef::from_str("faux"),
            unused,
            sdk::callbacks::free_string,
            std::ptr::null_mut()
        ),
        0
    );
    composition::register(
        api,
        serde_json::json!({"version":1,"id":"sdk-wrapper","providers":["sdk-source"]}),
        |_, model, _, continuation| {
            if model.provider == "sdk-source" {
                Ok(Arc::new(Wrapper {
                    models: vec![model.clone()],
                    continuation,
                }))
            } else {
                Ok(continuation.provider(None))
            }
        },
    )
    .map(|_| 0)
    .unwrap_or(1)
});
