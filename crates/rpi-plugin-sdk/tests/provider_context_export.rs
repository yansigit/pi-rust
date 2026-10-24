#![cfg(feature = "provider")]
use rpi_ai::{Api, AssistantMessage, Context, Model, Provider, SimpleStreamOptions};
use rpi_plugin_sdk as sdk;
use std::{cell::RefCell, ffi::c_void, ptr, sync::Arc};

fn factory(
    _: &sdk::provider::ProviderContext,
    _: &Model,
    options: &SimpleStreamOptions,
) -> Result<Arc<dyn Provider>, String> {
    if options.api_key.as_deref() == Some("fail") {
        return Err("client build failed".into());
    }
    Ok(rpi_ai::providers::faux::FauxProvider::new(
        rpi_ai::providers::faux::FauxScript::new()
            .with_text(options.api_key.as_deref().unwrap_or("anonymous")),
    ))
}
sdk::export_provider_with_context!(
    "context-test",
    "http://localhost",
    "faux",
    serde_json::json!({"id":"context-test"}),
    factory
);

type Hooks = (
    sdk::PluginBeginShutdownFn,
    sdk::PluginFinishShutdownFn,
    usize,
);
type Registered = (sdk::ProviderStreamRequestFn, sdk::FreeStringFn, usize);
thread_local! {
    static HOOKS: RefCell<Option<Hooks>> = const { RefCell::new(None) };
    static REGISTERED: RefCell<Option<Registered>> = const { RefCell::new(None) };
}
extern "C" fn lifecycle(
    begin: sdk::PluginBeginShutdownFn,
    finish: sdk::PluginFinishShutdownFn,
    data: *mut c_void,
) -> i32 {
    HOOKS.with(|slot| *slot.borrow_mut() = Some((begin, finish, data as usize)));
    0
}
extern "C" fn register(
    _: sdk::StbStringRef,
    _: sdk::StbStringRef,
    _: sdk::StbStringRef,
    request: sdk::ProviderStreamRequestFn,
    free: sdk::FreeStringFn,
    data: *mut c_void,
) -> i32 {
    REGISTERED.with(|slot| *slot.borrow_mut() = Some((request, free, data as usize)));
    0
}
extern "C" fn runtime_action(
    _: u32,
    _: sdk::StbStringRef,
    _: *mut sdk::StbString,
    _: *mut c_void,
) -> i32 {
    1
}
fn api() -> sdk::PluginApi {
    sdk::PluginApi {
        abi_version: sdk::RPI_PLUGIN_ABI_VERSION_UNIFIED,
        struct_size: std::mem::size_of::<sdk::PluginApi>() as u32,
        free_string: sdk::callbacks::free_string,
        register_tool: None,
        register_command: None,
        register_shortcut: None,
        register_flag: None,
        register_provider: None,
        register_message_renderer: None,
        register_markdown_transformer: None,
        register_entry_renderer: None,
        register_event_handler: None,
        register_resources_discover: None,
        runtime_action,
        dispatch_event: None,
        user_data: ptr::null_mut(),
        declare: None,
        register_before_agent_start: None,
        register_oauth_provider: None,
        register_streaming_provider: None,
        register_tool_with_wake: None,
        register_plugin_lifecycle: None,
        register_provider_auth: None,
        register_credential_store: None,
        register_provider_wrapper: None,
        delegate_provider_stream: None,
        provider_continuation: None,
        native_request: None,
        register_provider_runtime: None,
        register_event_result_handler: None,
        register_service: None,
        native_context: None,
        register_native_command: None,
        register_owned_tool_with_wake: None,
    }
}

