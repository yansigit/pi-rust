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
extern "C" fn service(
    input: StbStringRef,
    _: *const AuthRequestControl,
    out: *mut StbString,
    _: *mut c_void,
) -> i32 {
    guard_or(1, || {
        let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
        unsafe {
            *out = StbString::from_string(json!({"version":1,"result":input}).to_string());
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
            let listed = call(json!({"port":"services","action":"list"})).await.unwrap();
            assert!(listed.as_array().unwrap().iter().any(|service| service["id"] == "stub-accounts"));
            let subscription = call(json!({"port":"services","action":"subscribe","service":"stub-accounts","serviceVersion":1,"topic":"changed"})).await.unwrap();
            call(json!({"port":"services","action":"publish","service":"stub-accounts","serviceVersion":1,"topic":"changed","value":{"label":"fixture"}})).await.unwrap();
            let event = call(json!({"port":"services","action":"next","subscription":subscription["subscription"]})).await.unwrap();
            assert_eq!(event["value"]["label"], "fixture");
            assert_eq!(event["sequence"], 1);
            call(json!({"port":"services","action":"unsubscribe","subscription":subscription["subscription"]})).await.unwrap();
            eprintln!("native services fixture passed");
            let tools = call(json!({"port":"tools","action":"inspect"})).await.unwrap();
            assert!(tools.as_array().unwrap().iter().any(|tool| tool["name"] == "echo" && tool["active"] == true));
            let before_count = crate::ECHO_EXECUTIONS.load(std::sync::atomic::Ordering::SeqCst);
            let result = call(json!({"port":"tools","action":"invoke","call":{"type":"toolCall","id":"host-tool", "name":"echo","arguments":{"text":"native"}}})).await.unwrap();
            assert_eq!(result["result"]["content"][0]["text"], "echo: native");
            assert_eq!(crate::ECHO_EXECUTIONS.load(std::sync::atomic::Ordering::SeqCst), before_count + 1);
            for recording in [json!({"version":1,"isError":false,"result":{"content":[{"type":"text","text":"external result"}]}}), Value::Null] {
                let expected_error = recording.is_null();
                let result = call(json!({"port":"tools","action":"present","call":{"type":"toolCall","id":"recorded-tool", "name":"echo","arguments":{"text":"must not execute"},"recorded":recording}})).await.unwrap();
                assert_eq!(result["isError"], expected_error);
                if !expected_error { assert_eq!(result["result"]["content"][0]["text"], "external result"); }
            }
            assert_eq!(crate::ECHO_EXECUTIONS.load(std::sync::atomic::Ordering::SeqCst), before_count + 1);
            assert!(call(json!({"port":"tools","action":"present","call":{"type":"toolCall","id":"missing", "name":"echo","arguments":{"text":"never"}}})).await.is_err());
            eprintln!("native tools and inert replay fixture passed");
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
    context: *mut c_void,
) -> i32 {
    guard_or(1, || {
        let request: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
        let tag = request["tag"].as_u64().unwrap();
        assert!(request["identity"]["operationId"].is_string());
        assert!(request["identity"]["sessionId"].is_string());
        let result = if tag == EventTag::BeforeAgentStart as u64 {
            let api = unsafe { &*context.cast::<PluginApi>() };
            let client = provider::native::Client::new(api)
                .unwrap()
                .scoped()
                .unwrap();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let result = runtime.block_on(async move {
                tokio::spawn(async move { client.request(json!({"port":"services","action":"call","service":"stub-accounts","serviceVersion":1,"method":"resolve"}), rpi_ai::SimpleStreamOptions::default().signal).await }).await.unwrap().unwrap()
            });
            assert_eq!(result["identity"], request["identity"]);
            eprintln!("native scoped services fixture passed");
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
    let context = Box::into_raw(Box::new(*api)).cast();
    if lifecycle(begin, finish, context) != 0 {
        unsafe {
            drop(Box::from_raw(context.cast::<PluginApi>()));
        }
        return Err("lifecycle registration failed".into());
    }
    let register_service = api.register_service.ok_or("missing native services")?;
    if register_service(
        StbStringRef::from_str(
            r#"{"version":1,"id":"stub-accounts","serviceVersion":1,"methods":["resolve"],"topics":["changed"]}"#,
        ),
        service,
        callbacks::free_string,
        std::ptr::null_mut(),
    ) != 0
    {
        return Err("service registration failed".into());
    }
    for tag in [
        EventTag::BeforeAgentStart,
        EventTag::Context,
        EventTag::MessageEnd,
    ] {
        if result_events(tag, event, callbacks::free_string, context) != 0 {
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
    if events(EventTag::SessionStart, startup, context) != 0 {
        return Err("startup registration failed".into());
    }
    if events(EventTag::SessionShutdown, shutdown, context) != 0 {
        return Err("shutdown registration failed".into());
    }
    Ok(())
}
