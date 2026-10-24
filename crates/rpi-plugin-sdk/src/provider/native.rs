//! Private versioned host ports. Missing capabilities are explicit errors.
use crate::{callbacks::OwnedString, AuthRequestControl, PluginApi, StbString, StbStringRef};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Client {
    api: PluginApi,
    transport: PluginApi,
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
            transport: *api,
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
            transport: self.transport,
            context: Some(std::sync::Arc::new(ContextLease {
                handle: handle as usize,
                context,
            })),
        })
    }
    pub async fn request(
        &self,
        request: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        let client = if self.api.native_context.is_some() {
            self.scoped()?
        } else {
            self.clone()
        };
        client.raw_request(request, signal).await
    }
    /// Acquire on a live provider callback (use `scoped()` before leaving its thread).
    /// A lease extends time only; it cannot select a different execution lane.
    pub async fn scope_service_lease(
        &self,
        service: &str,
        service_version: u32,
        methods: &[&str],
        signal: CancellationToken,
    ) -> Result<ScopeServiceLease, String> {
        self.require_capabilities(&["services.scopeLease.v1"], signal.clone())
            .await?;
        let result = self
            .request(
                serde_json::json!({"port":"serviceLeases","action":"acquire",
            "service":service,"serviceVersion":service_version,"methods":methods,
            "capability":"services.scopeLease.v1"}),
                signal,
            )
            .await?;
        let lease = result["lease"]
            .as_str()
            .ok_or("invalid scope service lease response")?
            .to_owned();
        Ok(ScopeServiceLease(std::sync::Arc::new(
            ScopeServiceLeaseInner {
                client: Self {
                    api: self.transport,
                    transport: self.transport,
                    context: None,
                },
                lease,
                released: std::sync::atomic::AtomicBool::new(false),
            },
        )))
    }
    // Deliberately does not call scoped(): grant calls use their stored host authority.
    async fn raw_request(
        &self,
        request: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        let client = self.clone();
        let signal = signal.child_token();
        let _cancel = signal.clone().drop_guard();
        tokio::task::spawn_blocking(move || client.execute(request, signal))
            .await
            .map_err(|_| "native host request task stopped".to_owned())?
    }
    fn execute(&self, mut request: Value, signal: CancellationToken) -> Result<Value, String> {
        request
            .as_object_mut()
            .ok_or("native request must be an object")?
            .insert("version".into(), 1.into());
        let api = &self.api;
        let _context = &self.context;
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
    }
}

/// Cloneable narrow service authority. The final clone releases best-effort.
#[derive(Clone)]
pub struct ScopeServiceLease(std::sync::Arc<ScopeServiceLeaseInner>);
struct ScopeServiceLeaseInner {
    client: Client,
    lease: String,
    released: std::sync::atomic::AtomicBool,
}
impl ScopeServiceLease {
    pub async fn call(
        &self,
        method: &str,
        input: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        if self.0.released.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("scope service lease revoked".into());
        }
        self.0
            .client
            .raw_request(
                serde_json::json!({"port":"serviceLeases","action":"call",
            "lease":self.0.lease,"method":method,"input":input}),
                signal,
            )
            .await
    }
    pub async fn release(&self) -> Result<(), String> {
        if self
            .0
            .released
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(());
        }
        self.0
            .client
            .raw_request(
                serde_json::json!({"port":"serviceLeases","action":"release",
            "lease":self.0.lease}),
                CancellationToken::new(),
            )
            .await
            .map(|_| ())
    }
}
impl Drop for ScopeServiceLeaseInner {
    fn drop(&mut self) {
        if self
            .released
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let request =
            serde_json::json!({"port":"serviceLeases","action":"release","lease":self.lease});
        // Release is a synchronous host registry operation; no worker may outlive the library.
        let _ = self.client.execute(request, CancellationToken::new());
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
