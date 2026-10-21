//! Loaded-library fixture for initialization before any host credential read.
use rpi_plugin_sdk::*;
use serde_json::{json, Value};
use std::{ffi::c_void, io::Write, path::Path};

extern "C" fn begin(_: *mut c_void) {}
extern "C" fn finish(context: *mut c_void) -> i32 {
    unsafe { drop(Box::from_raw(context.cast::<PluginApi>())) };
    0
}
extern "C" fn request(
    input: StbStringRef,
    _: *const AuthRequestControl,
    out: *mut StbString,
    context: *mut c_void,
) -> i32 {
    guard_or(1, || {
        let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
        let api = unsafe { &*context.cast::<PluginApi>() };
        let client = provider::native::Client::new(api).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let call = |timeout| {
            runtime.block_on(client.request(
                json!({"port":"auth","action":"credentialStore","providerId":"stub-auth","lockTimeoutMs":timeout}),
                rpi_ai::SimpleStreamOptions::default().signal,
            ))
        };
        let result = match input["action"].as_str() {
            Some("initialize") => {
                assert!(Path::new(input["input"]["agentDir"].as_str().unwrap()).is_absolute());
                if std::env::var_os("RPI_STUB_INITIALIZE_FAIL").is_some() {
                    json!({"version":1,"error":"fixture initialization refused"})
                } else {
                    assert_eq!(call(1000).unwrap()["committed"], true);
                    eprintln!("native initialization committed before credential resolution");
                    json!({"version":1,"result":null})
                }
            }
            Some("credentialStore") => {
                assert_eq!(input["input"]["expiryUnit"], "seconds");
                assert!(call(0)
                    .unwrap_err()
                    .contains("credential store lock timed out"));
                let path = Path::new(input["input"]["authPath"].as_str().unwrap());
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(path)
                    .unwrap();
                file.write_all(
                    br#"{"stub-auth":{"type":"api_key","key":"initialization-fixture-only"}}"#,
                )
                .unwrap();
                file.sync_all().unwrap();
                json!({"version":1,"result":{"committed":true}})
            }
            _ => return 1,
        };
        unsafe { *out = StbString::from_string(result.to_string()) };
        0
    })
}

pub fn register(api: &PluginApi) -> Result<(), String> {
    let declare = api.declare.ok_or("missing declarations")?;
    let model = rpi_ai::Model::new(
        "stub-model",
        "Stub",
        rpi_ai::Api::Faux,
        "stub-provider",
        "https://stub.example",
    );
    if declare(StbStringRef::from_str(&json!({"providers":[{"id":"stub-auth","models":[]},{"id":"stub-provider","requiresAuth":false,"models":[model]}]}).to_string())) != 0 {
        return Err("initialization declaration failed".into());
    }
    let context = Box::into_raw(Box::new(*api)).cast();
    if api.register_plugin_lifecycle.ok_or("missing lifecycle")?(begin, finish, context) != 0 {
        unsafe { drop(Box::from_raw(context.cast::<PluginApi>())) };
        return Err("initialization lifecycle failed".into());
    }
    if api.register_provider_runtime.ok_or("missing runtime")?(
        StbStringRef::from_str(
            r#"{"version":1,"id":"stub-auth","actions":["initialize","credentialStore"]}"#,
        ),
        request,
        callbacks::free_string,
        context,
    ) != 0
    {
        return Err("initialization registration failed".into());
    }
    Ok(())
}
