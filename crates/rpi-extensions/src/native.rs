//! Private native runtime requests. Ownership is supplied by the host bridge,
//! never by a plugin-provided provider/model descriptor or JSON owner field.
use crate::ActionBridge;
use rpi_plugin_sdk::{AuthRequestControl, StbString, StbStringRef};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    ffi::c_void,
    sync::{mpsc, Arc, Mutex, OnceLock},
    time::Duration,
};
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

struct Lease {
    bridge: Arc<ActionBridge>,
}
static LEASES: OnceLock<Mutex<HashMap<usize, Lease>>> = OnceLock::new();
fn leases() -> &'static Mutex<HashMap<usize, Lease>> {
    LEASES.get_or_init(Mutex::default)
}

pub(crate) fn retire_owner(owner: u64) {
    let removed = {
        let mut leases = leases().lock().unwrap_or_else(|e| e.into_inner());
        let ids: Vec<_> = leases
            .iter()
            .filter(|(_, lease)| {
                lease
                    .bridge
                    .native_owner()
                    .is_some_and(|known| known.id == owner)
            })
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .filter_map(|id| {
                ActionBridge::remove_handle(id);
                leases.remove(&id)
            })
            .collect::<Vec<_>>()
    };
    drop(removed);
}

/// Acquire (0) or release (1) an opaque runtime context, never a raw bridge pointer.
pub(crate) extern "C" fn context(operation: u32, handle: *mut c_void) -> *mut c_void {
    rpi_plugin_sdk::guard_or(std::ptr::null_mut(), || {
        if operation == 1 {
            let removed = {
                let mut leases = leases().lock().unwrap_or_else(|e| e.into_inner());
                let removed = leases.remove(&(handle as usize));
                if removed.is_some() {
                    ActionBridge::remove_handle(handle as usize);
                }
                removed
            };
            drop(removed);
            return std::ptr::null_mut();
        }
        if operation != 0 {
            return std::ptr::null_mut();
        }
        let Some(bridge) =
            ActionBridge::native_invocation(handle as usize).filter(|b| b.is_active())
        else {
            return std::ptr::null_mut();
        };
        let mut leases = leases().lock().unwrap_or_else(|e| e.into_inner());
        if !bridge.is_active() || leases.len() >= 4096 {
            return std::ptr::null_mut();
        }
        let id = bridge.new_handle() as usize;
        leases.insert(id, Lease { bridge });
        id as *mut c_void
    })
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
            // Registration can run on a current-thread host runtime that cannot service queued work.
            if args["port"] == "capabilities" && args["action"] == "inspect" {
                if unsafe { control.as_ref() }
                    .is_some_and(|control| (control.is_cancelled)(control.user_data) != 0)
                {
                    return Err("native runtime request cancelled".into());
                }
                return Ok(json!([
                    "assistant.durationMs",
                    "json.exactNumbers",
                    "auth.publicErrors",
                    "auth.credentialStoreLock",
                    "providers.initialize",
                    "auth.providerRequest",
                    "auth.dynamicMethods"
                ]));
            }
            let signal = bridge.native_signal();
            let _cancel = signal.clone().drop_guard();
            let (sender, receiver) = mpsc::sync_channel(1);
            let host = bridge.host.clone();
            let work_signal = signal.clone();
            let services = bridge.services.clone();
            let dialogs = bridge.ui_dialog_mailbox();
            let operation = bridge.operation.clone();
            let tool_scope = bridge.tool_scope.clone();
            bridge.runtime.spawn(async move {
                let work = async move {
                    if args["port"] == "services" {
                        services.request(owner, args, work_signal).await
                    } else if args["port"] == "dialogs" {
                        dialogs.request(owner, args, work_signal).await
                    } else {
                        host.native_request(owner, args, work_signal).await
                    }
                };
                let result = rpi_agent::tool_execution_scope::inherit(tool_scope, async move {
                    match operation {
                        Some(identity) => rpi_ai::identity::scope(identity, work).await,
                        None => work.await,
                    }
                })
                .await;
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
