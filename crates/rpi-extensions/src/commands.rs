//! Owned command callbacks run off the UI thread and retain their library through completion.
use crate::{private_callback, ActionBridge, ExtensionSession};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc, Weak};
use tokio_util::sync::CancellationToken;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    pub version: u32,
    pub name: String,
    pub description: String,
}
impl Manifest {
    pub(crate) fn parse(input: &str) -> Result<Self, String> {
        let mut value: Self =
            serde_json::from_str(input).map_err(|_| "invalid native command manifest")?;
        value.name = value.name.strip_prefix('/').unwrap_or(&value.name).into();
        if value.version != 1
            || value.name.is_empty()
            || value.name.len() > 128
            || value
                .name
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '/')
            || value.description.len() > 65536
        {
            return Err("invalid native command manifest".into());
        }
        Ok(value)
    }
}
#[derive(Clone)]
pub struct NativeCommand {
    pub(crate) callback: private_callback::Callback,
    pub(crate) bridge: Weak<ActionBridge>,
    pub(crate) stopping: CancellationToken,
    pub(crate) permits: Arc<tokio::sync::Semaphore>,
}
impl NativeCommand {
    pub fn start(
        &self,
        session: &ExtensionSession,
        name: &str,
        args: String,
        signal: CancellationToken,
    ) -> Result<CommandCall, String> {
        if args.len() > 1024 * 1024 {
            return Err("native command arguments exceed limit".into());
        }
        let bridge = self
            .bridge
            .upgrade()
            .filter(|bridge| bridge.is_active())
            .ok_or("native command owner expired")?;
        let snapshot = session
            .snapshot_arc()
            .filter(|snapshot| snapshot.is_active())
            .ok_or("native command session expired")?;
        let owner = bridge.native_owner().ok_or("native command has no owner")?;
        if session
            .action_bridge()
            .map_or(true, |b| b.generation() != owner.generation)
        {
            return Err("native command belongs to another session".into());
        }
        if self.stopping.is_cancelled() || signal.is_cancelled() {
            return Err("native command cancelled".into());
        }
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| "native command capacity exhausted")?;
        let callback = self.callback.clone();
        let keepalive = session.keepalive();
        let stopping = self.stopping.clone();
        let signal = signal.child_token();
        let work_signal = signal.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        let runtime = bridge.runtime.clone();
        let owner = Arc::downgrade(&bridge);
        let completion_stopping = stopping.clone();
        let input = json!({"version":1,"command":name,"args":args}).to_string();
        runtime.spawn(async move {
            let _permit = permit;
            let _snapshot = snapshot;
            let _cancel = work_signal.clone().drop_guard();
            let scope = bridge.for_command(work_signal.clone());
            let work = private_callback::invoke_scoped(
                callback,
                keepalive,
                input,
                work_signal.clone(),
                true,
                Some(scope),
            );
            tokio::pin!(work);
            let result = tokio::select! {
                biased;
                _ = stopping.cancelled() => { work_signal.cancel(); work.await },
                value = &mut work => value,
            };
            let result = result
                .map_err(|_| "native command callback failed".to_owned())
                .and_then(|response| {
                    if response["version"] != 1 {
                        return Err("unsupported native command response".into());
                    }
                    if let Some(error) = response["error"].as_str() {
                        return Err(error.into());
                    }
                    response
                        .get("result")
                        .cloned()
                        .ok_or_else(|| "native command omitted its result".into())
                });
            let _ = send.send(result);
        });
        Ok(CommandCall {
            receive,
            signal,
            runtime,
            owner,
            stopping: completion_stopping,
        })
    }
}
/// Dropping the caller cancels waiting work; admitted callbacks still own their capacity and library.
pub struct CommandCall {
    receive: tokio::sync::oneshot::Receiver<Result<Value, String>>,
    signal: CancellationToken,
    runtime: tokio::runtime::Handle,
    owner: Weak<ActionBridge>,
    stopping: CancellationToken,
}
impl Drop for CommandCall {
    fn drop(&mut self) {
        self.signal.cancel();
    }
}
impl CommandCall {
    pub async fn wait(mut self) -> Result<Value, String> {
        (&mut self.receive)
            .await
            .map_err(|_| "native command task stopped".to_owned())?
    }
    pub fn complete(self, finish: impl FnOnce(Result<Value, String>) + Send + 'static) {
        let owner = self.owner.clone();
        let stopping = self.stopping.clone();
        self.runtime.clone().spawn(async move {
            let result = self.wait().await;
            if !stopping.is_cancelled() && owner.upgrade().is_some_and(|owner| owner.is_active()) {
                finish(result);
            }
        });
    }
}

pub(crate) extern "C" fn legacy_unavailable(
    _: rpi_plugin_sdk::StbStringRef,
    _: *mut rpi_plugin_sdk::StbString,
    _: *mut std::ffi::c_void,
) -> i32 {
    -1
}
