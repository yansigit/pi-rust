use super::*;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_harness::types::AgentHarnessOptions;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_reverse_bindings_fail_closed_but_still_reclaim_their_children() {
    let (host, cell) = HarnessActionHost::new_empty(
        vec![],
        std::env::temp_dir(),
        Handle::current(),
        BTreeMap::new(),
        rpi_extensions::ToolCallContext::default(),
    );
    assert!(host
        .get_execution_context(json!({}))
        .await
        .unwrap_err()
        .contains("before harness"));
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_text("slow")
            .with_tokens_per_second(0.1),
    );
    let parent = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap();
    HarnessActionHost::set_harness(&cell, Arc::new(parent.clone()));
    assert!(host.get_execution_context(json!({})).await.is_ok());
    let child = host
        .child_agent(7, json!({"op":"start","task":"report"}))
        .await
        .unwrap();
    let id = &child["childId"];
    drop(parent);
    assert!(host
        .get_execution_context(json!({}))
        .await
        .unwrap_err()
        .contains("after harness"));
    assert!(host
        .child_agent(7, json!({"op":"start","task":"stale"}))
        .await
        .unwrap_err()
        .contains("after harness"));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let value = host
                .child_agent(7, json!({"op":"poll","childId":id}))
                .await
                .unwrap();
            if value["status"] == "failed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert!(host
        .child_agent(8, json!({"op":"dispose","childId":id}))
        .await
        .is_err());
    host.child_agent(7, json!({"op":"dispose","childId":id}))
        .await
        .unwrap();
    assert!(host
        .child_agent(7, json!({"op":"poll","childId":id}))
        .await
        .is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires RPI_TEST_STUB_LIBRARY built with the native session fixture"]
async fn native_session_commits_cross_loaded_library_and_survive_reopen() {
    use rpi_extensions::{ActionBridge, HostApi, NullDiagnostics};
    use rpi_harness::session::jsonl::JsonlSessionStorage;
    use rpi_harness::session::memory::SystemClock;
    use rpi_harness::session::types::{Entry, SessionStorage};
    use rpi_harness::session::DefaultIdGenerator;
    let path = std::env::var_os("RPI_TEST_STUB_LIBRARY").expect("set current fixture library");
    let dir = tempfile::tempdir().unwrap();
    let session = crate::session::create_jsonl_session(dir.path(), dir.path().to_str().unwrap())
        .await
        .unwrap();
    let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
    let harness = AgentHarness::create(AgentHarnessOptions {
        session,
        model: provider.default_model().clone(),
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap();
    let (host, cell) = HarnessActionHost::new_empty(
        vec![],
        dir.path().into(),
        Handle::current(),
        BTreeMap::new(),
        Default::default(),
    );
    HarnessActionHost::set_harness(&cell, Arc::new(harness.clone()));
    let bridge = ActionBridge::new(Handle::current(), Arc::new(host));
    let api = HostApi::with_action_bridge(
        "session-fixture",
        Default::default(),
        Arc::new(NullDiagnostics),
        bridge.clone(),
    );
    let loaded =
        rpi_extensions::load_one(path, Arc::new(NullDiagnostics), Some(bridge.clone())).unwrap();
    let status = tokio::task::spawn_blocking(move || {
        // The loaded library and API stay alive until the foreign call returns.
        let call = unsafe {
            loaded
                .library
                .get::<extern "C" fn(rpi_plugin_sdk::PluginApi) -> i32>(
                    b"plugin_stub_native_session",
                )
                .unwrap()
        };
        let status = call(api.build_vtable());
        (status, loaded, api)
    })
    .await
    .unwrap();
    assert_eq!(status.0, 0);
    bridge.finish_native_writes().await;
    let entries = harness
        .session()
        .storage()
        .snapshot()
        .await
        .unwrap()
        .entries;
    assert!(entries.iter().any(|entry| matches!(entry, Entry::Custom(custom) if custom.custom_type == "fixture:native-commit")));
    let mut paths = vec![dir.path().to_owned()];
    let mut log = None;
    while let Some(path) = paths.pop() {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                paths.push(path);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                log = Some(path);
            }
        }
    }
    let reopened = JsonlSessionStorage::load(
        Arc::new(rpi_tools::OsExecutionEnv::with_cwd(dir.path().into())),
        log.unwrap().to_str().unwrap(),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(reopened.snapshot().await.unwrap().entries).unwrap(),
        serde_json::to_value(entries).unwrap()
    );
}
