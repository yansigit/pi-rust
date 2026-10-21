use super::*;
use crate::session::memory::{InMemorySessionStorage, SystemClock};
use crate::session::session::DefaultIdGenerator;
use crate::session::types::SessionMetadata;
use crate::types::HarnessTool;
use rpi_agent::{BeforeToolCallResult, CollectorEmitter};
use rpi_ai::providers::faux::{FauxProvider, FauxScript, FauxStep};

fn session(id: &str) -> Session {
    Session::new(
        Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: id.into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )),
        None,
    )
}
fn emitter() -> Arc<dyn AgentEmitter> {
    Arc::new(CollectorEmitter::new().0)
}

#[tokio::test]
async fn named_abort_only_cancels_the_matching_run_on_its_lane() {
    let (harness, _, _) = parent(FauxScript::new()).await;
    let (old, old_signal, _) = harness.acquire_run(OperationKind::Run).await.unwrap();
    assert!(harness.request_abort(&old).await.unwrap());
    assert!(old_signal.is_cancelled());
    harness.release_run(&old);
    let (current, signal, _) = harness.acquire_run(OperationKind::Run).await.unwrap();
    assert!(!harness.request_abort(&old).await.unwrap());
    assert!(!signal.is_cancelled());
    assert!(harness.lane("other").request_abort(&current).await.is_err());
    assert!(!signal.is_cancelled());
    assert!(harness.request_abort(&current).await.unwrap());
    assert!(signal.is_cancelled());
    harness.release_run(&current);
    assert!(!harness.request_abort(&current).await.unwrap());
}

