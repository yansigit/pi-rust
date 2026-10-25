//! Exactly one compact-and-continue invocation; the failed generation remains
//! auditable, but durable context edits exclude it from future provider input.
use rpi_ai::providers::faux::{faux_assistant_message, FauxProvider, FauxScript, FauxStep};
use rpi_ai::types::StopReason;
use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use rpi_harness::session::{
    memory::{InMemorySessionStorage, SystemClock},
    session::Session,
    types::*,
    DefaultIdGenerator,
};
use rpi_harness::types::{AgentHarnessOptions, CompactionSettings};
use std::sync::{Arc, Mutex};

struct Hooks {
    trace: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
}
#[async_trait::async_trait]
impl rpi_ai::ProviderHooks for Hooks {
    async fn lifecycle(
        &self,
        event: &str,
        data: serde_json::Value,
        _: rpi_ai::identity::RequestIdentity,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<Option<serde_json::Value>, String> {
        self.trace
            .lock()
            .unwrap()
            .push((event.into(), data.clone()));
        if event == "session_before_compact" {
            return Ok(Some(serde_json::json!({"compaction":{
                "summary":"durable summary", "tokensBefore":300,"retainedTail":[], "usage":null,"details":null
            }})));
        }
        Ok(None)
    }
}
async fn setup(
    steps: Vec<FauxStep>,
) -> (
    Arc<AgentHarness>,
    Arc<FauxProvider>,
    Arc<Mutex<Vec<(String, serde_json::Value)>>>,
) {
    let script = FauxScript::new();
    script.set_responses(steps);
    let provider = FauxProvider::new(script);
    let mut model = provider.default_model().clone();
    model.context_window = 120;
    let storage = Arc::new(InMemorySessionStorage::new(
        SessionMetadata {
            id: "overflow".into(),
            created_at: 0,
            parent_session_id: None,
        },
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    ));
    let mut options = AgentHarnessOptions {
        model,
        thinking_level: Default::default(),
        active_tool_names: Vec::new(),
        tools: Vec::new(),
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
        agent_emitter: None,
        before_tool_call: None,
        after_tool_call: None,
        transform_context: None,
        entry_transforms: Vec::new(),
        provider_hooks: None,
    };
    options.models = vec![provider.clone()];
    options.compaction = CompactionSettings {
        enabled: true,
        reserve_tokens: 20,
        keep_recent_tokens: 1,
    };
    options.retry.enabled = false;
    let trace = Arc::new(Mutex::new(Vec::new()));
    options.provider_hooks = Some(Arc::new(Hooks {
        trace: trace.clone(),
    }));
    (
        Arc::new(AgentHarness::create(options).await.unwrap()),
        provider,
        trace,
    )
}
fn overflow() -> FauxStep {
    let mut message = faux_assistant_message("failed content", StopReason::Error);
    // Faux's default timestamp is 1, predating the real compaction clock.
    message.timestamp = i64::MAX - 1;
    message.error_message = Some("prompt is too long: context length exceeded".into());
    FauxStep::message(message)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overflow_omission_is_durable_and_continues_once() {
    let (harness, provider, trace) = setup(vec![overflow(), FauxStep::text("recovered")]).await;
    let result = harness
        .prompt_text(&"question ".repeat(300), vec![])
        .await
        .unwrap();
    assert!(
        matches!(result.outcome, HarnessRunOutcome::Completed { .. }),
        "{:?}",
        result.outcome
    );
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    let entries = harness
        .session()
        .view("main")
        .find_entries(&Default::default())
        .await
        .unwrap();
    let edits: Vec<_> = entries
        .iter()
        .filter_map(|entry| {
            if let Entry::ContextEdit(edit) = entry {
                Some(edit)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(edits.len(), 1);
    assert!(edits[0].replacement.is_null());
    assert!(entries
        .iter()
        .any(|entry| entry.id() == edits[0].target_id && entry.as_message().is_some()));
    let trace = trace.lock().unwrap();
    let before: Vec<_> = trace
        .iter()
        .filter(|(event, _)| event == "session_before_compact")
        .collect();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].1["willRetry"], true);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_overflow_exhausts_without_a_third_call_or_second_summary() {
    let (harness, provider, trace) =
        setup(vec![overflow(), overflow(), FauxStep::text("must not run")]).await;
    let result = harness
        .prompt_text(&"question ".repeat(300), vec![])
        .await
        .unwrap();
    match result.outcome {
        HarnessRunOutcome::Failed { error, .. } => {
            assert_eq!(error.message,"Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model.");
            // Exhaustion is not a compaction failure: the run skipped the
            // second recovery (`agent-session.ts:3047-3062`) and reports it.
            assert_eq!(error.code, "overflow_recovery_exhausted");
        }
        outcome => panic!("unexpected {outcome:?}"),
    }
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    let trace = trace.lock().unwrap();
    assert_eq!(
        trace
            .iter()
            .filter(|(event, _)| event == "session_before_compact")
            .count(),
        1
    );
    let failed = trace
        .iter()
        .find(|(event, _)| event == "session_compact_failed")
        .unwrap();
    assert_eq!(failed.1["willRetry"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_truncated_response_has_the_exact_length_exhaustion_error() {
    let truncated = || {
        let mut message = faux_assistant_message("partial", StopReason::Length);
        message.timestamp = i64::MAX - 1;
        message.usage.output = 1;
        FauxStep::message(message)
    };
    let (harness, provider, _) = setup(vec![
        truncated(),
        truncated(),
        FauxStep::text("must not run"),
    ])
    .await;
    let result = harness
        .prompt_text(&"question ".repeat(300), vec![])
        .await
        .unwrap();
    match result.outcome {
        HarnessRunOutcome::Failed { error, .. } => assert_eq!(
            error.message,
            "Truncated response recovery failed after one compact-and-retry attempt."
        ),
        outcome => panic!("unexpected {outcome:?}"),
    }
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
}
