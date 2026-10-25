//! Pi's recovery boundary and cancellation semantics.
//!
//! `_prepareRun` runs `_checkCompaction(lastAssistant, false)` before every
//! prompt (`agent-session.ts:1886-1897`), so an overflowing assistant left
//! behind by the previous run is omitted durably and the compaction hooks see
//! the assistant's own retry intent as `willRetry`
//! (`agent-session.ts:3071-3075`, `:3121-3140`). A cancellation that lands
//! during the recovery compaction is reported as Pi's cancelled retry
//! (`:1978`, `:1878`, `:3776-3786`), never as a clean finish.
use rpi_agent::AgentMessage;
use rpi_ai::providers::faux::{faux_assistant_message, FauxProvider, FauxScript, FauxStep};
use rpi_ai::types::StopReason;
use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use rpi_harness::session::{
    memory::{InMemorySessionStorage, SystemClock},
    session::Session,
    types::*,
    DefaultIdGenerator,
};
use rpi_harness::types::{AgentHarnessOptions, CompactionSettings, HarnessTool};
use rpi_tools::{create_write_tool, ExecutionToolContext, InMemoryExecutionEnv};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, Weak,
};

struct Hooks {
    cancel_compaction: bool,
    /// Decline the extension override so the host generates the summary and
    /// keeps its own retained tail, the way a session without a compaction
    /// extension behaves.
    decline_compaction: bool,
    trace: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
}

#[async_trait::async_trait]
impl rpi_ai::ProviderHooks for Hooks {
    async fn lifecycle(
        &self,
        event: &str,
        data: serde_json::Value,
        _: rpi_ai::identity::RequestIdentity,
        signal: tokio_util::sync::CancellationToken,
    ) -> Result<Option<serde_json::Value>, String> {
        self.trace
            .lock()
            .unwrap()
            .push((event.into(), data.clone()));
        if event == "session_before_compact" {
            if self.cancel_compaction {
                // The abort arrives while the recovery compaction is running.
                signal.cancel();
                return Ok(None);
            }
            if self.decline_compaction {
                return Ok(None);
            }
            return Ok(Some(serde_json::json!({"compaction":{
                "summary":"durable summary", "tokensBefore":300,
                "retainedTail":[], "usage":null, "details":null
            }})));
        }
        Ok(None)
    }
}

type Trace = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

/// Queues one steering message after the first finished turn so the same run
/// produces a second assistant message — the only way a turn can leave two
/// messages behind a compaction boundary without tool results (a tool result
/// would trip the host's mid-loop threshold valve instead).
struct SteerOnce {
    harness: Mutex<Weak<AgentHarness>>,
    sent: AtomicBool,
}

impl SteerOnce {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            harness: Mutex::new(Weak::new()),
            sent: AtomicBool::new(false),
        })
    }
}

impl rpi_agent::AgentEmitter for SteerOnce {
    fn emit(&self, event: rpi_agent::AgentEvent) -> futures::future::BoxFuture<'static, ()> {
        let harness = self.harness.lock().unwrap().upgrade();
        let steer = matches!(
            &event,
            rpi_agent::AgentEvent::TurnEnd {
                message: AgentMessage::Assistant(_),
                ..
            }
        ) && !self.sent.swap(true, Ordering::SeqCst);
        Box::pin(async move {
            let (Some(harness), true) = (harness, steer) else {
                return;
            };
            let message = AgentMessage::User(rpi_ai::types::UserMessage::new("keep going", 1));
            let options = rpi_harness::types::SendMessageOptions {
                trigger_turn: None,
                deliver_as: rpi_harness::types::MessageDelivery::Steer,
            };
            let _ = harness.send_message(message, options).await;
        })
    }
}

async fn setup(
    steps: Vec<FauxStep>,
    cancel_compaction: bool,
) -> (Arc<AgentHarness>, Arc<FauxProvider>, Trace) {
    setup_with_tools(steps, cancel_compaction, false, false, None).await
}

