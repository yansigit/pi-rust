//! Post-run automatic compaction: Pi checks the finished turn after persisting
//! it (`_handlePostAgentRun` → `_checkCompaction`). A single turn that crosses
//! the threshold without any tool call never reaches the mid-loop check, so this
//! is the only path that can compact it.

use std::sync::Arc;

use rpi_ai::providers::faux::{FauxProvider, FauxScript, FauxStep};

use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use rpi_harness::session::memory::{InMemorySessionStorage, SystemClock};
use rpi_harness::session::session::Session;
use rpi_harness::session::types::SessionMetadata;
use rpi_harness::session::DefaultIdGenerator;
use rpi_harness::types::{AgentHarnessOptions, CompactionSettings};

async fn harness(
    script: FauxScript,
    compaction: CompactionSettings,
) -> (Arc<AgentHarness>, Arc<FauxProvider>) {
    let provider = FauxProvider::new(script);
    let mut model = provider.default_model().clone();
    model.context_window = 120;
    let storage = Arc::new(InMemorySessionStorage::new(
        SessionMetadata {
            id: "life".into(),
            created_at: 0,
            parent_session_id: None,
        },
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    ));
    let options = AgentHarnessOptions {
        model,
        thinking_level: Default::default(),
        active_tool_names: Vec::new(),
        tools: Vec::new(),
        system_prompt: None,
        resources: Default::default(),
        stream_options: Default::default(),
        retry: Default::default(),
        compaction,
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
    let harness = Arc::new(AgentHarness::create(options).await.expect("harness"));
    (harness, provider)
}

fn settings() -> CompactionSettings {
    CompactionSettings {
        enabled: true,
        reserve_tokens: 20,
        keep_recent_tokens: 1,
    }
}

struct Hooks {
    mode: &'static str,
    trace: Arc<std::sync::Mutex<Vec<String>>>,
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
        if event.starts_with("session_") && event.contains("compact") {
            self.trace.lock().unwrap().push(event.into());
            if event == "session_before_compact" {
                assert_eq!(data["reason"], "overflow");
                assert_eq!(
                    data["willRetry"], false,
                    "completed overflow is not replayed"
                );
                if self.mode == "veto" {
                    return Ok(Some(serde_json::json!({"cancel":true})));
                }
                if self.mode == "abort" {
                    signal.cancel();
                }
                if self.mode != "default" {
                    return Ok(Some(serde_json::json!({"compaction": {
                        "summary":"summary", "tokensBefore":200, "retainedTail":[], "usage":null,"details":null
                    }})));
                }
            }
        }
        Ok(None)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_compaction_brackets_default_override_veto_and_abort() {
    for mode in ["default", "override", "veto", "abort"] {
        let script = FauxScript::new();
        let mut answer = rpi_ai::providers::faux::faux_assistant_message(
            "answer ".repeat(400),
            rpi_ai::types::StopReason::Stop,
        );
        answer.usage.input = 200;
        script.set_responses(vec![FauxStep::message(answer), FauxStep::text("summary")]);
        let (harness, _) = harness(script, settings()).await;
        let trace = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        harness
            .set_provider_hooks(Some(Arc::new(Hooks {
                mode,
                trace: trace.clone(),
            })))
            .await
            .unwrap();
        let observed = trace.clone();
        let mut listener = harness.events().watch(|| ());
        listener.start(Arc::new(move |event| {
            if let rpi_harness::events::HarnessEvent::CompactionProgress(event) = event {
                observed
                    .lock()
                    .unwrap()
                    .push(if event.active { "start" } else { "end" }.into());
            }
        }));
        let result = harness
            .prompt_text(&"question ".repeat(400), vec![])
            .await
            .unwrap();
        assert!(matches!(
            result.outcome,
            HarnessRunOutcome::Completed { .. }
        ));
        let expected = if matches!(mode, "veto" | "abort") {
            vec![
                "start",
                "session_before_compact",
                "end",
                "session_compact_failed",
            ]
        } else {
            vec!["start", "session_before_compact", "session_compact", "end"]
        };
        assert_eq!(*trace.lock().unwrap(), expected, "{mode}");
    }
}
