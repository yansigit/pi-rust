//! Private native runtime requests. Ownership is supplied by the host bridge,
//! never by a plugin-provided provider/model descriptor or JSON owner field.
use crate::ActionBridge;
use rpi_plugin_sdk::{AuthRequestControl, StbString, StbStringRef};
use serde_json::{json, Value};
use std::{ffi::c_void, sync::mpsc, time::Duration};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeOwner {
    pub generation: String,
    pub id: u64,
    pub plugin: String,
    pub child: bool,
}

#[async_trait::async_trait]
pub trait NativeProviderRuntime: Send + Sync {
    async fn request(
        &self,
        owner: NativeOwner,
        args: Value,
        signal: CancellationToken,
    ) -> Result<Value, String>;
}

pub(crate) extern "C" fn request(
    input: StbStringRef,
    control: *const AuthRequestControl,
    out: *mut StbString,
    data: *mut c_void,
) -> i32 {
    rpi_plugin_sdk::guard_or(1, || {
        if out.is_null() {
            return 2;
        }
        let result = (|| {
            if !control.is_null()
                && unsafe { control.cast::<u32>().read() }
                    < std::mem::size_of::<AuthRequestControl>() as u32
            {
                return Err("short native cancellation control".into());
            }
            let bridge = ActionBridge::native_invocation(data as usize)
                .ok_or("expired native runtime handle")?;
            if !bridge.is_active() {
                return Err("inactive native runtime".into());
            }
            let owner = bridge
                .native_owner()
                .ok_or("native runtime request has no plugin owner")?;
            let args: Value = serde_json::from_str(unsafe { input.as_str() })
                .map_err(|_| "invalid native request")?;
            if args["version"] != 1 {
                return Err("unsupported native runtime version".into());
            }
            let signal = bridge.native_signal();
            let _cancel = signal.clone().drop_guard();
            let (sender, receiver) = mpsc::sync_channel(1);
            let host = bridge.host.clone();
            let work_signal = signal.clone();
            bridge.runtime.spawn(async move {
                let result = host.native_request(owner, args, work_signal).await;
                let _ = sender.send(result);
            });
            loop {
                let cancelled = unsafe { control.as_ref() }
                    .is_some_and(|control| (control.is_cancelled)(control.user_data) != 0);
                if cancelled || signal.is_cancelled() || !bridge.is_active() {
                    return Err("native runtime request cancelled".into());
                }
                match receiver.recv_timeout(Duration::from_millis(10)) {
                    Ok(result) => return result,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => return Err("native runtime task stopped".into()),
                }
            }
        })();
        let (status, response) = match result {
            Ok(value) => (0, json!({"version":1,"result":value})),
            Err(error) => (1, json!({"version":1,"error":error})),
        };
        unsafe {
            *out = StbString::from_string(response.to_string());
        }
        status
    })
}
