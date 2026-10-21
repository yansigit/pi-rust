//! Private versioned host ports. Missing capabilities are explicit errors.
use crate::{callbacks::OwnedString, AuthRequestControl, PluginApi, StbString, StbStringRef};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Client {
    api: PluginApi,
    context: Option<std::sync::Arc<ContextLease>>,
}
impl Client {
    /// Fail explicitly when an older host would silently discard required metadata.
    pub async fn require_capabilities(
        &self,
        required: &[&str],
        signal: CancellationToken,
    ) -> Result<(), String> {
        let response = self
            .request(
                serde_json::json!({"port":"capabilities","action":"inspect"}),
                signal,
            )
            .await
            .map_err(|error| format!("native capability discovery failed; upgrade rpi: {error}"))?;
        let available = response
            .as_array()
            .ok_or("invalid native capability response")?;
        let missing: Vec<_> = required
            .iter()
            .filter(|name| !available.iter().any(|value| value.as_str() == Some(**name)))
            .copied()
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "host is missing required native capabilities: {}",
                missing.join(", ")
            ))
        }
    }
    pub fn new(api: &PluginApi) -> Result<Self, String> {
        if api.native_request.is_none() {
            return Err("native runtime ports require a newer rpi host".into());
        }
        Ok(Self {
            api: *api,
            context: None,
        })
    }
    /// Call on the foreign callback thread before moving work to a worker/runtime.
    pub fn scoped(&self) -> Result<Self, String> {
        self.capture(0)
    }
    /// Retain only captured session origins through caller cancellation. Negotiate
    /// `sessions.persistenceLease` first; other ports and new captures are refused.
    pub fn session_persistence(&self) -> Result<Self, String> {
        self.capture(2)
    }
    /// Watch a child runtime after this callback returns. Negotiate
    /// `context.scopeLifetime`; only context.inspect/waitClosed are permitted.
    /// Drop after worker cleanup so the child host's idle barrier can complete.
    pub fn scope_lifetime(&self) -> Result<Self, String> {
        self.capture(3)
    }
    fn capture(&self, operation: u32) -> Result<Self, String> {
        let context = self
            .api
            .native_context
            .ok_or("native invocation contexts require a newer rpi host")?;
        let handle = context(operation, self.api.user_data);
        if handle.is_null() {
            return Err("native invocation context expired or exhausted".into());
        }
        let mut api = self.api;
        api.user_data = handle;
        Ok(Self {
            api,
            context: Some(std::sync::Arc::new(ContextLease {
                handle: handle as usize,
                context,
            })),
        })
    }
    pub async fn request(
        &self,
        mut request: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        let object = request
            .as_object_mut()
            .ok_or("native request must be an object")?;
        object.insert("version".into(), 1.into());
        let client = if self.api.native_context.is_some() {
            self.scoped()?
        } else {
            self.clone()
        };
        let signal = signal.child_token();
        let _cancel = signal.clone().drop_guard();
        tokio::task::spawn_blocking(move || {
            let api = &client.api;
            let _context = &client.context;
            let control = AuthRequestControl {
                struct_size: std::mem::size_of::<AuthRequestControl>() as u32,
                is_cancelled,
                user_data: (&signal as *const CancellationToken).cast_mut().cast(),
            };
            let mut output = StbString::empty();
            let status = (api.native_request.unwrap())(
                StbStringRef::from_str(&request.to_string()),
                &control,
                &mut output,
                api.user_data,
            );
            // Host ports invert the usual provider ownership of returned strings.
            let output = unsafe { OwnedString::from_raw(output, Some(api.free_string)) };
            let value: Value = serde_json::from_str(&output.to_string_lossy())
                .map_err(|_| "invalid native host response")?;
            if value["version"] != 1 {
                return Err("unsupported native host response".into());
            }
            if status != 0 {
                return Err(value["error"]
                    .as_str()
                    .unwrap_or("native host request failed")
                    .into());
            }
            value
                .get("result")
                .cloned()
                .ok_or_else(|| "native host response omitted its result".into())
        })
        .await
        .map_err(|_| "native host request task stopped".to_owned())?
    }
}
extern "C" fn is_cancelled(data: *mut std::ffi::c_void) -> u8 {
    u8::from(unsafe { &*data.cast::<CancellationToken>() }.is_cancelled())
}

struct ContextLease {
    handle: usize,
    context: extern "C" fn(u32, *mut std::ffi::c_void) -> *mut std::ffi::c_void,
}
impl Drop for ContextLease {
    fn drop(&mut self) {
        (self.context)(1, self.handle as *mut std::ffi::c_void);
    }
}
