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

pub(crate) async fn invoke(
    callback: Callback,
    keepalive: Arc<crate::PluginKeepalive>,
    request: String,
    signal: CancellationToken,
    drain: bool,
) -> Result<serde_json::Value, AuthError> {
    let signal = signal.child_token();
    let _cancel_callback = signal.clone().drop_guard();
    let callback_signal = signal.clone();
    let pending = tokio::task::spawn_blocking(move || {
        let _keepalive = keepalive;
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
            _ = signal.cancelled() => Err(AuthError::Cancelled),
            result = pending => result.map_err(|_| AuthError::Task)?,
        }
    }
}

extern "C" fn is_cancelled(context: *mut c_void) -> u8 {
    // SAFETY: the callback borrows this token until it returns.
    u8::from(unsafe { &*context.cast::<CancellationToken>() }.is_cancelled())
}
