//! Loaded-library acceptance fixture for private runtime ports and durable login.
use rpi_plugin_sdk::*;
use serde_json::{json, Value};
use std::ffi::c_void;
fn model(id: &str) -> rpi_ai::Model {
    rpi_ai::Model::new(
        id,
        id,
        rpi_ai::Api::Faux,
        "stub-provider",
        "https://stub.example",
    )
}
extern "C" fn operations(
    input: StbStringRef,
    _: *const AuthRequestControl,
    out: *mut StbString,
    _: *mut c_void,
) -> i32 {
    guard_or(1, || {
        let request: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
        let response = match request["action"].as_str() {
            Some("refreshCatalog") => {
                json!({"version":1,"result":{"models":[model("catalog-before"),model("catalog-after")]}})
            }
            Some("preflight") => json!({"version":1,"result":null}),
            _ => return 1,
        };
        unsafe {
            *out = StbString::from_string(response.to_string());
        }
        0
    })
}
extern "C" fn startup(_: StablePluginEvent, context: *mut c_void) -> i32 {
    guard_or(1, || {
        let api = unsafe { &*context.cast::<PluginApi>() };
        let client = provider::native::Client::new(api).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let call = |args| client.request(args, rpi_ai::SimpleStreamOptions::default().signal);
            let before = call(json!({"port":"providers","action":"inspect"})).await.unwrap();
            let revision = before["revision"].as_u64().unwrap();
            let refreshed = call(json!({"port":"providers","action":"refresh","providerId":"stub-provider","allowNetwork":false})).await.unwrap();
            assert_eq!(refreshed["revision"], revision + 1);
            let after = call(json!({"port":"providers","action":"inspect"})).await.unwrap();
            assert!(after["providers"].as_array().unwrap().iter().any(|p| p["id"] == "stub-provider" && p["models"].as_array().unwrap().iter().any(|m| m["id"] == "catalog-after")));
            assert!(call(json!({"port":"providers","action":"publish","providerId":"anthropic","models":[]})).await.is_err());
            let methods = call(json!({"port":"auth","action":"methods","providerId":"stub-auth"})).await.unwrap();
            assert_eq!(methods[0]["id"], "browser");
            let login = call(json!({"port":"auth","action":"login","providerId":"stub-auth","methodId":"browser"})).await.unwrap();
            assert_eq!(login["interaction"]["title"], "Stub login");
            assert!(login["interaction"].get("continuation").is_none());
            let ticket = &login["loginId"];
            assert_eq!(call(json!({"port":"auth","action":"continue","loginId":ticket,"value":"stub-code"})).await.unwrap()["kind"], "success");
            assert!(call(json!({"port":"auth","action":"continue","loginId":ticket,"value":"stub-code"})).await.is_err());
            let status = call(json!({"port":"auth","action":"status","providerId":"stub-auth"})).await.unwrap();
            assert_eq!(status["expires"], 1234567890000_i64);
            assert!(!status.to_string().contains("stub-login-access"));
            eprintln!("native catalog and durable login fixture passed");
        });
        0
    })
}
extern "C" fn shutdown(_: StablePluginEvent, _: *mut c_void) -> i32 {
    eprintln!("native catalog shutdown fixture passed");
    0
}
extern "C" fn begin(_: *mut c_void) {}
extern "C" fn finish(context: *mut c_void) -> i32 {
    unsafe {
        drop(Box::from_raw(context.cast::<PluginApi>()));
    }
    0
}
extern "C" fn event(
    input: StbStringRef,
    _: *const AuthRequestControl,
    out: *mut StbString,
    _: *mut c_void,
) -> i32 {
    guard_or(1, || {
        let request: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
        let tag = request["tag"].as_u64().unwrap();
        assert!(request["identity"]["operationId"].is_string());
        assert!(request["identity"]["sessionId"].is_string());
        let result = if tag == EventTag::BeforeAgentStart as u64 {
            eprintln!("native operation start fixture passed");
            json!({"systemPrompt":"native fixture system prompt", "message":{
                "customType":"native-fixture", "content":"native custom context", "display":false
            }})
        } else if tag == EventTag::Context as u64 {
            assert!(request["identity"]["requestId"].is_string());
            assert!(request["identity"]["turnId"].is_string());
            assert!(request["event"]["messages"]
                .to_string()
                .contains("native custom context"));
            json!({})
        } else if request["event"]["message"]["kind"] == "assistant" {
            let mut message = request["event"]["message"].clone();
            message["content"] = json!([{"type":"text","text":"native event modified response"}]);
            eprintln!("native message result fixture passed");
            json!({"message":message})
        } else {
            json!({})
        };
        unsafe {
            *out = StbString::from_string(json!({"version":1,"result":result}).to_string());
        }
        0
    })
}
pub fn register(api: &PluginApi) -> Result<(), String> {
    let lifecycle = api.register_plugin_lifecycle.ok_or("missing lifecycle")?;
    let events = api.register_event_handler.ok_or("missing events")?;
    let operations_register = api
        .register_provider_runtime
        .ok_or("missing provider runtime")?;
    let declare = api.declare.ok_or("missing declarations")?;
    provider::native::Client::new(api)?;
    let result_events = api
        .register_event_result_handler
        .ok_or("missing awaited events")?;
    for tag in [
        EventTag::BeforeAgentStart,
        EventTag::Context,
        EventTag::MessageEnd,
    ] {
        if result_events(tag, event, callbacks::free_string, std::ptr::null_mut()) != 0 {
            return Err("event registration failed".into());
        }
    }

    if declare(StbStringRef::from_str(&json!({"providers":[{"id":"stub-provider","requiresAuth":false,"models":[model("catalog-before")]},{"id":"stub-auth","models":[]}]}).to_string())) != 0 { return Err("catalog declaration failed".into()); }
    if operations_register(
        StbStringRef::from_str(
            r#"{"version":1,"id":"stub-provider","actions":["preflight","refreshCatalog"]}"#,
        ),
        operations,
        callbacks::free_string,
        std::ptr::null_mut(),
    ) != 0
    {
        return Err("provider operations registration failed".into());
    }
    let context = Box::into_raw(Box::new(*api)).cast();
    if lifecycle(begin, finish, context) != 0 {
        unsafe {
            drop(Box::from_raw(context.cast::<PluginApi>()));
        }
        return Err("lifecycle registration failed".into());
    }
    if events(EventTag::SessionStart, startup, context) != 0 {
        return Err("startup registration failed".into());
    }
    if events(EventTag::SessionShutdown, shutdown, context) != 0 {
        return Err("shutdown registration failed".into());
    }
    Ok(())
}