async fn parent(
    script: FauxScript,
) -> (
    AgentHarness,
    Arc<FauxProvider>,
    Arc<rpi_tools::InMemoryExecutionEnv>,
) {
    let provider = FauxProvider::new(script);
    let env = Arc::new(rpi_tools::InMemoryExecutionEnv::new());
    let ctx = rpi_tools::ExecutionToolContext::new(env.clone(), Some(env.clone()));
    let tools = vec![
        HarnessTool::new(rpi_tools::create_read_tool(&ctx, None)),
        HarnessTool::new(rpi_tools::create_write_tool(&ctx)),
    ];
    let parent = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider.clone()],
        tools,
        session: session("parent"),
        system_prompt: Some("project instructions".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    (parent, provider, env)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshots_live_binding_and_runs_during_an_active_parent_without_touching_its_transcript() {
    let script = FauxScript::new().with_text("child report");
    let (parent, provider, _) = parent(script).await;
    let mut model = provider.default_model().clone();
    model.id = "live-model".into();
    model.base_url = "https://secret-endpoint.test/?token=secret".into();
    model.headers = Some(std::collections::BTreeMap::from([(
        "Authorization".into(),
        "private-key".into(),
    )]));
    parent.set_model(model.clone()).await.unwrap();
    parent
        .set_thinking_level(rpi_ai::ThinkingLevel::High)
        .await
        .unwrap();
    parent.set_active_tools(vec!["read".into()]).await.unwrap();
    let (run_id, _, _) = parent.acquire_run(OperationKind::Run).await.unwrap();
    let child = parent
        .create_child(session("child"), emitter(), 2, None)
        .await
        .unwrap();
    let snap = child.harness.snapshot_config().unwrap();
    assert_eq!(snap.model, model);
    assert_eq!(snap.thinking_level, rpi_ai::ThinkingLevel::High);
    assert_eq!(snap.system_prompt.as_deref(), Some("project instructions"));
    assert!(Arc::ptr_eq(
        &snap.models[0],
        &parent.snapshot_config().unwrap().models[0]
    ));
    assert_eq!(snap.tools.len(), 1);
    let context = parent.execution_context().unwrap().to_string();
    assert!(!context.contains("private-key"));
    assert!(!context.contains("secret-endpoint"));
    let result = child.harness.prompt_text("do work", vec![]).await.unwrap();
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert!(parent.get_tip_id().await.unwrap().is_none());
    assert!(child.harness.get_tip_id().await.unwrap().is_some());
    assert_eq!(child.harness.session().storage().metadata().id, "child");
    parent.release_run(&run_id);
    assert!(child.scope.is_cancelled());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn children_cannot_expand_tools_or_create_grandchildren() {
    let (parent, _, _) = parent(FauxScript::new()).await;
    parent.set_active_tools(vec!["read".into()]).await.unwrap();
    assert!(parent
        .create_child(parent.session().clone(), emitter(), 1, None)
        .await
        .is_err());
    assert!(parent
        .create_child(session("bad"), emitter(), 1, Some(vec!["write".into()]))
        .await
        .is_err());
    let child = parent
        .create_child(session("child"), emitter(), 1, Some(vec![]))
        .await
        .unwrap();
    assert!(child.harness.get_tools().await.unwrap().is_empty());
    assert!(child
        .harness
        .create_child(session("grandchild"), emitter(), 1, None)
        .await
        .is_err());
    assert!(child
        .execution_scope
        .run(parent.create_child(session("via-root"), emitter(), 1, None))
        .await
        .is_err());
    assert!(parent
        .create_child(session("bad"), emitter(), 0, None)
        .await
        .is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_turn_budget_is_per_instance_and_does_not_change_parent_or_environment() {
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::tool_call("read", serde_json::json!({"path":"test.txt"})),
        FauxStep::text("must remain unused"),
    ]);
    let (parent, provider, env) = parent(script).await;
    use rpi_tools::FileSystem;
    env.write_file(
        "test.txt",
        rpi_tools::FileContent::Text("fixture".into()),
        None,
    )
    .await
    .unwrap();
    let before = std::env::var_os(crate::run_budget::MAX_TURNS_ENV);
    let child = parent
        .create_child(session("child"), emitter(), 1, None)
        .await
        .unwrap();
    child
        .harness
        .prompt_text("read file", vec![])
        .await
        .unwrap();
    assert_eq!(provider.script().pending_count(), 1);
    assert!(parent.max_turns.is_none());
    assert_eq!(child.harness.max_turns, Some(1));
    assert_eq!(std::env::var_os(crate::run_budget::MAX_TURNS_ENV), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approval_hook_is_inherited_and_blocks_mutating_child_tools() {
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::tool_call(
            "write",
            serde_json::json!({"path":"blocked.txt","content":"unsafe"}),
        ),
        FauxStep::text("denied"),
    ]);
    let (parent, _, env) = parent(script).await;
    let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = called.clone();
    let weak_parent = parent.downgrade();
    parent.inner.lock().unwrap().before_tool_call = Some(Arc::new(move |_, _| {
        flag.store(true, std::sync::atomic::Ordering::Release);
        assert!(rpi_agent::tool_execution_scope::is_child_execution());
        let root = weak_parent.upgrade().unwrap();
        Box::pin(async move {
            assert!(root
                .create_child(session("recursive-hook"), emitter(), 1, None)
                .await
                .is_err());
            Some(BeforeToolCallResult {
                block: true,
                reason: Some("permission denied".into()),
                terminate: false,
                args: None,
            })
        })
    }));
    let child = parent
        .create_child(session("child"), emitter(), 3, None)
        .await
        .unwrap();
    child.harness.prompt_text("write", vec![]).await.unwrap();
    assert!(called.load(std::sync::atomic::Ordering::Acquire));
    use rpi_tools::FileSystem;
    assert!(env.read_text_file("blocked.txt", None).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_and_session_switch_cancel_old_scopes_but_new_session_can_delegate() {
    let (parent, _, _) = parent(FauxScript::new()).await;
    let first = parent
        .create_child(session("first"), emitter(), 1, None)
        .await
        .unwrap();
    parent.set_session(session("next-parent")).await.unwrap();
    assert!(first.scope.is_cancelled());
    let next = parent
        .create_child(session("next-child"), emitter(), 1, None)
        .await
        .unwrap();
    assert!(!next.scope.is_cancelled());
    parent.close().await.unwrap();
    assert!(next.scope.is_cancelled());
    assert!(parent
        .create_child(session("closed"), emitter(), 1, None)
        .await
        .is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn side_question_inherits_private_binding_but_not_main_run_lifetime_or_tools() {
    let (parent, provider, _) = parent(FauxScript::new().with_text("side answer")).await;
    let mut model = provider.default_model().clone();
    model.id = "live-model".into();
    model.base_url = "https://private-endpoint.test".into();
    // Exercise inheritance with a model that can actually enable thinking.
    model.reasoning = true;
    model.headers = Some(BTreeMap::from([(
        "Authorization".into(),
        "private-key".into(),
    )]));
    parent.set_model(model.clone()).await.unwrap();
    parent
        .set_thinking_level(rpi_ai::ThinkingLevel::High)
        .await
        .unwrap();
    let (run_id, _, _) = parent.acquire_run(OperationKind::Run).await.unwrap();
    let parent_tip = parent.get_tip_id().await.unwrap();
    let (child, reference) = parent
        .create_side_question(session("side"), emitter())
        .await
        .unwrap();
    assert!(reference.is_empty());
    let config = child.harness.snapshot_config().unwrap();
    assert_eq!(config.model, model);
    assert!(Arc::ptr_eq(
        &config.models[0],
        &parent.snapshot_config().unwrap().models[0]
    ));
    assert_eq!(config.thinking_level, rpi_ai::ThinkingLevel::High);
    assert!(config.tools.is_empty());
    assert!(config.before_tool_call.is_none());
    assert!(config.transform_context.is_none());
    assert_eq!(child.harness.max_turns, Some(1));
    parent.release_run(&run_id);
    assert!(
        !child.scope.is_cancelled(),
        "main completion must not cancel side questions"
    );
    let result = child.harness.prompt_text("side?", vec![]).await.unwrap();
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert_eq!(parent.get_tip_id().await.unwrap(), parent_tip);
    parent.close().await.unwrap();
    assert!(child.scope.is_cancelled());
}

#[test]
fn side_reference_is_utf8_bounded_and_excludes_non_text_blocks() {
    use rpi_ai::types::{UserContent, UserMessage};
    let messages = vec![
        AgentMessage::User(UserMessage {
            role: rpi_ai::types::UserRole,
            content: UserContent::Text("old-secret-marker".into()),
            timestamp: 0,
        }),
        AgentMessage::User(UserMessage {
            role: rpi_ai::types::UserRole,
            content: UserContent::Text("中".repeat(30_000)),
            timestamp: 0,
        }),
    ];
    let reference = side_question_reference(&messages);
    assert!(reference.len() <= 64 * 1024);
    assert!(!reference.contains("old-secret-marker"));
    assert!(reference.contains("[user]"));
}

#[tokio::test]
async fn direct_tool_ports_respect_child_tools_lifetime_and_session_without_new_turns() {
    let (parent, provider, env) = parent(FauxScript::new().with_text("must not consume")).await;
    let child = parent
        .create_child(session("child"), emitter(), 1, Some(vec!["read".into()]))
        .await
        .unwrap();
    use rpi_tools::FileSystem;
    env.write_file(
        "test.txt",
        rpi_tools::FileContent::Text("fixture".into()),
        None,
    )
    .await
    .unwrap();
    let call = |name: &str, args| {
        serde_json::from_value(
            serde_json::json!({"type":"toolCall","id":"direct", "name":name,"arguments":args}),
        )
        .unwrap()
    };
    let scope = child.execution_scope.clone();
    scope
        .run(async {
            let target = parent.for_current_execution().unwrap();
            assert_eq!(target.session().storage().metadata().id, "child");
            let tools = target.inspect_tools().unwrap();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0]["name"], "read");
            assert_eq!(tools[0]["active"], true);
            let result = target
                .invoke_tool(
                    call("read", serde_json::json!({"path":"test.txt"})),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(!result.is_error);
            let result = target
                .invoke_tool(
                    call(
                        "write",
                        serde_json::json!({"path":"forbidden", "content":"bad"}),
                    ),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(result.is_error);
            let identity =
                rpi_ai::identity::RequestIdentity::operation(Some("parent".into()), "native-tool");
            assert!(rpi_ai::identity::scope(
                identity,
                target.invoke_tool(
                    call("read", serde_json::json!({"path":"test.txt"})),
                    CancellationToken::new()
                )
            )
            .await
            .is_err());
        })
        .await;
    assert!(env.read_text_file("forbidden", None).await.is_err());
    assert!(parent.get_tip_id().await.unwrap().is_none());
    assert!(child.harness.get_tip_id().await.unwrap().is_none());
    assert_eq!(provider.script().pending_count(), 1);
    let (side, _) = parent
        .create_side_question(session("side"), emitter())
        .await
        .unwrap();
    side.execution_scope
        .run(async {
            assert!(parent
                .for_current_execution()
                .unwrap()
                .inspect_tools()
                .unwrap()
                .is_empty());
        })
        .await;
    drop(child);
    assert!(scope
        .run(async { parent.for_current_execution() })
        .await
        .is_err());
    assert_eq!(parent.inspect_tools().unwrap().len(), 2);
}

#[tokio::test]
async fn native_child_runtime_closes_after_active_run_and_before_last_scope_drop() {
    let (parent, _, _) = parent(FauxScript::new()).await;
    let (child, _) = parent
        .create_side_question(session("lifetime"), emitter())
        .await
        .unwrap();
    let scope = child.execution_scope.clone();
    let retained = scope.retain_runtime().unwrap();
    let (run_id, signal, _) = child.harness.acquire_run(OperationKind::Run).await.unwrap();
    child.harness.close().await.unwrap();
    assert!(!signal.is_cancelled());
    assert!(!scope.runtime_closing().is_cancelled());
    child.harness.release_run(&run_id);
    assert!(scope.runtime_closing().is_cancelled());
    assert!(!scope.stopped().is_cancelled());
    assert!(scope.retain_runtime().is_none());
    drop(retained);
    scope.wait_idle().await;
    assert!(parent.native_context().is_ok());

    let (other, _) = parent
        .create_side_question(session("drop"), emitter())
        .await
        .unwrap();
    let scope = other.execution_scope.clone();
    drop(other);
    assert!(scope.runtime_closing().is_cancelled());
    assert!(!scope.stopped().is_cancelled());
}

#[tokio::test]
async fn cancelling_a_direct_tool_request_drops_a_noncooperative_hook() {
    for cancel in ["request", "session", "close"] {
        let (parent, _, _) = parent(FauxScript::new()).await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let notify = entered.clone();
        parent.inner.lock().unwrap().before_tool_call = Some(Arc::new(move |_, _| {
            notify.notify_one();
            Box::pin(std::future::pending())
        }));
        let call = serde_json::from_value(
            serde_json::json!({"id":"cancel-me","name":"read","arguments":{"path":"never-read"}}),
        )
        .unwrap();
        let signal = CancellationToken::new();
        let task = tokio::spawn({
            let parent = parent.clone();
            let signal = signal.clone();
            async move { parent.invoke_tool(call, signal).await }
        });
        entered.notified().await;
        match cancel {
            "session" => parent.set_session(session("switched")).await.unwrap(),
            "close" => parent.close().await.unwrap(),
            _ => signal.cancel(),
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(parent.get_tip_id().await.unwrap().is_none());
    }
}