/// Same harness, optionally with the write tool so a scripted turn can leave
/// several messages behind the recovery boundary.
async fn setup_with_tools(
    steps: Vec<FauxStep>,
    cancel_compaction: bool,
    with_write_tool: bool,
    decline_compaction: bool,
    steer: Option<Arc<SteerOnce>>,
) -> (Arc<AgentHarness>, Arc<FauxProvider>, Trace) {
    let script = FauxScript::new();
    script.set_responses(steps);
    let provider = FauxProvider::new(script);
    let mut model = provider.default_model().clone();
    model.context_window = 120;
    let storage = Arc::new(InMemorySessionStorage::new(
        SessionMetadata {
            id: "recovery-boundary".into(),
            created_at: 0,
            parent_session_id: None,
        },
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    ));
    let trace: Trace = Arc::new(Mutex::new(Vec::new()));
    let env = Arc::new(InMemoryExecutionEnv::new());
    let tools: Vec<HarnessTool> = if with_write_tool {
        vec![HarnessTool::new(create_write_tool(
            &ExecutionToolContext::new(env.clone(), Some(env)),
        ))]
    } else {
        Vec::new()
    };
    let options = AgentHarnessOptions {
        model,
        thinking_level: Default::default(),
        active_tool_names: if tools.is_empty() {
            Vec::new()
        } else {
            vec!["write".into()]
        },
        tools,
        system_prompt: None,
        resources: Default::default(),
        stream_options: Default::default(),
        retry: Default::default(),
        compaction: CompactionSettings {
            enabled: true,
            reserve_tokens: 20,
            keep_recent_tokens: 1,
        },
        steering_mode: Default::default(),
        follow_up_mode: Default::default(),
        tool_execution: Default::default(),
        drive: Default::default(),
        session: Session::new(storage, None),
        allow_existing_session: false,
        models: vec![provider.clone()],
        to_provider_messages: None,
        entry_projectors: Default::default(),
        agent_emitter: steer
            .as_ref()
            .map(|steer| steer.clone() as Arc<dyn rpi_agent::AgentEmitter>),
        before_tool_call: None,
        after_tool_call: None,
        transform_context: None,
        entry_transforms: Vec::new(),
        provider_hooks: Some(Arc::new(Hooks {
            cancel_compaction,
            decline_compaction,
            trace: trace.clone(),
        })),
    };
    let harness = Arc::new(AgentHarness::create(options).await.unwrap());
    if let Some(steer) = steer {
        *steer.harness.lock().unwrap() = Arc::downgrade(&harness);
    }
    (harness, provider, trace)
}

fn overflow() -> FauxStep {
    let mut message = faux_assistant_message("failed content", StopReason::Error);
    // Faux's default timestamp is 1, predating the real compaction clock.
    message.timestamp = i64::MAX - 1;
    message.error_message = Some("prompt is too long: context length exceeded".into());
    FauxStep::message(message)
}

fn tool_result(text: &str) -> AgentMessage {
    AgentMessage::ToolResult(Box::new(rpi_ai::types::ToolResultMessage {
        structured_content: None,
        role: rpi_ai::types::ToolResultRole,
        tool_call_id: "call-1".into(),
        tool_name: "read".into(),
        content: vec![rpi_ai::types::Content::text(text)],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        is_error: false,
        timestamp: 0,
    }))
}

/// Seed the transcript a previous run would have left behind: a prompt the
/// model answered with an overflow error.
async fn seed_overflowed_turn(harness: &AgentHarness, trailing: Vec<AgentMessage>) -> String {
    let view = harness.session().view("main");
    view.append_message(AgentMessage::User(rpi_ai::types::UserMessage::new(
        "old question ".repeat(300),
        1,
    )))
    .await
    .unwrap();
    let mut failed = faux_assistant_message("failed content", StopReason::Error);
    failed.timestamp = i64::MAX - 1;
    failed.error_message = Some("prompt is too long: context length exceeded".into());
    view.append_message(AgentMessage::Assistant(Box::new(failed)))
        .await
        .unwrap();
    let assistant_id = harness.session().get_leaf_id().await.unwrap().unwrap();
    for message in trailing {
        view.append_message(message).await.unwrap();
    }
    assistant_id
}

async fn entries(harness: &AgentHarness) -> Vec<Entry> {
    harness
        .session()
        .view("main")
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .unwrap()
}

fn omissions(entries: &[Entry]) -> Vec<ContextEditEntry> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::ContextEdit(edit) if edit.replacement.is_null() => Some(edit.clone()),
            _ => None,
        })
        .collect()
}

