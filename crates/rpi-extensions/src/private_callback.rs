//! Private, cancellable callbacks shared by auth and account credential stores.
use rpi_ai::auth::AuthError;
use rpi_plugin_sdk::{AuthRequestControl, AuthRequestFn, FreeStringFn, StbString, StbStringRef};
use std::{ffi::c_void, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(crate) struct Callback {
    pub request: AuthRequestFn,
    pub free: FreeStringFn,
    pub context: *mut c_void,
}

// SAFETY: native registration requires thread-safe callbacks and library-owned contexts.
unsafe impl Send for Callback {}
unsafe impl Sync for Callback {}

impl Callback {
    fn invoke(&self, request: &str, control: &AuthRequestControl, output: &mut StbString) -> i32 {
        (self.request)(
            StbStringRef::from_str(request),
            control,
            output,
            self.context,
        )
    }
}

pub(crate) fn capture_invocation(
    bridge: Option<&std::sync::Weak<crate::ActionBridge>>,
    signal: &CancellationToken,
) -> Result<Option<Arc<crate::ActionBridge>>, AuthError> {
    bridge
        .map(|bridge| {
            let bridge = bridge.upgrade().ok_or(AuthError::Cancelled)?;
            if !bridge.is_active() {
                return Err(AuthError::Cancelled);
            }
            Ok(bridge.for_invocation(
                rpi_agent::tool_execution_scope::is_child_execution(),
                signal.clone(),
            ))
        })
        .transpose()
}

pub(crate) async fn invoke(
    callback: Callback,
    keepalive: Arc<crate::PluginKeepalive>,
    request: String,
    signal: CancellationToken,
    drain: bool,
) -> Result<serde_json::Value, AuthError> {
    invoke_scoped(callback, keepalive, request, signal, drain, None).await
}

pub(crate) async fn invoke_scoped(
    callback: Callback,
    keepalive: Arc<crate::PluginKeepalive>,
    request: String,
    signal: CancellationToken,
    drain: bool,
    bridge: Option<Arc<crate::ActionBridge>>,
) -> Result<serde_json::Value, AuthError> {
    invoke_scoped_guarded(callback, keepalive, request, signal, drain, bridge, None).await
}

/// Retain admission until foreign execution and output deallocation finish,
/// even when the async caller stops waiting for the blocking callback.
pub(crate) async fn invoke_scoped_guarded(
    callback: Callback,
    keepalive: Arc<crate::PluginKeepalive>,
    request: String,
    signal: CancellationToken,
    drain: bool,
    bridge: Option<Arc<crate::ActionBridge>>,
    execution_guard: Option<Box<dyn Send>>,
) -> Result<serde_json::Value, AuthError> {
    let background = bridge
        .as_ref()
        .and_then(|bridge| bridge.tool_scope.as_ref())
        .map(|scope| scope.track_background())
        .or_else(rpi_agent::tool_execution_scope::track_background_tool);
    let signal = signal.child_token();
    let _cancel_callback = signal.clone().drop_guard();
    let bridge = bridge.map(|bridge| bridge.with_callback_signal(signal.clone()));
    let callback_signal = signal.clone();
    let mut pending = tokio::task::spawn_blocking(move || {
        let _execution_guard = execution_guard;
        let _keepalive = keepalive;
        let _background = background;
        let _invocation = bridge.as_ref().map(|bridge| bridge.enter_invocation());
        if callback_signal.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let control = AuthRequestControl {
            struct_size: std::mem::size_of::<AuthRequestControl>() as u32,
            is_cancelled,
            user_data: (&callback_signal as *const CancellationToken)
                .cast_mut()
                .cast(),
        };
        let mut output = StbString::empty();
        let status = callback.invoke(&request, &control, &mut output);
        // SAFETY: the callback transfers output to its registered deallocator.
        let output = unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(output, Some(callback.free))
        };
        if status != 0 {
            return Err(AuthError::Resolve);
        }
        serde_json::from_str(&output.to_string_lossy()).map_err(|_| AuthError::Resolve)
    });
    if drain {
        // Lock acquisition must consume a late success so its transaction can
        // be released, even when the requesting caller has stopped waiting.
        pending.await.map_err(|_| AuthError::Task)?
    } else {
        tokio::select! {
            biased;
            _ = signal.cancelled() => {
                pending.abort(); // A queued foreign call must not begin after cancellation.
                Err(AuthError::Cancelled)
            },
            result = &mut pending => result.map_err(|_| AuthError::Task)?,
        }
    }
}

extern "C" fn is_cancelled(context: *mut c_void) -> u8 {
    // SAFETY: the callback borrows this token until it returns.
    u8::from(unsafe { &*context.cast::<CancellationToken>() }.is_cancelled())
}