extern "C" fn old_native(
    input: sdk::StbStringRef,
    _: *const sdk::AuthRequestControl,
    out: *mut sdk::StbString,
    _: *mut c_void,
) -> i32 {
    let value: serde_json::Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
    assert_eq!(
        value["port"], "capabilities",
        "old host must not receive acquire"
    );
    unsafe {
        *out = sdk::StbString::from_string("{\"version\":1,\"result\":[]}".into());
    }
    0
}
#[tokio::test]
async fn scope_service_lease_old_host_fails_closed() {
    let mut api = api();
    api.native_request = Some(old_native);
    let client = sdk::provider::native::Client::new(&api).unwrap();
    let result = client
        .scope_service_lease("pi-fabric", 1, &["compact"], Default::default())
        .await;
    assert!(
        matches!(result, Err(error) if error == "host is missing required native capabilities: services.scopeLease.v1")
    );
}

extern "C" fn emit(raw: sdk::StbString, data: *mut c_void) -> i32 {
    let _owned =
        unsafe { sdk::callbacks::OwnedString::from_raw(raw, Some(sdk::callbacks::free_string)) };
    assert_eq!(std::thread::current().id(), unsafe {
        *data.cast::<std::thread::ThreadId>()
    });
    0
}
fn call(registration: Registered, key: &str) -> AssistantMessage {
    let (request, free, data) = registration;
    let model = Model::new("m", "M", Api::Faux, "context-test", "http://localhost");
    let json =
        serde_json::json!({"model":model,"context":Context::new(vec![]),"options":{"apiKey":key}})
            .to_string();
    let mut out = sdk::StbString::empty();
    let mut thread_id = std::thread::current().id();
    assert_eq!(
        request(
            sdk::StbStringRef::from_str(&json),
            Some(emit),
            (&mut thread_id as *mut std::thread::ThreadId).cast(),
            ptr::null(),
            None,
            ptr::null_mut(),
            &mut out,
            data as *mut c_void
        ),
        0
    );
    let owned = unsafe { sdk::callbacks::OwnedString::from_raw(out, Some(free)) };
    serde_json::from_str(&owned.to_string_lossy()).unwrap()
}
#[test]
fn exported_context_handles_concurrent_credentials_errors_shutdown_and_old_hosts() {
    for has_lifecycle in [false, true] {
        let mut api = api();
        api.register_streaming_provider = Some(register);
        api.register_plugin_lifecycle = has_lifecycle.then_some(lifecycle);
        assert_eq!(rpi_plugin_register(&api), 0);
        let registration = REGISTERED.with(|slot| slot.borrow_mut().take().unwrap());
        assert_eq!(registration.2 != 0, has_lifecycle);
        let workers: Vec<_> = ["first-secret", "second-secret"].into_iter().map(|key| std::thread::spawn(move || {
            let message = call(registration, key);
            assert!(matches!(message.content.as_slice(), [rpi_ai::Content::Text(text)] if text.text == key));
        })).collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let failed = call(registration, "fail");
        assert_eq!(failed.stop_reason, rpi_ai::StopReason::Error);
        assert_eq!(failed.error_message.as_deref(), Some("client build failed"));
        if has_lifecycle {
            let (begin, finish, data) = HOOKS.with(|slot| slot.borrow_mut().take().unwrap());
            begin(data as *mut c_void);
            begin(data as *mut c_void);
            let rejected = call(registration, "after-shutdown");
            assert_eq!(rejected.stop_reason, rpi_ai::StopReason::Error);
            assert_eq!(finish(data as *mut c_void), 0);
        }
    }
}

extern "C" fn register_wrapper(
    _: sdk::StbStringRef,
    request: sdk::ProviderStreamRequestFn,
    free: sdk::FreeStringFn,
    data: *mut c_void,
) -> i32 {
    REGISTERED.with(|slot| *slot.borrow_mut() = Some((request, free, data as usize)));
    0
}
extern "C" fn unused_delegate(
    _: sdk::StbStringRef,
    _: Option<sdk::ProviderStreamEmitFn>,
    _: *mut c_void,
    _: *const sdk::ProviderStreamControl,
    _: Option<sdk::ProviderStreamCompletionFn>,
    _: *mut c_void,
    _: *mut sdk::StbString,
    _: *mut c_void,
) -> i32 {
    1
}
extern "C" fn unused_continuation(
    _: sdk::StbStringRef,
    _: *const sdk::AuthRequestControl,
    _: *mut sdk::StbString,
    _: *mut c_void,
) -> i32 {
    1
}

