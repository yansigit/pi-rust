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

#[derive(Clone)]
struct Lease {
    bridge: Arc<ActionBridge>,
    session_persistence: bool,
    scope_lifetime: Option<Arc<rpi_agent::tool_execution_scope::BackgroundToolGuard>>,
}
tokio::task_local! { static SESSION_PERSISTENCE: bool; }

/// Host-internal authority of a captured session lease, never read from request JSON.
pub fn is_session_persistence() -> bool {
    SESSION_PERSISTENCE
        .try_with(|value| *value)
        .unwrap_or(false)
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
                // A retiring owner revokes authority immediately, but its worker
                // must still release the child's drain guard after cleanup.
                lease.scope_lifetime.is_none()
                    && lease
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

/// Acquire (0), release (1), retain existing session origins (2), or watch/drain a child runtime (3).
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
        if !matches!(operation, 0 | 2 | 3) {
            return std::ptr::null_mut();
        }
        let inherited = leases()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(handle as usize))
            .filter(|lease| lease.session_persistence || lease.scope_lifetime.is_some())
            .cloned();
        let Some(bridge) = inherited
            .as_ref()
            .map(|lease| lease.bridge.clone())
            .or_else(|| ActionBridge::native_invocation(handle as usize))
            .filter(|b| b.is_active())
        else {
            return std::ptr::null_mut();
        };
        if inherited.as_ref().is_some_and(|lease| {
            (operation == 2 && !lease.session_persistence)
                || (operation == 3 && lease.scope_lifetime.is_none())
        }) {
            return std::ptr::null_mut();
        }
        if matches!(operation, 2 | 3)
            && inherited.is_none()
            && (bridge.native_signal().is_cancelled()
                || bridge
                    .tool_scope
                    .as_ref()
                    .is_some_and(|scope| scope.stopped().is_cancelled()))
        {
            return std::ptr::null_mut();
        }
        let scope_lifetime = if let Some(lease) = &inherited {
            lease.scope_lifetime.clone()
        } else if operation == 3 {
            let Some(guard) = bridge
                .tool_scope
                .as_ref()
                .and_then(|scope| scope.retain_runtime())
            else {
                return std::ptr::null_mut();
            };
            Some(Arc::new(guard))
        } else {
            None
        };
        let mut leases = leases().lock().unwrap_or_else(|e| e.into_inner());
        if !bridge.is_active() || leases.len() >= 4096 {
            return std::ptr::null_mut();
        }
        let id = bridge.new_handle() as usize;
        leases.insert(
            id,
            Lease {
                bridge,
                session_persistence: operation == 2
                    || inherited
                        .as_ref()
                        .is_some_and(|lease| lease.session_persistence),
                scope_lifetime,
            },
        );
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
            if unsafe { control.as_ref() }
                .is_some_and(|control| (control.is_cancelled)(control.user_data) != 0)
            {
                return Err("native runtime request cancelled".into());
            }
            let args: Value = serde_json::from_str(unsafe { input.as_str() })
                .map_err(|_| "invalid native request")?;
            if args["version"] != 1 {
                return Err("unsupported native runtime version".into());
            }
            let grant_transport = args["port"] == "serviceLeases"
                && matches!(args["action"].as_str(), Some("call" | "release"));
            let retained = leases()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&(data as usize))
                .filter(|lease| lease.session_persistence || lease.scope_lifetime.is_some())
                .cloned();
            let bridge = retained
                .as_ref()
                .map(|lease| lease.bridge.clone())
                .or_else(|| {
                    if grant_transport {
                        ActionBridge::resolve_handle(data as usize)
                    } else {
                        ActionBridge::native_invocation(data as usize)
                    }
                })
                .ok_or("expired native runtime handle")?;
            if !bridge.is_active() {
                return Err("inactive native runtime".into());
            }
            let owner = bridge
                .native_owner()
                .ok_or("native runtime request has no plugin owner")?;
            let session_persistence = retained
                .as_ref()
                .is_some_and(|lease| lease.session_persistence);
            let scope_lifetime = retained
                .as_ref()
                .is_some_and(|lease| lease.scope_lifetime.is_some());
            if scope_lifetime {
                if args["port"] != "context"
                    || !matches!(args["action"].as_str(), Some("inspect" | "waitClosed"))
                {
                    return Err(
                        "scope lifetime lease only permits inspection and shutdown observation"
                            .into(),
                    );
                }
                if args["action"] == "waitClosed" {
                    let scope = bridge
                        .tool_scope
                        .as_ref()
                        .ok_or("child runtime is unavailable")?;
                    let closing = scope.runtime_closing();
                    loop {
                        if !bridge.is_active()
                            || unsafe { control.as_ref() }.is_some_and(|control| {
                                (control.is_cancelled)(control.user_data) != 0
                            })
                        {
                            return Err("native runtime watch cancelled".into());
                        }
                        if closing.is_cancelled() {
                            return Ok(Value::Null);
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
            if session_persistence
                && (args["port"] != "sessions"
                    || !args["origin"]
                        .as_str()
                        .is_some_and(|origin| !origin.is_empty())
                    || !matches!(
                        args["action"].as_str(),
                        Some(
                            "snapshot"
                                | "view"
                                | "location"
                                | "append"
                                | "journalAppend"
                                | "sync"
                                | "release"
                        )
                    ))
            {
                return Err(
                    "session persistence lease only permits captured-origin operations".into(),
                );
            }
            // Registration can run on a current-thread host runtime that cannot service queued work.
            if (matches!(args["port"].as_str(), Some("capabilities" | "context"))
                && args["action"] == "inspect")
                || (args["port"] == "context" && args["action"] == "bootstrap")
            {
                if unsafe { control.as_ref() }
                    .is_some_and(|control| (control.is_cancelled)(control.user_data) != 0)
                {
                    return Err("native runtime request cancelled".into());
                }
                if !scope_lifetime && bridge.native_signal().is_cancelled() {
                    return Err("native runtime request cancelled".into());
                }
                if args["port"] == "context" {
                    if args["action"] == "bootstrap" {
                        return bridge.host.native_bootstrap();
                    }
                    return Ok(json!({"owner":owner,"identity":bridge.operation,
                        "scopeId":bridge.tool_scope.as_ref().map(|scope|scope.runtime_id()),
                        "closing":bridge.tool_scope.as_ref().is_some_and(|scope|scope.runtime_closing().is_cancelled())}));
                }
                return Ok(json!([
                    "assistant.durationMs",
                    "json.exactNumbers",
                    "auth.publicErrors",
                    "auth.credentialStoreLock",
                    "providers.initialize",
                    "providers.awaitedPublication",
                    "providers.refreshIsolation",
                    "auth.providerRequest",
                    "auth.providerPolicy",
                    "auth.configuredValues",
                    "auth.dynamicMethods",
                    "auth.resolveOwned",
                    "context.inspect",
                    "context.bootstrap",
                    "context.snapshot",
                    "context.flags",
                    "context.abort",
                    "context.scopeLifetime",
                    "context.trustProvenance",
                    "events.lifecycleReasons",
                    "events.compactionDetails",
                    "events.selectionDetails",
                    "events.turnIndex",
                    "requests.liveBinding",
                    "events.promptContext",
                    "tools.provenance",
                    "tools.metadata",
                    "tools.activation",
                    "tools.exactActivation",
                    "tools.registrationPolicy",
                    "tools.selectionState",
                    "tools.boundedProgress",
                    "tools.hostBindings",
                    "providers.scopedCallbacks",
                    "providers.childCatalogs",
                    "sessions.location",
                    "sessions.view",
                    "sessions.persistenceLease",
                    "services.scopeLease.v1",
                    "commands.owned",
                    "ui.notifications",
                    "ui.status"
                ]));
            }
            if grant_transport && args["action"] == "release" {
                return bridge.services.release_scope_lease(&owner, &args);
            }
            if args["port"] == "serviceLeases" && args["action"] == "acquire" {
                return bridge.services.acquire_scope_lease(&bridge, &args);
            }
            let signal = if session_persistence || grant_transport {
                CancellationToken::new()
            } else {
                bridge.native_signal()
            };
            if signal.is_cancelled() {
                return Err("native runtime request cancelled".into());
            }
            let _cancel = signal.clone().drop_guard();
            let (sender, receiver) = mpsc::sync_channel(1);
            let host = bridge.host.clone();
            let work_signal = signal.clone();
            let services = bridge.services.clone();
            let dialogs = bridge.ui_dialog_mailbox();
            let status = bridge.extension_status_mailbox();
            let operation = bridge.operation.clone();
            let bridge_identity = operation.clone();
            let tool_scope = bridge.tool_scope.clone();
            bridge.runtime.spawn(async move {
                let work = async move {
                    if grant_transport {
                        services.scope_lease_request(owner, args, work_signal).await
                    } else if args["port"] == "services" {
                        services.request(owner, args, work_signal).await
                    } else if args["port"] == "ui" {
                        if work_signal.is_cancelled() {
                            return Err("native notification cancelled".into());
                        }
                        if args["action"] == "status" {
                            status.native_status(
                                owner.id,
                                rpi_agent::tool_execution_scope::current_scope(),
                                &args,
                            )
                        } else {
                            dialogs.notify(owner, &args)
                        }
                    } else if args["port"] == "dialogs" {
                        dialogs.request(owner, args, work_signal).await
                    } else {
                        let snapshot = args["port"] == "context" && args["action"] == "snapshot";
                        let mut result = host
                            .native_request(owner.clone(), args, work_signal)
                            .await?;
                        if snapshot {
                            if !result.is_object() {
                                return Err("invalid native context snapshot".into());
                            }
                            result["owner"] =
                                serde_json::to_value(owner).map_err(|_| "invalid native owner")?;
                            result["identity"] = serde_json::to_value(bridge_identity)
                                .map_err(|_| "invalid request identity")?;
                            result["hasUI"] = dialogs.is_attached().into();
                        }
                        Ok(result)
                    }
                };
                let result = SESSION_PERSISTENCE
                    .scope(
                        session_persistence,
                        rpi_agent::tool_execution_scope::inherit(tool_scope, async move {
                            match operation {
                                Some(identity) => rpi_ai::identity::scope(identity, work).await,
                                None => work.await,
                            }
                        }),
                    )
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
