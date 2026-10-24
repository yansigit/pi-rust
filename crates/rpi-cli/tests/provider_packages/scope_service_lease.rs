use super::*;
use rpi_cli::extensions_actions::HarnessActionHost;
use rpi_extensions::{ActionBridge, RuntimeActionHost};
use rpi_harness::{
    agent_harness::{AgentHarness, AgentLane},
    types::{AgentHarnessOptions, CompactionSettings},
};
use serde_json::{json, Value};
async fn wait_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("missing {}", path.display()));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loaded_scope_service_compaction_after_callback_is_selected_idle_only_and_revocable() {
    for scenario in [
        "child", "main", "closing", "epoch", "unload", "busy", "cancel",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("worker");
        let (host, cell) = HarnessActionHost::new_empty(
            vec![],
            directory.path().into(),
            tokio::runtime::Handle::current(),
            Default::default(),
            Default::default(),
        );
        let summary = rpi_ai::providers::faux::FauxProvider::new(
            rpi_ai::providers::faux::FauxScript::new()
                .with_text("fixture summary")
                .with_tokens_per_second(if matches!(scenario, "busy" | "cancel") {
                    1.0
                } else {
                    1e9
                }),
        );
        let parent = AgentHarness::create(AgentHarnessOptions {
            model: summary.models()[0].clone(),
            models: vec![summary],
            compaction: CompactionSettings {
                enabled: false,
                reserve_tokens: 1,
                keep_recent_tokens: 1,
            },
            ..Default::default()
        })
        .await
        .unwrap();
        HarnessActionHost::set_harness(&cell, Arc::new(parent.clone()));
        let child_session = rpi_harness::session::Session::new(
            Arc::new(rpi_harness::session::InMemorySessionStorage::new(
                rpi_harness::session::types::SessionMetadata {
                    id: "scope-child".into(),
                    created_at: 0,
                    parent_session_id: None,
                },
                Arc::new(rpi_harness::session::SystemClock),
                Arc::new(rpi_harness::session::DefaultIdGenerator::new()),
            )),
            None,
        );
        let child = parent
            .create_child(
                child_session,
                Arc::new(rpi_agent::CollectorEmitter::new().0),
                1,
                None,
            )
            .await
            .unwrap();
        let selected = if scenario == "main" {
            parent.clone()
        } else {
            child.harness.clone()
        };
        for lane in [&parent, &child.harness] {
            for index in 0..5 {
                lane.append_message(rpi_agent::AgentMessage::User(
                    rpi_ai::types::UserMessage::new(
                        format!("{index} {}", "history ".repeat(200)),
                        index,
                    ),
                ))
                .await
                .unwrap();
            }
        }
        let main_tip = parent.get_tip_id().await.unwrap();
        let child_tip = child.harness.get_tip_id().await.unwrap();
        let host = Arc::new(host);
        let bridge = ActionBridge::new(tokio::runtime::Handle::current(), host.clone());
        let session = rpi_extensions::load_session_mixed(
            &[],
            &[delegation_library::scope_library().into()],
            Arc::new(rpi_extensions::NullDiagnostics),
            Some(bridge),
        );
        let providers = rpi_extensions::PluggableProvider::from_session(
            &session,
            tokio::runtime::Handle::current(),
        );
        assert_eq!(providers.len(), 1);
        let provider = &providers[0];
        let epoch = DelegationEpoch::new(DelegationScopeId::fresh(), providers.clone());
        let resolver: Arc<dyn DelegationResolver> = Arc::new(Resolver(epoch.clone()));
        let binding =
            DelegationBinding::root(Arc::downgrade(&resolver), epoch.clone(), "scope-lease")
                .unwrap();
        let mut options = SimpleStreamOptions::default();
        options.metadata = Some(
            serde_json::from_value(json!({"marker":marker,"cancel":scenario == "cancel"})).unwrap(),
        );
        let work = binding.scope(async {
            provider
                .stream_simple(&provider.models()[0], &Context::default(), &options)
                .await
                .result()
                .await
                .unwrap()
        });
        let message = if scenario == "main" {
            work.await
        } else {
            child.execution_scope.run(work).await
        };
        assert_eq!(message.stop_reason, StopReason::Stop, "{message:?}");
        wait_file(&marker.with_extension("ready")).await;
        // An independently observed ordinary invocation expiry is the worker's gate.
        assert_eq!(
            std::fs::read_to_string(marker.with_extension("ready")).unwrap(),
            "expired"
        );
        let mut run = None;
        match scenario {
            "closing" => child.execution_scope.close_runtime(),
            "epoch" => epoch.revoke(),
            "unload" => session.begin_shutdown(),
            "busy" => {
                assert_eq!(
                    child
                        .execution_scope
                        .run(host.is_idle(json!({})))
                        .await
                        .unwrap()["isIdle"],
                    true
                );
                let target = selected.clone();
                run = Some(tokio::spawn(async move {
                    target.prompt_text("newer retry", vec![]).await
                }));
                tokio::time::timeout(Duration::from_secs(3), async {
                    while child
                        .execution_scope
                        .run(host.is_idle(json!({})))
                        .await
                        .unwrap()["isIdle"]
                        == true
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            }
            _ => {}
        }
        std::fs::write(marker.with_extension("go"), "go").unwrap();
        wait_file(&marker.with_extension("result")).await;
        let result: Value =
            serde_json::from_slice(&std::fs::read(marker.with_extension("result")).unwrap())
                .unwrap();
        match scenario {
            "child" | "main" => {
                assert_eq!(result["idle"]["Ok"]["isIdle"], true, "{result}");
                assert!(
                    result["compact"]["Ok"]["outcome"]
                        .as_str()
                        .unwrap()
                        .starts_with("Complete"),
                    "{result}"
                );
                let query = rpi_harness::session::types::EntryQuery {
                    entry_type: Some("compaction"),
                    ..Default::default()
                };
                assert_eq!(
                    selected.find_entries(&query).await.unwrap().len(),
                    1,
                    "{scenario}: no selected transcript compaction"
                );
                let other = if scenario == "main" {
                    &child.harness
                } else {
                    &parent
                };
                assert!(
                    other.find_entries(&query).await.unwrap().is_empty(),
                    "{scenario}: wrong transcript compacted"
                );
            }
            "cancel" => {
                assert_eq!(
                    result["compact"]["Err"], "native runtime request cancelled",
                    "{result}"
                );
                tokio::time::timeout(Duration::from_secs(3), async {
                    while child
                        .execution_scope
                        .run(host.is_idle(json!({})))
                        .await
                        .unwrap()["isIdle"]
                        != true
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("cancelled compaction retained the lane");
                let query = rpi_harness::session::types::EntryQuery {
                    entry_type: Some("compaction"),
                    ..Default::default()
                };
                assert!(selected.find_entries(&query).await.unwrap().is_empty());
            }
            "busy" => {
                assert_eq!(result["idle"]["Ok"]["isIdle"], false, "{result}");
                assert_eq!(
                    result["compact"]["Err"], "selected execution scope is busy",
                    "{result}"
                );
                assert!(
                    !run.as_ref().unwrap().is_finished(),
                    "delayed compaction aborted newer retry"
                );
                selected.abort().await.unwrap();
                run.take().unwrap().await.unwrap().unwrap();
            }
            _ => {
                assert!(
                    result["compact"].get("Err").is_some(),
                    "{scenario}: {result}"
                );
                assert_eq!(
                    child.harness.get_tip_id().await.unwrap(),
                    child_tip,
                    "revoked grant modified child"
                );
            }
        }
        if scenario != "main" {
            assert_eq!(
                parent.get_tip_id().await.unwrap(),
                main_tip,
                "{scenario}: main transcript changed"
            );
        } else {
            assert_eq!(child.harness.get_tip_id().await.unwrap(), child_tip);
        }
        session.begin_shutdown();
        drop(session);
        drop(child);
    }
}
