//! Pre-prompt compaction sees only prior finalized history; accepted input
//! is already recorded in the operation intent before a failing boundary.

use std::sync::Arc;

use rpi_ai::providers::faux::{FauxProvider, FauxScript, FauxStep};

use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use rpi_harness::session::session::Session;
use rpi_harness::session::sqlite::SqliteSessionRepo;
use rpi_harness::session::types::{SessionRepo, SessionStorage};
use rpi_harness::types::{AgentHarnessOptions, CompactionSettings};

async fn harness(
    script: FauxScript,
    compaction: CompactionSettings,
) -> (
    Arc<AgentHarness>,
    Arc<FauxProvider>,
    SqliteSessionRepo,
    tempfile::TempDir,
) {
    let provider = FauxProvider::new(script);
    let mut model = provider.default_model().clone();
    model.context_window = 120;
    let dir = tempfile::tempdir().unwrap();
    let repo = SqliteSessionRepo::open(dir.path().join("preprompt.db")).unwrap();
    let storage = repo.create(&Default::default()).await.unwrap();
    let options = AgentHarnessOptions {
        model,
        thinking_level: Default::default(),
        active_tool_names: Vec::new(),
        tools: Vec::new(),
        system_prompt: None,
        resources: Default::default(),
        stream_options: Default::default(),
        retry: rpi_harness::types::RetryPolicy {
            base_delay_ms: 1,
            ..Default::default()
        },
        compaction,
        steering_mode: Default::default(),
        follow_up_mode: Default::default(),
        tool_execution: Default::default(),
        drive: Default::default(),
        session: Session::new(Arc::new(storage), None),
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
    let harness = Arc::new(AgentHarness::create(options).await.expect("harness"));
    (harness, provider, repo, dir)
}

fn settings() -> CompactionSettings {
    CompactionSettings {
        enabled: true,
        reserve_tokens: 20,
        keep_recent_tokens: 1,
    }
}

struct Hooks {
    abort: bool,
    payloads: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
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
        if event == "session_before_compact" {
            self.payloads.lock().unwrap().push(data);
            if self.abort {
                signal.cancel();
            }
            return Ok(Some(serde_json::json!({"compaction":{
                "summary":"previous context only","tokensBefore":300,"retainedTail":[],"usage":null,"details":null
            }})));
        }
        Ok(None)
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_prompt_is_not_compaction_input_and_remains_admitted_on_abort() {
    use rpi_agent::AgentMessage;
    use rpi_harness::session::types::{LaneRecord, OperationIntent};
    for abort in [false, true] {
        let script = FauxScript::new();
        script.set_responses(vec![FauxStep::text("next answer")]);
        let (harness, provider, repo, _dir) = harness(script, settings()).await;
        let view = harness.session().view("main");
        view.append_message(AgentMessage::User(rpi_ai::types::UserMessage::new(
            "old question ".repeat(300),
            1,
        )))
        .await
        .unwrap();
        let mut old = rpi_ai::providers::faux::faux_assistant_message(
            "old answer",
            rpi_ai::types::StopReason::Stop,
        );
        old.usage.input = 200;
        view.append_message(AgentMessage::Assistant(Box::new(old)))
            .await
            .unwrap();
        let payloads = Arc::new(std::sync::Mutex::new(Vec::new()));
        harness
            .set_provider_hooks(Some(Arc::new(Hooks {
                abort,
                payloads: payloads.clone(),
            })))
            .await
            .unwrap();
        let marker = "NEW_PROMPT_MUST_NOT_BE_SUMMARIZED";
        let result = harness.prompt_text(marker, vec![]).await.unwrap();
        let payloads = payloads.lock().unwrap();
        assert_eq!(payloads.len(), 1);
        assert!(!payloads[0]["preparation"].to_string().contains(marker));
        assert!(!payloads[0]["branchEntries"].to_string().contains(marker));
        // Reopen authoritative storage, not just the live in-memory handle.
        let reopened = repo
            .open(&harness.session().storage().metadata())
            .await
            .unwrap();
        let records = reopened.find_records(&Default::default()).await.unwrap();
        assert!(records.iter().any(|record| matches!(record,LaneRecord::OperationStarted(started)
            if matches!(&started.intent,OperationIntent::Run{original_prompt,..} if serde_json::to_string(original_prompt).unwrap().contains(marker)))));
        let entries = view.find_entries(&Default::default()).await.unwrap();
        let prompt_present = entries
            .iter()
            .any(|entry| serde_json::to_string(entry).unwrap().contains(marker));
        if abort {
            assert!(matches!(result.outcome, HarnessRunOutcome::Failed { .. }));
            assert!(
                !prompt_present,
                "admitted, not yet placed in the transcript"
            );
            assert_eq!(
                provider
                    .state()
                    .call_count
                    .load(std::sync::atomic::Ordering::SeqCst),
                0
            );
        } else {
            assert!(matches!(
                result.outcome,
                HarnessRunOutcome::Completed { .. }
            ));
            assert!(prompt_present);
            assert_eq!(
                provider
                    .state()
                    .call_count
                    .load(std::sync::atomic::Ordering::SeqCst),
                1
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_transport_retry_precedes_overflow_recovery() {
    let mut transient =
        rpi_ai::providers::faux::faux_assistant_message("", rpi_ai::types::StopReason::Error);
    transient.error_message = Some("429 rate limit exceeded".into());
    transient.timestamp = i64::MAX - 1;
    let mut overflow = transient.clone();
    overflow.error_message = Some("prompt is too long: context length exceeded".into());
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::message(transient),
        FauxStep::message(overflow),
        FauxStep::text("recovered"),
    ]);
    let (harness, provider, _repo, _dir) = harness(script, settings()).await;
    let payloads = Arc::new(std::sync::Mutex::new(Vec::new()));
    harness
        .set_provider_hooks(Some(Arc::new(Hooks {
            abort: false,
            payloads: payloads.clone(),
        })))
        .await
        .unwrap();
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
        3
    );
    let payloads = payloads.lock().unwrap();
    assert_eq!(payloads.len(), 1, "only overflow invokes compaction");
    assert_eq!(payloads[0]["willRetry"], true);
    let records = harness
        .session()
        .find_records(&Default::default())
        .await
        .unwrap();
    assert!(records.iter().any(|record| matches!(
        record,
        rpi_harness::session::types::LaneRecord::RetryPending(_)
    )));
}
