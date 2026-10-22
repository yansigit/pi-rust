//! Post-run automatic compaction: Pi checks the finished turn after persisting
//! it (`_handlePostAgentRun` → `_checkCompaction`). A single turn that crosses
//! the threshold without any tool call never reaches the mid-loop check, so this
//! is the only path that can compact it.

use std::sync::Arc;

use rpi_agent::{AgentMessage, AgentMessageRole};
use rpi_ai::providers::faux::{FauxProvider, FauxScript, FauxStep};
use rpi_ai::types::{Content, UserMessage, UserContent};
use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use rpi_harness::session::memory::{InMemorySessionStorage, SystemClock};
use rpi_harness::session::session::Session;
use rpi_harness::session::types::{Entry, SessionMetadata};
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_completed_turn_over_the_threshold_compacts_after_it_is_persisted() {
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::text("answer ".repeat(400)),
        FauxStep::text("## Goal\nsummary"),
    ]);
    let (harness, provider) = harness(script, settings()).await;
    let result = harness.prompt_text("question", vec![]).await.unwrap();
    assert!(
        matches!(result.outcome, HarnessRunOutcome::Completed { .. }),
        "{:?}",
        result.outcome
    );
    // One provider call for the turn, one for the summary.
    assert_eq!(
        provider.state().call_count.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    let entries = harness
        .session()
        .view("main")
        .find_entries(&Default::default())
        .await
        .unwrap();
    let compaction = entries
        .iter()
        .find_map(|entry| match entry {
            Entry::Compaction(compaction) => Some(compaction),
            _ => None,
        })
        .expect("post-run compaction entry");
    // The single turn is larger than the keep budget and there is no earlier
    // history, so Pi takes the split-turn path: a turn-prefix summary only.
    assert_eq!(
        compaction.summary,
        "No prior history.\n\n---\n\n**Turn Context (split turn):**\n\n## Goal\nsummary"
    );
    assert!(compaction.tokens_before > 0);
    // The compaction entry is the new branch tip, matching Pi reporting the
    // post-compaction position.
    let HarnessRunOutcome::Completed { leaf_id, .. } = &result.outcome else {
        unreachable!("checked above")
    };
    assert_eq!(&compaction.base.id, leaf_id);
    // The recent assistant answer survives in the retained tail.
    assert!(compaction.retained_tail.iter().any(|message| {
        matches!(
            message.role(),
            AgentMessageRole::Assistant
        ) && matches!(message, AgentMessage::Assistant(assistant) if assistant.content.iter().any(|block| matches!(block, Content::Text(text) if text.text.starts_with("answer"))))
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accepted_user_message_is_never_dropped_by_the_post_run_check() {
    let script = FauxScript::new();
    script.set_responses(vec![FauxStep::text("short answer")]);
    let (harness, provider) = harness(script, settings()).await;
    harness
        .append_message(AgentMessage::User(UserMessage::new(
            UserContent::Text("earlier".into()),
            1,
        )))
        .await
        .unwrap();
    let result = harness.prompt_text("question", vec![]).await.unwrap();
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    // No compaction was needed and nothing else was requested from the provider.
    assert_eq!(
        provider.state().call_count.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    let entries = harness
        .session()
        .view("main")
        .find_entries(&Default::default())
        .await
        .unwrap();
    assert!(!entries
        .iter()
        .any(|entry| matches!(entry, Entry::Compaction(_))));
}