#[test]
fn captured_wrapper_state_is_isolated_and_retained_until_shutdown() {
    let mut api = api();
    api.register_provider_wrapper = Some(register_wrapper);
    api.register_plugin_lifecycle = Some(lifecycle);
    api.delegate_provider_stream = Some(unused_delegate);
    api.provider_continuation = Some(unused_continuation);
    let mut registrations = Vec::new();
    for label in ["first session", "second session"] {
        let state = Arc::new(label.to_string());
        let weak = Arc::downgrade(&state);
        sdk::provider::composition::register_with_factory(
            &api,
            serde_json::json!({"version":1,"id":label}),
            move |_, _, _, _| {
                Ok(rpi_ai::providers::faux::FauxProvider::new(
                    rpi_ai::providers::faux::FauxScript::new().with_text(state.as_str()),
                ))
            },
        )
        .unwrap();
        registrations.push((
            REGISTERED.with(|slot| slot.borrow_mut().take().unwrap()),
            HOOKS.with(|slot| slot.borrow_mut().take().unwrap()),
            weak,
        ));
    }
    for (registration, hooks, state) in registrations {
        let (request, free, data) = registration;
        let model = Model::new("m", "M", Api::Faux, "context-test", "http://localhost");
        let json = serde_json::json!({"model":model,"context":Context::new(vec![]),"options":{},"__rpiContinuation":{"version":1,"id":"owned","generation":"session"}}).to_string();
        let mut out = sdk::StbString::empty();
        assert_eq!(
            request(
                sdk::StbStringRef::from_str(&json),
                None,
                ptr::null_mut(),
                ptr::null(),
                None,
                ptr::null_mut(),
                &mut out,
                data as *mut c_void
            ),
            0
        );
        let output = unsafe { sdk::callbacks::OwnedString::from_raw(out, Some(free)) };
        let message: AssistantMessage = serde_json::from_str(&output.to_string_lossy()).unwrap();
        assert!(
            matches!(message.content.as_slice(), [rpi_ai::Content::Text(text)] if text.text == *state.upgrade().unwrap())
        );
        hooks.0(hooks.2 as *mut c_void);
        assert!(state.upgrade().is_some());
        assert_eq!(hooks.1(hooks.2 as *mut c_void), 0);
        assert!(state.upgrade().is_none());
    }
}