async fn entries_contain(harness: &AgentHarness, needle: &str) -> bool {
    entries(harness)
        .await
        .iter()
        .any(|entry| serde_json::to_string(entry).unwrap().contains(needle))
}

async fn call_count(provider: &FauxProvider) -> usize {
    provider
        .state()
        .call_count
        .load(std::sync::atomic::Ordering::SeqCst)
}

/// H2: the boundary check must honour `Run { will_retry }` — persist the
/// omission before preparing the summary and tell the hooks `willRetry: true`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_prompt_boundary_omits_the_overflowed_assistant_before_the_prompt() {
    let (harness, provider, trace) = setup(vec![FauxStep::text("recovered")], false).await;
    let assistant_id = seed_overflowed_turn(&harness, Vec::new()).await;

    let result = harness.prompt_text("next question", vec![]).await.unwrap();
    assert!(
        matches!(result.outcome, HarnessRunOutcome::Completed { .. }),
        "{:?}",
        result.outcome
    );

    let entries = entries(&harness).await;
    let edits = omissions(&entries);
    assert_eq!(edits.len(), 1, "{entries:#?}");
    assert_eq!(edits[0].target_id, assistant_id);

    // The omission lands before the new prompt, and after its target: Pi
    // appends it inside `_prepareRun`, before the prompt is sent.
    let position =
        |predicate: &dyn Fn(&Entry) -> bool| entries.iter().position(|entry| predicate(entry));
    let assistant_at = position(&|entry| entry.id() == assistant_id).unwrap();
    let edit_at = position(&|entry| matches!(entry, Entry::ContextEdit(_))).unwrap();
    let prompt_at = position(&|entry| {
        serde_json::to_string(entry)
            .unwrap()
            .contains("next question")
    })
    .unwrap();
    assert!(assistant_at < edit_at && edit_at < prompt_at);

    assert_eq!(call_count(&provider).await, 1);
    let trace = trace.lock().unwrap();
    let before: Vec<_> = trace
        .iter()
        .filter(|(event, _)| event == "session_before_compact")
        .collect();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].1["willRetry"], true);
    // Pi's payload is exactly this key set (`agent-session.ts:3126-3140`):
    // `automatic` only exists on `compaction_start`, and a failure is reported
    // through `errorMessage` alone, never an extra `error` field
    // (`:3262-3270`).
    let mut keys: Vec<_> = before[0]
        .1
        .as_object()
        .expect("session_before_compact payload")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "branchEntries",
            "customInstructions",
            "preparation",
            "reason",
            "willRetry"
        ]
    );
    let compact: Vec<_> = trace
        .iter()
        .filter(|(event, _)| event == "session_compact")
        .collect();
    assert_eq!(compact.len(), 1);
    assert_eq!(compact[0].1["willRetry"], true);
}

/// The recovery omission also covers the tool results that belong to the
/// overflowed assistant (host divergence documented on
/// `recovery_omission_targets`; Pi receives them from the live run instead).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_prompt_boundary_omits_the_tool_results_that_follow_the_assistant() {
    let (harness, _, _) = setup(vec![FauxStep::text("recovered")], false).await;
    let assistant_id =
        seed_overflowed_turn(&harness, vec![tool_result("first"), tool_result("second")]).await;

    harness.prompt_text("next question", vec![]).await.unwrap();

    let entries = entries(&harness).await;
    let edits = omissions(&entries);
    let tool_ids: Vec<String> = entries
        .iter()
        .filter(|entry| matches!(entry.as_message(), Some(AgentMessage::ToolResult(_))))
        .map(|entry| entry.id().to_string())
        .collect();
    assert_eq!(tool_ids.len(), 2);
    assert_eq!(
        edits
            .iter()
            .map(|edit| edit.target_id.clone())
            .collect::<Vec<_>>(),
        std::iter::once(assistant_id)
            .chain(tool_ids)
            .collect::<Vec<_>>()
    );
}

