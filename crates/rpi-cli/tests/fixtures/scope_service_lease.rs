//! Independent DLL: SDK grant consumer + Fabric-shaped service forwarding to host.
use rpi_plugin_sdk::{self as sdk, provider::native::Client, StbString, StbStringRef};
use serde_json::{json, Value};
use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};
static API: Mutex<Option<sdk::PluginApi>> = Mutex::new(None);
static STOP: AtomicBool = AtomicBool::new(false);
static WORKERS: Mutex<Vec<std::thread::JoinHandle<()>>> = Mutex::new(Vec::new());
fn out(output: *mut StbString, value: Value) -> i32 {
    unsafe {
        *output = StbString::from_string(value.to_string());
    }
    0
}
extern "C" fn service(
    input: StbStringRef,
    _: *const sdk::AuthRequestControl,
    output: *mut StbString,
    _: *mut c_void,
) -> i32 {
    sdk::guard_or(1, || {
        let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
        let action = match input["method"].as_str().unwrap() {
            "isIdle" => sdk::RuntimeActionId::IsIdle,
            "compact" => sdk::RuntimeActionId::Compact,
            _ => return out(output, json!({"version":1,"error":"unknown method"})),
        };
        let api = API.lock().unwrap().unwrap();
        let mut response = StbString::empty();
        let rc = (api.runtime_action)(
            action.into(),
            StbStringRef::from_str("{}"),
            &mut response,
            api.user_data,
        );
        let response =
            unsafe { sdk::callbacks::OwnedString::from_raw(response, Some(api.free_string)) };
        let response: Value = serde_json::from_str(&response.to_string_lossy()).unwrap();
        if rc == 0 {
            out(output, json!({"version":1,"result":response}))
        } else {
            out(output, json!({"version":1,"error":response["error"]}))
        }
    })
}
extern "C" fn request(input: StbStringRef, output: *mut StbString, _: *mut c_void) -> i32 {
    sdk::guard_or(1, || {
        let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
        let marker = input["options"]["metadata"]["marker"]
            .as_str()
            .unwrap()
            .to_owned();
        let cancel = input["options"]["metadata"]["cancel"] == true;
        let client = Client::new(&API.lock().unwrap().unwrap())
            .unwrap()
            .scoped()
            .unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let lease = runtime
            .block_on(client.scope_service_lease(
                "pi-fabric",
                1,
                &["isIdle", "compact"],
                Default::default(),
            ))
            .unwrap();
        WORKERS.lock().unwrap().push(std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async move {
                // Prove the ordinary invocation really ended, not just terminal emission.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while client
                    .request(json!({"port":"capabilities"}), Default::default())
                    .await
                    .is_ok()
                {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "callback did not expire"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                std::fs::write(format!("{marker}.ready"), "expired").unwrap();
                while !std::path::Path::new(&format!("{marker}.go")).exists()
                    && !STOP.load(Ordering::SeqCst)
                {
                    assert!(std::time::Instant::now() < deadline, "worker not released");
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                // Shutdown invalidates owner authority but cannot leave an unjoined DLL thread.
                let idle = lease.call("isIdle", json!({}), Default::default()).await;
                let signal = rpi_ai::SimpleStreamOptions::default().signal;
                if cancel {
                    let signal = signal.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        signal.cancel();
                    });
                }
                let compact = lease.call("compact", json!({}), signal).await;
                std::fs::write(
                    format!("{marker}.result"),
                    json!({"idle":idle,"compact":compact}).to_string(),
                )
                .unwrap();
                let _ = lease.release().await;
            });
        }));
        let mut message = rpi_ai::AssistantMessage::empty(rpi_ai::Api::Faux, "scope-lease", "m", 0);
        message.stop_reason = rpi_ai::StopReason::Stop;
        message.content.push(
            serde_json::from_value(json!({"type":"text","text":"callback returned"})).unwrap(),
        );
        out(output, serde_json::to_value(message).unwrap())
    })
}
extern "C" fn begin(_: *mut c_void) {
    STOP.store(true, Ordering::SeqCst);
}
extern "C" fn finish(_: *mut c_void) -> i32 {
    for worker in WORKERS.lock().unwrap().drain(..) {
        worker.join().unwrap();
    }
    0
}
sdk::export_plugin!(|api| {
    *API.lock().unwrap() = Some(*api);
    STOP.store(false, Ordering::SeqCst);
    api.register_plugin_lifecycle.unwrap()(begin, finish, std::ptr::null_mut());
    assert_eq!(api.register_service.unwrap()(StbStringRef::from_str(&json!({"version":1,"id":"pi-fabric","serviceVersion":1,"methods":["isIdle","compact"],"topics":[]}).to_string()), service, sdk::callbacks::free_string, std::ptr::null_mut()), 0);
    let model = rpi_ai::Model::new("m", "Scope Lease", rpi_ai::Api::Faux, "scope-lease", "");
    assert_eq!(
        api.declare.unwrap()(StbStringRef::from_str(
            &json!({"providers":[{"id":"scope-lease","models":[model]}]}).to_string()
        )),
        0
    );
    api.register_provider.unwrap()(
        StbStringRef::from_str("scope-lease"),
        StbStringRef::from_str(""),
        StbStringRef::from_str("faux"),
        request,
        sdk::callbacks::free_string,
        std::ptr::null_mut(),
    )
});
