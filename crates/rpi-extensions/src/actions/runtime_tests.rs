use super::*;
use serde_json::{json, Value};
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Host {
    calls: AtomicUsize,
    revoked: AtomicUsize,
    side_sinks: std::sync::Mutex<Vec<SideQuestionSink>>,
}
#[async_trait::async_trait]
impl RuntimeActionHost for Host {
    fn native_bootstrap(&self) -> Result<Value, String> {
        Ok(json!({"cwd":"/native/project","agentDir":"/native/agent"}))
    }
    async fn native_request(
        &self,
        owner: crate::native::NativeOwner,
        args: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        if args["port"] == "sessions" {
            return Ok(
                json!({"owner":owner, "persistence":crate::native::is_session_persistence(),
                "cancelled":signal.is_cancelled(), "childScope":rpi_agent::tool_execution_scope::is_child_execution()}),
            );
        }
        assert_eq!(args["port"], "context");
        assert_eq!(args["action"], "snapshot");
        Ok(json!({"mode":"rpc","hasUI":"forged","owner":"forged","identity":"forged"}))
    }
    async fn side_question(
        &self,
        owner: u64,
        args: Value,
        sink: SideQuestionSink,
    ) -> Result<Value, String> {
        assert!(args.get("__rpiRuntimeKey").is_none());
        sink(json!({"op":"register","id":"btw-test","type":"text","slot":"overlay"}))?;
        self.side_sinks.lock().unwrap().push(sink);
        Ok(json!({"owner":owner}))
    }
    async fn send_message(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn send_user_message(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn append_entry(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn set_session_name(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn get_active_tools(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn set_active_tools(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn set_model(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn get_thinking_level(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn set_thinking_level(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn compact(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn get_system_prompt(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn new_session(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn fork(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn navigate_tree(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn switch_session(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn reload(&self, _: Value) -> Result<Value, String> {
        unreachable!()
    }
    async fn child_agent(&self, owner: u64, args: Value) -> Result<Value, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            rpi_agent::tool_execution_scope::invocation_signal().is_some(),
            "dispatch must retain the originating invocation's cancellation"
        );
        assert!(
            args.get("__rpiRuntimeKey").is_none(),
            "capabilities must not reach action payloads"
        );
        Ok(json!({"owner":owner}))
    }
    fn cancel_child_agents(&self, _: u64) {
        self.revoked.fetch_add(1, Ordering::SeqCst);
    }
}
fn call(id: usize, args: Value) -> (i32, Value) {
    let mut out = StbString::empty();
    let text = args.to_string();
    let rc = trampoline_runtime_action(
        RuntimeActionId::ChildAgent.into(),
        StbStringRef::from_str(&text),
        &mut out,
        id as *mut c_void,
    );
    // SAFETY: the trampoline returned a live host allocation to its caller.
    let owned = unsafe {
        rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
    };
    (rc, serde_json::from_str(&owned.to_string_lossy()).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_actions_require_a_live_parent_invocation_and_its_capability() {
    let host = Arc::new(Host::default());
    let root = ActionBridge::new(Handle::current(), host.clone());
    let plugin = root.for_plugin("research");
    let parent = plugin.for_invocation(false, CancellationToken::new());
    let child = plugin.for_invocation(true, CancellationToken::new());
    assert_eq!(
        call(plugin.user_data() as usize, json!({"op":"start"})).0,
        1
    );
    assert_eq!(
        call(parent.user_data() as usize, json!({"op":"start"})).0,
        1
    );
    assert_eq!(
        call(
            parent.user_data() as usize,
            json!({"op":"start","__rpiRuntimeKey":"guessed"})
        )
        .0,
        1
    );
    assert_eq!(
        call(
            child.user_data() as usize,
            json!({"op":"start","__rpiRuntimeKey":child.invocation_key()})
        )
        .0,
        1
    );
    // A child synchronous callback cannot spoof another invocation's ID/key.
    {
        let _guard = child.enter_invocation();
        assert_eq!(
            call(
                parent.user_data() as usize,
                json!({"op":"start","__rpiRuntimeKey":parent.invocation_key()})
            )
            .0,
            1
        );
    }
    let accepted = call(
        parent.user_data() as usize,
        json!({"op":"start","__rpiRuntimeKey":parent.invocation_key()}),
    );
    assert_eq!(accepted.0, 0);
    assert_eq!(
        accepted.1["owner"],
        plugin.plugin_identity.as_ref().unwrap().0
    );
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    parent.invocation.as_ref().unwrap().2.cancel();
    assert_eq!(
        call(
            parent.user_data() as usize,
            json!({"op":"start","__rpiRuntimeKey":parent.invocation_key()})
        )
        .0,
        1
    );
}

#[tokio::test]
async fn dropped_candidate_and_unknown_handles_are_rejected_without_pointer_dereference() {
    let host = Arc::new(Host::default());
    let old = ActionBridge::new(Handle::current(), host.clone());
    let old_id = old.user_data() as usize;
    let candidate = ActionBridge::new(Handle::current(), host);
    let candidate_id = candidate.user_data() as usize;
    drop(candidate); // Failed reload: old bridge is still active.
    assert!(ActionBridge::resolve_handle(old_id).is_some());
    assert!(ActionBridge::resolve_handle(candidate_id).is_none());
    assert_eq!(
        call(candidate_id, json!({})).1["error"],
        "expired runtime handle"
    );
    assert_eq!(
        call(usize::MAX, json!({})).1["error"],
        "expired runtime handle"
    );
    let replacement = ActionBridge::new(Handle::current(), Arc::new(Host::default()));
    assert_ne!(replacement.user_data() as usize, candidate_id);
}

#[tokio::test]
async fn finishing_an_invocation_does_not_unload_its_plugin_owner() {
    let host = Arc::new(Host::default());
    let root = ActionBridge::new(Handle::current(), host.clone());
    let plugin = root.for_plugin("research");
    let invocation = plugin.for_invocation(false, CancellationToken::new());
    let id = invocation.user_data() as usize;
    drop(invocation);
    assert!(ActionBridge::resolve_handle(id).is_none());
    assert_eq!(host.revoked.load(Ordering::SeqCst), 0);
    root.invalidate();
    assert_eq!(host.revoked.load(Ordering::SeqCst), 1);
}

mod cancel_wait;
mod cleanup;
mod driver;
mod pacing;
mod streaming;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_runtime_lease_survives_callback_cancellation_and_drains_before_idle() {
    let root = ActionBridge::new(Handle::current(), Arc::new(Host::default()));
    let plugin = root.for_plugin("child-runtime");
    assert!(crate::native::context(3, plugin.user_data()).is_null());
    let signal = CancellationToken::new();
    let scope = Arc::new(rpi_agent::tool_execution_scope::ToolExecutionScope::default());
    let invocation = scope
        .run(async { plugin.for_invocation(true, signal.clone()) })
        .await;
    let lease = crate::native::context(3, invocation.user_data()) as usize;
    assert_ne!(lease, 0);
    signal.cancel();
    let captured = crate::native::context(0, lease as *mut c_void) as usize;
    assert_ne!(captured, 0);
    assert!(crate::native::context(2, captured as *mut c_void).is_null());
    assert!(crate::native::context(3, invocation.user_data()).is_null());
    let call = |handle: usize, args: Value| async move {
        tokio::task::spawn_blocking(move || {
            let mut out = StbString::empty();
            let rc = crate::native::request(
                StbStringRef::from_str(&args.to_string()),
                std::ptr::null(),
                &mut out,
                handle as *mut c_void,
            );
            let out = unsafe {
                rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
            };
            (
                rc,
                serde_json::from_str::<Value>(&out.to_string_lossy()).unwrap(),
            )
        })
        .await
        .unwrap()
    };
    let inspection = call(
        captured,
        json!({"version":1,"port":"context","action":"inspect"}),
    )
    .await;
    assert_eq!(inspection.0, 0);
    assert_eq!(inspection.1["result"]["scopeId"], scope.runtime_id());
    assert_eq!(inspection.1["result"]["owner"]["child"], true);
    for args in [
        json!({"version":1,"port":"auth","action":"resolve"}),
        json!({"version":1,"port":"sessions","action":"append","origin":"captured"}),
        json!({"version":1,"port":"context","action":"bootstrap"}),
    ] {
        assert_eq!(call(captured, args).await.0, 1);
    }
    let watch = tokio::spawn(call(
        captured,
        json!({"version":1,"port":"context","action":"waitClosed"}),
    ));
    tokio::task::yield_now().await;
    assert!(!watch.is_finished());
    scope.close_runtime();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), watch)
            .await
            .unwrap()
            .unwrap()
            .0,
        0
    );
    crate::native::context(1, captured as *mut c_void);
    assert!(
        tokio::time::timeout(Duration::from_millis(5), scope.wait_idle())
            .await
            .is_err()
    );
    crate::native::context(1, lease as *mut c_void);
    tokio::time::timeout(Duration::from_secs(1), scope.wait_idle())
        .await
        .unwrap();
    let scope = Arc::new(rpi_agent::tool_execution_scope::ToolExecutionScope::default());
    let invocation = scope
        .run(async { plugin.for_invocation(true, CancellationToken::new()) })
        .await;
    let lease = crate::native::context(3, invocation.user_data()) as usize;
    assert_ne!(lease, 0);
    root.invalidate();
    assert_eq!(
        call(
            lease,
            json!({"version":1,"port":"context","action":"waitClosed"})
        )
        .await
        .0,
        1
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(5), scope.wait_idle())
            .await
            .is_err()
    );
    crate::native::context(1, lease as *mut c_void);
    tokio::time::timeout(Duration::from_secs(1), scope.wait_idle())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_persistence_lease_retains_scope_but_cannot_start_new_work() {
    let root = ActionBridge::new(Handle::current(), Arc::new(Host::default()));
    let plugin = root.for_plugin("session-writer");
    let signal = CancellationToken::new();
    let scope = Arc::new(rpi_agent::tool_execution_scope::ToolExecutionScope::default());
    let invocation = scope
        .run(async { plugin.for_invocation(true, signal.clone()) })
        .await;
    let lease = crate::native::context(2, invocation.user_data()) as usize;
    assert_ne!(lease, 0);
    signal.cancel();
    assert!(crate::native::context(2, invocation.user_data()).is_null());
    let rescope = crate::native::context(0, lease as *mut c_void) as usize;
    assert_ne!(rescope, 0);
    let call = |handle: usize, args: Value| async move {
        tokio::task::spawn_blocking(move || {
            let mut out = StbString::empty();
            let rc = crate::native::request(
                StbStringRef::from_str(&args.to_string()),
                std::ptr::null(),
                &mut out,
                handle as *mut c_void,
            );
            let out = unsafe {
                rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
            };
            (
                rc,
                serde_json::from_str::<Value>(&out.to_string_lossy()).unwrap(),
            )
        })
        .await
        .unwrap()
    };
    let append = json!({"version":1,"port":"sessions","action":"journalAppend","origin":"already-captured","customType":"fixture"});
    assert_eq!(
        call(invocation.user_data() as usize, append.clone())
            .await
            .0,
        1
    );
    let result = call(rescope, append.clone()).await;
    assert_eq!(result.0, 0);
    assert_eq!(result.1["result"]["persistence"], true);
    assert_eq!(result.1["result"]["cancelled"], false);
    assert_eq!(result.1["result"]["childScope"], true);
    assert_eq!(result.1["result"]["owner"]["child"], true);
    for args in [
        json!({"port":"sessions","action":"capture"}),
        json!({"port":"context","action":"snapshot"}),
        json!({"port":"auth","action":"resolve","providerId":"cursor"}),
        json!({"port":"tools","action":"invoke"}),
    ] {
        let mut args = args;
        args["version"] = 1.into();
        assert_eq!(call(rescope, args).await.0, 1);
    }
    plugin.invalidate();
    assert_eq!(call(rescope, append).await.0, 1);
    crate::native::context(1, lease as *mut c_void);
    crate::native::context(1, rescope as *mut c_void);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn side_questions_use_plugin_owner_and_stale_sinks_cannot_resurrect_panels() {
    let host = Arc::new(Host::default());
    let root = ActionBridge::new(Handle::current(), host.clone());
    let plugin = root.for_plugin("btw");
    let invoke = |bridge: &Arc<ActionBridge>| {
        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            RuntimeActionId::SideQuestion.into(),
            StbStringRef::from_str(r#"{"op":"start","question":"q","owner":999}"#),
            &mut out,
            bridge.user_data(),
        );
        // SAFETY: trampoline transfers its output allocation to this caller.
        let output = unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
        };
        (
            rc,
            serde_json::from_str::<Value>(&output.to_string_lossy()).unwrap(),
        )
    };
    assert_eq!(invoke(&root).0, 1);
    let invocation = plugin.for_invocation(false, CancellationToken::new());
    assert_eq!(invoke(&invocation).0, 1);
    let value = invoke(&plugin);
    assert_eq!(value.0, 0);
    assert_eq!(value.1["owner"], plugin.plugin_identity.as_ref().unwrap().0);
    assert!(!root.extension_status_mailbox().panels().is_empty());
    root.invalidate();
    assert!(root.extension_status_mailbox().panels().is_empty());
    let sink = host.side_sinks.lock().unwrap()[0].clone();
    assert!(sink(json!({"op":"register","id":"btw-test","type":"text"})).is_err());
    assert!(root.extension_status_mailbox().panels().is_empty());
}

#[tokio::test]
async fn capability_discovery_is_available_during_synchronous_registration() {
    let root = ActionBridge::new(Handle::current(), Arc::new(Host::default()));
    let plugin = root.for_plugin("registering");
    let mut output = StbString::empty();
    // This runs directly on a current-thread runtime: scheduling and then blocking would deadlock.
    let status = crate::native::request(
        StbStringRef::from_str(r#"{"version":1,"port":"capabilities","action":"inspect"}"#),
        std::ptr::null(),
        &mut output,
        plugin.user_data(),
    );
    let output = unsafe {
        rpi_plugin_sdk::callbacks::OwnedString::from_raw(output, Some(crate::host_free_string))
    };
    assert_eq!(status, 0);
    let response: Value = serde_json::from_str(&output.to_string_lossy()).unwrap();
    assert!(response["result"]
        .as_array()
        .unwrap()
        .contains(&json!("auth.publicErrors")));
    plugin.invalidate();
    let mut output = StbString::empty();
    assert_ne!(
        crate::native::request(
            StbStringRef::from_str(r#"{"version":1,"port":"capabilities","action":"inspect"}"#),
            std::ptr::null(),
            &mut output,
            plugin.user_data()
        ),
        0
    );
    unsafe {
        drop(rpi_plugin_sdk::callbacks::OwnedString::from_raw(
            output,
            Some(crate::host_free_string),
        ));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn native_bootstrap_is_synchronous_and_checks_owner_lifetime() {
    let root = ActionBridge::new(Handle::current(), Arc::new(Host::default()));
    let plugin = root.for_plugin("registering");
    let invoke = |handle| {
        let mut output = StbString::empty();
        let status = crate::native::request(
            StbStringRef::from_str(
                r#"{"version":1,"port":"context","action":"bootstrap","agentDir":"/forged"}"#,
            ),
            std::ptr::null(),
            &mut output,
            handle,
        );
        let output = unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(output, Some(crate::host_free_string))
        };
        (
            status,
            serde_json::from_str::<Value>(&output.to_string_lossy()).unwrap(),
        )
    };
    assert_eq!(
        invoke(plugin.user_data()),
        (
            0,
            json!({"version":1,"result":{"cwd":"/native/project","agentDir":"/native/agent"}})
        )
    );
    assert_eq!(invoke(root.user_data()).0, 1);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        invoke(plugin.for_invocation(false, cancelled).user_data()).0,
        1
    );
    plugin.invalidate();
    assert_eq!(invoke(plugin.user_data()).0, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_snapshot_and_status_use_the_invocation_owner_and_attached_ui() {
    let root = ActionBridge::new(Handle::current(), Arc::new(Host::default()));
    let plugin = root.for_plugin("fixture");
    let other = root.for_plugin("other");
    let request = |bridge: &Arc<ActionBridge>, args: Value| {
        let mut output = StbString::empty();
        let status = crate::native::request(
            StbStringRef::from_str(&args.to_string()),
            std::ptr::null(),
            &mut output,
            bridge.user_data(),
        );
        let output = unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(output, Some(crate::host_free_string))
        };
        assert_eq!(status, 0, "{}", output.to_string_lossy());
        serde_json::from_str::<Value>(&output.to_string_lossy()).unwrap()["result"].clone()
    };
    let identity = rpi_ai::identity::RequestIdentity::operation(Some("session".into()), "fixture");
    let invocation = rpi_ai::identity::scope(identity.clone(), async {
        plugin.for_invocation(false, CancellationToken::new())
    })
    .await;
    let inspect = json!({"version":1,"port":"context","action":"snapshot"});
    let value = request(&invocation, inspect.clone());
    assert_eq!(value["identity"], json!(identity));
    assert_eq!(value["owner"]["plugin"], "fixture");
    assert_eq!(value["hasUI"], false);
    root.ui_dialog_mailbox().attach();
    assert_eq!(request(&invocation, inspect)["hasUI"], true);
    let status =
        |value| json!({"version":1,"port":"ui","action":"status","key":"same-key","value":value});
    request(&plugin, status("first"));
    request(&other, status("second"));
    assert_eq!(root.extension_status_mailbox().panels().len(), 2);
    request(&plugin, status("updated"));
    assert_eq!(root.extension_status_mailbox().panels().len(), 2);
    request(&plugin, status(""));
    assert_eq!(root.extension_status_mailbox().panels().len(), 1);
    root.invalidate();
    assert!(root.extension_status_mailbox().panels().is_empty());
}

#[tokio::test]
async fn native_context_reports_the_captured_invocation_and_rejects_expired_handles() {
    let root = ActionBridge::new(Handle::current(), Arc::new(Host::default()));
    let plugin = root.for_plugin("owner");
    let child = plugin.for_invocation(true, CancellationToken::new());
    let invoke = |handle| {
        let mut output = StbString::empty();
        let status = crate::native::request(
            StbStringRef::from_str(
                r#"{"version":1,"port":"context","action":"inspect","owner":{"child":false}}"#,
            ),
            std::ptr::null(),
            &mut output,
            handle,
        );
        let output = unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(output, Some(crate::host_free_string))
        };
        (
            status,
            serde_json::from_str::<Value>(&output.to_string_lossy()).unwrap(),
        )
    };
    let parent_result = invoke(plugin.user_data());
    assert_eq!(parent_result.0, 0);
    assert_eq!(parent_result.1["result"]["owner"]["child"], false);
    let handle = crate::native::context(0, child.user_data());
    assert!(!handle.is_null());
    let result = invoke(handle);
    assert_eq!(result.0, 0);
    assert_eq!(result.1["result"]["owner"]["child"], true);
    assert_eq!(
        result.1["result"]["owner"]["id"],
        parent_result.1["result"]["owner"]["id"]
    );
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        invoke(plugin.for_invocation(false, cancelled).user_data()).0,
        1
    );
    plugin.invalidate();
    assert_eq!(invoke(handle).0, 1);
    crate::native::context(1, handle);
}
