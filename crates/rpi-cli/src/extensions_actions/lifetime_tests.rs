use super::*;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_harness::types::AgentHarnessOptions;
use serde_json::json;

#[tokio::test(flavor = "current_thread")]
async fn native_bootstrap_uses_rpi_locations_before_harness_construction() {
    let cwd = std::env::temp_dir().join("native-bootstrap-project");
    let (host, _) = HarnessActionHost::new_empty(
        vec![],
        cwd.clone(),
        Handle::current(),
        BTreeMap::new(),
        Default::default(),
    );
    assert_eq!(
        host.native_bootstrap().unwrap(),
        json!({
            "cwd": cwd,
            "agentDir": crate::config::agent_dir().unwrap(),
            "sessionDir": default_session_dir(&cwd),
        })
    );
    assert!(host.get_execution_context(json!({})).await.is_err());
}

#[tokio::test]
async fn native_execution_context_and_session_capture_agree_on_child_identity() {
    use rpi_harness::session::types::SessionMetadata;
    use rpi_harness::session::{DefaultIdGenerator, InMemorySessionStorage, Session, SystemClock};
    let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
    let env = Arc::new(rpi_tools::InMemoryExecutionEnv::new());
    let tool_context = rpi_tools::ExecutionToolContext::new(env.clone(), Some(env));
    let parent = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        tools: vec![
            rpi_harness::types::HarnessTool::new(rpi_tools::create_read_tool(&tool_context, None)),
            rpi_harness::types::HarnessTool::new(rpi_tools::create_write_tool(&tool_context)),
        ],
        active_tool_names: vec!["read".into(), "write".into()],
        ..Default::default()
    })
    .await
    .unwrap();
    let (host, cell) = HarnessActionHost::new_empty(
        vec![],
        std::env::temp_dir(),
        Handle::current(),
        BTreeMap::new(),
        Default::default(),
    );
    HarnessActionHost::set_harness(&cell, Arc::new(parent.clone()));
    let snapshot_request = json!({"version":1,"port":"context","action":"snapshot","mode":"forged","projectTrusted":true});
    let owner = rpi_extensions::native::NativeOwner {
        id: 1,
        generation: "generation".into(),
        plugin: "fixture".into(),
        child: false,
    };
    assert!(host
        .native_request(owner.clone(), snapshot_request.clone(), Default::default())
        .await
        .unwrap_err()
        .contains("before startup"));
    host.set_native_mode("rpc");
    let snapshot = host
        .native_request(owner.clone(), snapshot_request.clone(), Default::default())
        .await
        .unwrap();
    assert_eq!(snapshot["mode"], "rpc");
    assert_eq!(snapshot["projectTrusted"], false);
    assert_eq!(snapshot["projectTrustResolved"], false);
    assert_eq!(snapshot["isIdle"], true);
    assert_eq!(snapshot["hasPendingMessages"], false);
    assert!(snapshot["model"].get("headers").is_none());
    assert!(snapshot["model"].get("baseUrl").is_none());
    for (trusted, resolved) in [(true, false), (false, true), (true, true)] {
        host.set_native_project_trust(trusted, resolved);
        let snapshot = host
            .native_request(owner.clone(), snapshot_request.clone(), Default::default())
            .await
            .unwrap();
        assert_eq!(snapshot["projectTrusted"], trusted);
        assert_eq!(snapshot["projectTrustResolved"], resolved);
    }
    let abort = json!({"port":"context","action":"abort"});
    assert!(host
        .native_request(owner.clone(), abort.clone(), Default::default())
        .await
        .unwrap_err()
        .contains("active operation identity"));
    let session = Session::new(
        Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "native-child".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )),
        None,
    );
    let child = parent
        .create_child(
            session,
            Arc::new(rpi_agent::CollectorEmitter::new().0),
            1,
            None,
        )
        .await
        .unwrap();
    let scope = child.execution_scope.clone();
    scope
        .run(async {
            let context = host.get_execution_context(json!({})).await.unwrap();
            assert_eq!(context["sessionId"], "native-child");
            let snapshot = host
                .native_request(owner.clone(), snapshot_request, Default::default())
                .await
                .unwrap();
            assert_eq!(snapshot["sessionId"], "native-child");
            assert_eq!(snapshot["projectTrusted"], true);
            assert_eq!(snapshot["projectTrustResolved"], true);
            host.native_request(
                owner.clone(),
                json!({"port":"tools","action":"setActive","tools":["read","missing","read"]}),
                Default::default(),
            )
            .await
            .unwrap();
            assert_eq!(
                child.harness.lane("main").get_active_tools().await.unwrap(),
                vec!["read"]
            );
            assert_eq!(
                parent.lane("main").get_active_tools().await.unwrap(),
                vec!["read", "write"]
            );
            host.native_request(
                owner.clone(),
                json!({"port":"tools","action":"setActive","tools":[]}),
                Default::default(),
            )
            .await
            .unwrap();
            assert!(child
                .harness
                .inspect_tools()
                .unwrap()
                .iter()
                .all(|tool| tool["active"] == false));
            assert_eq!(
                parent.lane("main").get_active_tools().await.unwrap(),
                vec!["read", "write"]
            );
            let foreign = rpi_ai::identity::RequestIdentity::operation(
                Some(parent.session().storage().metadata().id.clone()),
                "prompt",
            );
            assert!(rpi_ai::identity::scope(
                foreign,
                host.native_request(owner.clone(), abort.clone(), Default::default())
            )
            .await
            .unwrap_err()
            .contains("another session"));
            let stale =
                rpi_ai::identity::RequestIdentity::operation(Some("native-child".into()), "prompt");
            assert_eq!(
                rpi_ai::identity::scope(
                    stale,
                    host.native_request(owner.clone(), abort.clone(), Default::default())
                )
                .await
                .unwrap(),
                json!({"aborted":false})
            );
            let captured = host
                .native_request(
                    rpi_extensions::native::NativeOwner {
                        id: 1,
                        generation: "generation".into(),
                        plugin: "fixture".into(),
                        child: true,
                    },
                    json!({"version":1,"port":"sessions","action":"capture"}),
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap();
            assert_eq!(captured["snapshot"]["metadata"]["id"], context["sessionId"]);
        })
        .await;
    drop(child);
    assert!(scope
        .run(host.get_execution_context(json!({})))
        .await
        .is_err());
    assert_eq!(
        host.get_execution_context(json!({})).await.unwrap()["sessionId"],
        parent.session().storage().metadata().id
    );
}

#[tokio::test]
async fn native_flags_only_return_requested_names_before_harness_construction() {
    let (host, _) = HarnessActionHost::new_empty(
        vec![],
        std::env::temp_dir(),
        Handle::current(),
        BTreeMap::from([
            ("chosen".into(), json!(true)),
            ("unrelated".into(), json!("private")),
        ]),
        Default::default(),
    );
    let owner = rpi_extensions::native::NativeOwner {
        id: 1,
        generation: "generation".into(),
        plugin: "fixture".into(),
        child: false,
    };
    assert_eq!(
        host.native_request(
            owner.clone(),
            json!({"port":"context","action":"flags","names":["chosen","missing"]}),
            Default::default()
        )
        .await
        .unwrap(),
        json!({"chosen":true})
    );
    assert!(host
        .native_request(
            owner,
            json!({"port":"context","action":"flags","names":[42]}),
            Default::default()
        )
        .await
        .is_err());
}

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
