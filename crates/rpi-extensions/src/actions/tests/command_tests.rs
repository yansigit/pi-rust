use super::*;
use crate::{commands::NativeCommand, ExtensionSession, PluginKeepalive};
use rpi_plugin_sdk::{AuthRequestControl, PluginApi};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize},
    Condvar,
};
use tokio_util::sync::CancellationToken;
struct Data {
    api: Mutex<Option<PluginApi>>,
    entered: AtomicUsize,
    returned: AtomicUsize,
    cancelled: AtomicBool,
    release: Mutex<bool>,
    changed: Condvar,
}
extern "C" fn callback(
    input: StbStringRef,
    control: *const AuthRequestControl,
    out: *mut StbString,
    context: *mut c_void,
) -> i32 {
    rpi_plugin_sdk::guard_or(1, || {
        let data = unsafe { &*context.cast::<Data>() };
        let input: Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
        let api = data.api.lock().unwrap().unwrap();
        let mut inspected = StbString::empty();
        assert_eq!(
            api.native_request.unwrap()(
                StbStringRef::from_str(r#"{"version":1,"port":"context","action":"inspect"}"#),
                control,
                &mut inspected,
                api.user_data
            ),
            0
        );
        let inspected = unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(inspected, Some(api.free_string))
        };
        let inspected: Value = serde_json::from_str(&inspected.to_string_lossy()).unwrap();
        assert_eq!(inspected["result"]["owner"]["plugin"], "command-owner");
        assert_eq!(inspected["result"]["owner"]["child"], false);
        data.entered.fetch_add(1, Ordering::SeqCst);
        if input["args"] == "hold" {
            let mut release = data.release.lock().unwrap();
            while !*release {
                let control = unsafe { &*control };
                if (control.is_cancelled)(control.user_data) != 0 {
                    data.cancelled.store(true, Ordering::SeqCst);
                }
                release = data
                    .changed
                    .wait_timeout(release, std::time::Duration::from_millis(2))
                    .unwrap()
                    .0;
            }
        }
        unsafe {
            *out = StbString::from_string(json!({"version":1,"result":input["args"]}).to_string());
        }
        data.returned.fetch_add(1, Ordering::SeqCst);
        0
    })
}
fn fixture() -> (
    Arc<crate::HostApi>,
    ExtensionSession,
    NativeCommand,
    Arc<Data>,
) {
    let master = ActionBridge::new(
        Handle::current(),
        Arc::new(MockHost {
            prompt: String::new(),
            saw: Mutex::new(vec![]),
        }),
    );
    let host = crate::HostApi::with_action_bridge(
        "command-owner",
        crate::ExtensionRegistry::new(),
        Arc::new(crate::NullDiagnostics),
        master.clone(),
    );
    let api = host.build_vtable();
    let data = Arc::new(Data {
        api: Mutex::new(Some(api)),
        entered: 0.into(),
        returned: 0.into(),
        cancelled: false.into(),
        release: Mutex::new(false),
        changed: Condvar::new(),
    });
    {
        let _registration = crate::enter_current_api(&host);
        let register = api.register_native_command.unwrap();
        for manifest in [
            r#"{"version":2,"name":"test","description":"test"}"#,
            r#"{"version":1,"name":"bad name","description":"test"}"#,
            r#"{"version":1,"name":"test","description":"test","owner":"forged"}"#,
        ] {
            assert_eq!(
                register(
                    StbStringRef::from_str(manifest),
                    callback,
                    crate::host_free_string,
                    Arc::as_ptr(&data).cast_mut().cast()
                ),
                -2
            );
        }
        let manifest =
            StbStringRef::from_str(r#"{"version":1,"name":"test","description":"test"}"#);
        assert_eq!(
            register(
                manifest,
                callback,
                crate::host_free_string,
                Arc::as_ptr(&data).cast_mut().cast()
            ),
            0
        );
        assert_eq!(
            register(
                manifest,
                callback,
                crate::host_free_string,
                Arc::as_ptr(&data).cast_mut().cast()
            ),
            1
        );
    }
    let registry = host.take_registry().unwrap();
    let snapshot = Arc::new(registry.snapshot());
    assert_eq!(snapshot.commands().len(), 1);
    let native = snapshot.commands()[0].native.as_ref().unwrap().clone();
    let session = ExtensionSession::from_parts(
        snapshot,
        Arc::new(PluginKeepalive::new(vec![], Some(master.clone()))),
        vec![],
        Some(master),
    );
    (host, session, native, data)
}
async fn until(predicate: impl Fn() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test(flavor = "current_thread")]
async fn command_scope_completes_without_retiring_owner_and_rejects_foreign_sessions() {
    let (_host, session, native, data) = fixture();
    for args in ["first", "second"] {
        assert_eq!(
            native
                .start(&session, "test", args.into(), CancellationToken::new())
                .unwrap()
                .wait()
                .await
                .unwrap(),
            args
        );
    }
    assert_eq!(data.returned.load(Ordering::SeqCst), 2);
    assert!(native
        .start(
            &ExtensionSession::none(),
            "test",
            "foreign".into(),
            CancellationToken::new()
        )
        .is_err());
    assert!(native
        .start(
            &session,
            "test",
            "x".repeat(1024 * 1024 + 1),
            CancellationToken::new()
        )
        .is_err());
    session.action_bridge().unwrap().invalidate();
    assert!(native
        .start(&session, "test", "stale".into(), CancellationToken::new())
        .is_err());
}
#[tokio::test(flavor = "current_thread")]
async fn cancellation_keeps_capacity_and_library_until_foreign_callback_returns() {
    for shutdown in [false, true] {
        let (host, session, mut native, data) = fixture();
        native.permits = Arc::new(tokio::sync::Semaphore::new(1));
        let keepalive = Arc::downgrade(&session.keepalive());
        let call = native
            .start(&session, "test", "hold".into(), CancellationToken::new())
            .unwrap();
        until(|| data.entered.load(Ordering::SeqCst) == 1).await;
        let rendered = Arc::new(AtomicBool::new(false));
        if shutdown {
            let rendered = rendered.clone();
            call.complete(move |_| rendered.store(true, Ordering::SeqCst));
            host.begin_shutdown();
        } else {
            drop(call);
        }
        until(|| data.cancelled.load(Ordering::SeqCst)).await;
        assert!(native
            .start(
                &session,
                "test",
                "overflow".into(),
                CancellationToken::new()
            )
            .is_err());
        assert_eq!(native.permits.available_permits(), 0);
        drop(session);
        assert!(keepalive.upgrade().is_some());
        *data.release.lock().unwrap() = true;
        data.changed.notify_all();
        until(|| native.permits.available_permits() == 1).await;
        assert_eq!(data.returned.load(Ordering::SeqCst), 1);
        until(|| keepalive.upgrade().is_none()).await;
        tokio::task::yield_now().await;
        assert!(
            !rendered.load(Ordering::SeqCst),
            "retired command rendered into a replacement UI"
        );
    }
}