// A handle registry models host invalidation/destruction without ever dereferencing
// an expired user_data pointer. Counts observe the synchronous Drop wire request.
#[derive(Default)]
struct LeaseHost {
    releases: std::sync::atomic::AtomicUsize,
    calls: std::sync::atomic::AtomicUsize,
    active: std::sync::atomic::AtomicBool,
}
fn lease_hosts() -> &'static std::sync::Mutex<std::collections::HashMap<usize, Arc<LeaseHost>>> {
    static HOSTS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<usize, Arc<LeaseHost>>>,
    > = std::sync::OnceLock::new();
    HOSTS.get_or_init(Default::default)
}
extern "C" fn lease_native(
    input: sdk::StbStringRef,
    _: *const sdk::AuthRequestControl,
    out: *mut sdk::StbString,
    data: *mut c_void,
) -> i32 {
    use std::sync::atomic::Ordering::SeqCst;
    let request: serde_json::Value = serde_json::from_str(unsafe { input.as_str() }).unwrap();
    let host = lease_hosts().lock().unwrap().get(&(data as usize)).cloned();
    let (status, value) = match host {
        None => (
            1,
            serde_json::json!({"version":1,"error":"expired native invocation context"}),
        ),
        Some(host) => {
            if request["action"] == "release" {
                host.releases.fetch_add(1, SeqCst);
            }
            if !host.active.load(SeqCst) {
                (
                    1,
                    serde_json::json!({"version":1,"error":"scope service lease revoked"}),
                )
            } else {
                let result = match request["port"].as_str().unwrap() {
                    "capabilities" => serde_json::json!(["services.scopeLease.v1"]),
                    _ if request["action"] == "acquire" => {
                        serde_json::json!({"lease":"test-grant"})
                    }
                    _ if request["action"] == "call" => {
                        host.calls.fetch_add(1, SeqCst);
                        serde_json::json!("ok")
                    }
                    _ => serde_json::Value::Null,
                };
                (0, serde_json::json!({"version":1,"result":result}))
            }
        }
    };
    unsafe {
        *out = sdk::StbString::from_string(value.to_string());
    }
    status
}
struct LeaseFixture {
    id: usize,
    host: Arc<LeaseHost>,
}
impl LeaseFixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let host = Arc::new(LeaseHost::default());
        host.active.store(true, std::sync::atomic::Ordering::SeqCst);
        lease_hosts().lock().unwrap().insert(id, host.clone());
        Self { id, host }
    }
    async fn acquire(&self) -> sdk::provider::native::ScopeServiceLease {
        let mut api = api();
        api.native_request = Some(lease_native);
        api.user_data = self.id as *mut c_void;
        sdk::provider::native::Client::new(&api)
            .unwrap()
            .scope_service_lease("service", 1, &["method"], Default::default())
            .await
            .unwrap()
    }
    fn releases(&self) -> usize {
        self.host.releases.load(std::sync::atomic::Ordering::SeqCst)
    }
}
impl Drop for LeaseFixture {
    fn drop(&mut self) {
        lease_hosts().lock().unwrap().remove(&self.id);
    }
}
#[tokio::test]
async fn scope_service_lease_only_final_clone_releases() {
    let host = LeaseFixture::new();
    let lease = host.acquire().await;
    let clone = lease.clone();
    drop(lease);
    assert_eq!(host.releases(), 0);
    assert_eq!(
        clone
            .call("method", serde_json::json!({}), Default::default())
            .await
            .unwrap(),
        "ok"
    );
    drop(clone);
    assert_eq!(host.releases(), 1);
}
#[tokio::test]
async fn scope_service_lease_explicit_release_is_shared_and_idempotent() {
    let host = LeaseFixture::new();
    let lease = host.acquire().await;
    let clone = lease.clone();
    lease.release().await.unwrap();
    clone.release().await.unwrap();
    assert_eq!(
        clone
            .call("method", serde_json::json!({}), Default::default())
            .await
            .unwrap_err(),
        "scope service lease revoked"
    );
    assert_eq!(host.host.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    drop(lease);
    drop(clone);
    assert_eq!(host.releases(), 1);
}
#[test]
fn scope_service_lease_drop_without_runtime() {
    let host = LeaseFixture::new();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let lease = runtime.block_on(host.acquire());
    drop(runtime);
    assert!(tokio::runtime::Handle::try_current().is_err());
    drop(lease);
    assert_eq!(host.releases(), 1);
}
#[test]
fn scope_service_lease_drop_after_host_invalidation_or_destruction() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let host = LeaseFixture::new();
    let lease = runtime.block_on(host.acquire());
    host.host
        .active
        .store(false, std::sync::atomic::Ordering::SeqCst);
    drop(lease);
    assert_eq!(
        host.releases(),
        1,
        "best-effort release tolerates host rejection"
    );
    let host = LeaseFixture::new();
    let lease = runtime.block_on(host.acquire());
    let observer = host.host.clone();
    drop(host); // Remove host registry entry; the transport handle is now stale.
    drop(runtime);
    drop(lease);
    assert_eq!(
        observer.releases.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}