/// M2: an abort that lands while the recovery compaction runs is Pi's
/// cancelled retry, not a normal finish and not a recovery failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_during_recovery_compaction_is_a_cancelled_retry() {
    let (harness, provider, trace) =
        setup(vec![overflow(), FauxStep::text("must not run")], true).await;

    let result = harness
        .prompt_text(&"question ".repeat(300), vec![])
        .await
        .unwrap();

    assert!(
        matches!(result.outcome, HarnessRunOutcome::Aborted { .. }),
        "{:?}",
        result.outcome
    );
    // The summarizer ran once (the extension declined, so the host generated
    // the summary itself); the cancelled retry never reached the provider.
    assert_eq!(call_count(&provider).await, 2);
    assert!(!entries_contain(&harness, "must not run").await);
    let entries = entries(&harness).await;
    // Pi omits before it compacts, so the omission stays durable.
    assert_eq!(omissions(&entries).len(), 1);
    assert!(!entries
        .iter()
        .any(|entry| matches!(entry, Entry::Compaction(_))));

    let trace = trace.lock().unwrap();
    let failed: Vec<_> = trace
        .iter()
        .filter(|(event, _)| event == "session_compact_failed")
        .collect();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].1["aborted"], true);
    assert_eq!(failed[0].1["willRetry"], false);
    // Pi leaves `errorMessage` undefined for an aborted compaction
    // (`agent-session.ts:3245-3268`).
    assert!(
        failed[0].1.get("errorMessage").is_none(),
        "{:?}",
        failed[0].1
    );
}

/// The failed attempt is persisted exactly once: recovery rewrites the next
/// generation's slice, so the omitted assistant must not be appended twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_persists_the_failed_attempt_exactly_once() {
    let (harness, _, _) = setup(vec![overflow(), FauxStep::text("recovered")], false).await;

    let result = harness
        .prompt_text(&"question ".repeat(300), vec![])
        .await
        .unwrap();
    assert!(
        matches!(result.outcome, HarnessRunOutcome::Completed { .. }),
        "{:?}",
        result.outcome
    );

    let entries = entries(&harness).await;
    let messages: Vec<&AgentMessage> = entries
        .iter()
        .filter_map(|entry| entry.as_message())
        .collect();
    assert_eq!(messages.len(), 3, "{entries:#?}");
    assert!(matches!(messages[0], AgentMessage::User(_)));
    let body = |message: &AgentMessage| serde_json::to_string(message).unwrap();
    assert!(body(messages[1]).contains("failed content"));
    assert!(body(messages[2]).contains("recovered"));
    assert_eq!(omissions(&entries).len(), 1);
}

/// M1: a successful recovery compaction must not suppress the post-run
/// `_checkCompaction` — Pi re-checks after every agent run
/// (`_handlePostAgentRun` → `_checkCompaction(message, true, toolResults)`,
/// `agent-session.ts:1974-1978`) and only uses `_overflowRecoveryAttempted` to
/// refuse a *second* overflow recovery, never to skip the threshold check.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_run_compaction_still_runs_after_a_successful_recovery() {
    // The steering message makes the retried run produce a second assistant
    // message, so the finished turn still has something to summarize once the
    // recovery compaction has already run.
    let mut recovered = faux_assistant_message("recovered ".repeat(200), StopReason::Stop);
    recovered.timestamp = i64::MAX - 1;
    let (harness, provider, trace) = setup_with_tools(
        vec![
            overflow(),
            FauxStep::text("compacted summary of the overflowed turn"),
            FauxStep::message(recovered),
            FauxStep::text("done"),
        ],
        false,
        false,
        true,
        Some(SteerOnce::new()),
    )
    .await;

    let result = harness
        .prompt_text(&"question ".repeat(300), vec![])
        .await
        .unwrap();
    assert!(
        matches!(result.outcome, HarnessRunOutcome::Completed { .. }),
        "{:?}",
        result.outcome
    );
    let trace = trace.lock().unwrap();
    let before: Vec<_> = trace
        .iter()
        .filter(|(event, _)| event == "session_before_compact")
        .collect();
    assert_eq!(before.len(), 2, "{trace:#?}");
    assert_eq!(before[0].1["willRetry"], true);
    // The finished turn is over the window with `stopReason: "stop"`, so Pi
    // compacts without asking the agent to continue
    // (`agent-session.ts:3066-3084`).
    assert_eq!(before[1].1["willRetry"], false);
    drop(trace);
    // overflow + summary + the two steered turns, then the post-run summary.
    assert_eq!(call_count(&provider).await, 4);
    assert_eq!(
        entries(&harness)
            .await
            .iter()
            .filter(|entry| matches!(entry, Entry::Compaction(_)))
            .count(),
        2
    );
}
