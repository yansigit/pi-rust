//! Private provider defaults must not read or refresh the dormant host credential.
use rpi_plugin_sdk::*;
use serde_json::{json, Value};
use std::ffi::c_void;

extern "C" fn request(
    input: StbStringRef,
    _: *const AuthRequestControl,
    out: *mut StbString,
    _: *mut c_void,
) -> i32 {
    guard_or(1, || {
        let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
        if input["action"] == "authMethods" {
            let method = json!({"id":"device","name":"Current login","kind":"oauth"});
            let result = match std::env::var("RPI_STUB_AUTH_METHODS")
                .unwrap_or_default()
                .as_str()
            {
                "empty" => json!([]),
                "invalid" => json!(42),
                "duplicate" => json!([method, method]),
                "blank" => json!([{"id":" ","name":"Current login","kind":"oauth"}]),
                "extra" => {
                    json!([{"id":"device","name":"Current login","kind":"oauth","secret":"private-method-data"}])
                }
                "unsupported" => json!([{"id":"key","name":"Key","kind":"api_key"}]),
                _ => json!([method]),
            };
            unsafe {
                *out = StbString::from_string(json!({"version":1,"result":result}).to_string());
            }
            return 0;
        }
        assert_eq!(input["action"], "requestAuth");
        assert_eq!(input["input"]["modelId"], "stub-model");
        let result = match std::env::var("RPI_STUB_REQUEST_AUTH").unwrap().as_str() {
            "prepared" => {
                json!({"version":1,"result":{"apiKey":"provider-prepared-key","headers":{"X-Provider-Private":"private-header"},"env":{"RPI_PRIVATE_REQUEST_ENV":"request-only"}}})
            }
            "ambient" => json!({"version":1,"result":null}),
            "policy" => {
                json!({"version":1,"result":{"resolve":{"apiKey":true,"oauth":false,"fallbackApiKey":"fallback-fixture-key"}}})
            }
            "invalid_policy" => {
                json!({"version":1,"result":{"resolve":{"apiKey":true,"oauth":false,"unknown":true}}})
            }
            "invalid" => json!({"version":1,"result":{"apiKey":42}}),
            _ => json!({"version":1,"error":"fixture provider refused request authentication"}),
        };
        unsafe {
            *out = StbString::from_string(result.to_string());
        }
        0
    })
}
pub fn register(api: &PluginApi) -> Result<(), String> {
    let model = rpi_ai::Model::new(
        "stub-model",
        "Stub",
        rpi_ai::Api::Faux,
        "stub-provider",
        "https://stub.example",
    );
    if api.declare.ok_or("missing declaration")?(StbStringRef::from_str(
        &json!({"providers":[{"id":"stub-provider","models":[model]}]}).to_string(),
    )) != 0
    {
        return Err("declaration failed".into());
    }
    if api.register_provider_auth.ok_or("missing auth")?(
        StbStringRef::from_str(
            &json!({"version":1,"id":"stub-provider","displayName":"Stub","apiKey":std::env::var("RPI_STUB_AUTH_METHODS").as_deref()!=Ok("unsupported"),"oauth":true,"methods":[{"id":"browser","name":"Original login","kind":"oauth"}]}).to_string(),
        ),
        super::on_auth_request,
        callbacks::free_string,
        std::ptr::null_mut(),
    ) != 0
    {
        return Err("auth registration failed".into());
    }
    if api.register_provider_runtime.ok_or("missing runtime")?(
        StbStringRef::from_str(
            r#"{"version":1,"id":"stub-provider","actions":["requestAuth","authMethods"]}"#,
        ),
        request,
        callbacks::free_string,
        std::ptr::null_mut(),
    ) != 0
    {
        return Err("runtime registration failed".into());
    }
    Ok(())
}
