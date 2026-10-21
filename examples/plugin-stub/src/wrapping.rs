//! Native-library fixture for the SDK's private account delegation path.
use rpi_ai::{AssistantMessageEventStream, Context, Model, Provider, SimpleStreamOptions};
use rpi_plugin_sdk::{
    provider::{
        composition::{self, Continuation},
        ProviderContext,
    },
    *,
};
use std::{ffi::c_void, sync::Arc};

struct Pool {
    continuation: Continuation,
    models: Vec<Model>,
}
#[async_trait::async_trait]
impl Provider for Pool {
    fn id(&self) -> &str {
        "stub-auth"
    }
    fn models(&self) -> &[Model] {
        &self.models
    }
    async fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let receipt = self
            .continuation
            .resolve_auth("stub-pool", "second", options)
            .await
            .expect("fixture auth resolution");
        self.continuation
            .provider(Some(receipt))
            .stream_simple(model, context, options)
            .await
    }
}
fn factory(
    _: &ProviderContext,
    model: &Model,
    _: &SimpleStreamOptions,
    continuation: Continuation,
) -> Result<Arc<dyn Provider>, String> {
    Ok(Arc::new(Pool {
        continuation,
        models: vec![model.clone()],
    }))
}
extern "C" fn read(
    request: StbStringRef,
    _: *const AuthRequestControl,
    out: *mut StbString,
    _: *mut c_void,
) -> i32 {
    guard_or(1, || {
        let request: serde_json::Value = serde_json::from_str(unsafe { request.as_str() }).unwrap();
        if request["action"] != "read"
            || request["accountId"] != "second"
            || request["providerId"] != "stub-auth"
        {
            return 1;
        }
        unsafe {
            *out=StbString::from_string(r#"{"version":1,"kind":"credential","payload":{"type":"api_key","key":"stub-pool-key"}}"#.into());
        }
        0
    })
}
extern "C" fn release(_: StbStringRef, _: *mut c_void) {}
pub fn register(api: &PluginApi) -> Result<(), String> {
    let register = api
        .register_credential_store
        .ok_or("host lacks native credential stores")?;
    if register(
        StbStringRef::from_str(r#"{"version":1,"id":"stub-pool"}"#),
        read,
        release,
        callbacks::free_string,
        std::ptr::null_mut(),
    ) != 0
    {
        return Err("credential store registration failed".into());
    }
    composition::register(
        api,
        serde_json::json!({"version":1,"id":"stub-pool","providers":["stub-auth"]}),
        factory,
    )
}
