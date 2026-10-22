//! Integration coverage for the tree-navigation UI caller.
//!
//! The `/tree` dialogs ultimately call the same runtime action a plugin uses
//! (`extensions_actions.rs` → `AgentLane::navigate_tree_with`). These tests
//! drive that public action with the option combinations the dialogs produce
//! and assert the hooks Pi defines for a tree navigation, including the
//! terminal `session_tree_failed` event for a cancelled or failed tree.
//!
//! The ten Pi 1.3.0 traces themselves are covered by
//! `rpi-harness/tests/tree_reference.rs`; this file covers the UI-facing caller
//! contract around them.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use rpi_agent::message::CustomMessage;
use rpi_agent::AgentMessage;
use rpi_ai::identity::RequestIdentity;
use rpi_ai::providers::faux::{faux_assistant_message, FauxProvider, FauxScript};
use rpi_ai::types::{Content, StopReason, UserContent, UserMessage};
use rpi_ai::{Provider, ProviderHooks};
use rpi_cli::extensions_actions::HarnessActionHost;
use rpi_extensions::RuntimeActionHost;
use rpi_harness::agent_harness::AgentHarness;
use rpi_harness::session::memory::{InMemorySessionStorage, SystemClock};
use rpi_harness::session::types::{ProvisionedEntry, ProvisionedKind, SessionMetadata};
use rpi_harness::session::{DefaultIdGenerator, Session};
use rpi_harness::types::AgentHarnessOptions;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// Records hook payloads. The mode decides what `session_before_tree` returns,
/// mirroring the UI choices (summarize, no summary) and the terminal failures.
struct Hooks {
    mode: &'static str,
    events: Mutex<Vec<Value>>,
    failures: Mutex<Vec<Value>>,
}

#[async_trait::async_trait]
impl ProviderHooks for Hooks {
    async fn lifecycle(
        &self,
        event: &str,
        data: Value,
        _identity: RequestIdentity,
        signal: CancellationToken,
    ) -> Result<Option<Value>, String> {
        if event == "session_tree_failed" {
            self.failures.lock().unwrap().push(data);
            return Ok(None);
        }
        if event != "session_before_tree" {
            let mut recorded = json!({ "type": event });
            for (key, value) in data.as_object().unwrap() {
                recorded[key] = value.clone();
            }
            self.events.lock().unwrap().push(recorded);
            return Ok(None);
        }
        assert!(!signal.is_cancelled());
        let mut preparation = data["preparation"].clone();
        preparation["entriesToSummarize"] = preparation["entriesToSummarize"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].clone())
            .collect();
        self.events
            .lock()
            .unwrap()
            .push(json!({ "type": event, "preparation": preparation }));
        match self.mode {
            "cancel" => Ok(Some(json!({ "cancel": true }))),
            "hookError" => Err("fixture hook failure".into()),
            "extensionSummary" => Ok(Some(json!({
                "summary": { "summary": "retained summary", "details": { "fixture": true } },
                "label": "ui label",
            }))),
            _ => Ok(None),
        }
    }
}

fn provisioned(id: &str, message: AgentMessage) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.into(),
        kind: ProvisionedKind::Message {
            message,
            terminate: None,
        },
    }
}

fn user(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage::new(
        UserContent::Blocks(vec![Content::text(text)]),
        0,
    ))
}

fn assistant(text: &str) -> AgentMessage {
    AgentMessage::Assistant(Box::new(faux_assistant_message(text, StopReason::Stop)))
}

/// `root` with `left` (current leaf), `target`, `user` and a custom message.
async fn fixture(mode: &'static str) -> (Arc<AgentHarness>, Arc<Hooks>) {
    let hooks = Arc::new(Hooks {
        mode,
        events: Mutex::new(Vec::new()),
        failures: Mutex::new(Vec::new()),
    });
    let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
    let storage = Arc::new(InMemorySessionStorage::new(
        SessionMetadata {
            id: "tree-ui".into(),
            created_at: 0,
            parent_session_id: None,
        },
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    ));
    let session = Session::new(storage, None);
    let harness = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        thinking_level: Default::default(),
        active_tool_names: Vec::new(),
        tools: Vec::new(),
        system_prompt: None,
        resources: Default::default(),
        stream_options: Default::default(),
        retry: Default::default(),
        compaction: Default::default(),
        steering_mode: Default::default(),
        follow_up_mode: Default::default(),
        tool_execution: Default::default(),
        drive: Default::default(),
        session,
        models: vec![provider.clone() as Arc<dyn Provider>],
        to_provider_messages: None,
        entry_projectors: Default::default(),
        agent_emitter: None,
        before_tool_call: None,
        after_tool_call: None,
        transform_context: None,
        entry_transforms: Vec::new(),
        provider_hooks: Some(hooks.clone()),
        allow_existing_session: false,
    })
    .await
    .unwrap();
    let session = harness.session();
    session
        .append_entry(provisioned("root", user("root prompt")), "main")
        .await
        .unwrap();
    for entry in [
        provisioned("left", assistant("abandoned")),
        provisioned("target", assistant("target response")),
        provisioned("user", user("edit this")),
        provisioned(
            "custom",
            AgentMessage::Custom(CustomMessage::new(
                "custom",
                vec![Content::text("edit custom")],
                json!({ "customType": "fixture", "display": true }),
                0,
            )),
        ),
    ] {
        session.move_lane("main", Some("root")).await.unwrap();
        session.append_entry(entry, "main").await.unwrap();
    }
    session.move_lane("main", Some("left")).await.unwrap();
    (Arc::new(harness), hooks)
}

/// Build the runtime action host the TUI/extension caller uses.
async fn host(harness: Arc<AgentHarness>) -> Arc<HarnessActionHost> {
    let cwd = tempfile::tempdir().unwrap();
    let (host, cell) = HarnessActionHost::new_empty(
        Vec::new(),
        cwd.path().to_path_buf(),
        tokio::runtime::Handle::current(),
        BTreeMap::new(),
        rpi_extensions::ToolCallContext::new(cwd.path().to_string_lossy()),
    );
    HarnessActionHost::set_harness(&cell, harness);
    Arc::new(host)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_summary_navigation_reports_the_hooks_and_moves_the_leaf() {
    let (harness, hooks) = fixture("plain").await;
    let host = host(harness.clone()).await;
    let result = host
        .navigate_tree(json!({ "targetId": "target" }))
        .await
        .unwrap();
    assert_eq!(result["cancelled"], false);
    assert_eq!(result["status"], "completed");

    let events = hooks.events.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["type"], "session_before_tree");
    assert_eq!(events[0]["preparation"]["targetId"], "target");
    assert_eq!(events[0]["preparation"]["oldLeafId"], "left");
    assert_eq!(events[0]["preparation"]["userWantsSummary"], false);
    assert_eq!(
        events[0]["preparation"]["entriesToSummarize"],
        json!(["left"])
    );
    assert!(events[0]["preparation"].get("customInstructions").is_none());
    assert_eq!(events[1]["type"], "session_tree");
    assert_eq!(events[1]["newLeafId"], "target");
    assert!(hooks.failures.lock().unwrap().is_empty());

    assert_eq!(
        harness.session().get_leaf_id().await.unwrap().as_deref(),
        Some("target")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn summarize_with_custom_prompt_forwards_the_dialog_options() {
    let (harness, hooks) = fixture("extensionSummary").await;
    let host = host(harness.clone()).await;
    let result = host
        .navigate_tree(json!({
            "targetId": "target",
            "summarize": true,
            "customInstructions": "keep the API notes",
            "label": "ui label",
        }))
        .await
        .unwrap();
    assert_eq!(result["cancelled"], false);

    let events = hooks.events.lock().unwrap().clone();
    let preparation = &events[0]["preparation"];
    assert_eq!(preparation["userWantsSummary"], true);
    assert_eq!(preparation["customInstructions"], "keep the API notes");
    assert_eq!(preparation["label"], "ui label");

    // An extension-provided summary is committed at the target with its label;
    // the terminal event names it and marks the extension origin.
    let tree = &events[1];
    assert_eq!(tree["type"], "session_tree");
    assert_eq!(tree["fromExtension"], true);
    assert_eq!(tree["summaryEntry"]["summary"], "retained summary");
    assert_eq!(tree["summaryEntry"]["fromHook"], true);
    let leaf = harness.session().get_leaf_id().await.unwrap();
    assert_eq!(leaf.as_deref(), Some(tree["newLeafId"].as_str().unwrap()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_cancel_reports_declined_and_terminates_the_tree() {
    let (harness, hooks) = fixture("cancel").await;
    let host = host(harness.clone()).await;
    let result = host
        .navigate_tree(json!({ "targetId": "target" }))
        .await
        .unwrap();
    assert_eq!(result["cancelled"], true);
    assert_eq!(result["status"], "declined");
    assert_eq!(
        harness.session().get_leaf_id().await.unwrap().as_deref(),
        Some("left")
    );

    let failures = hooks.failures.lock().unwrap().clone();
    assert_eq!(
        failures.len(),
        1,
        "cancelled trees end with session_tree_failed"
    );
    assert_eq!(failures[0]["targetId"], "target");
    assert_eq!(failures[0]["cancelled"], true);
    assert_eq!(failures[0]["aborted"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_error_is_rejected_and_terminates_the_tree() {
    let (harness, hooks) = fixture("hookError").await;
    let host = host(harness.clone()).await;
    let error = host
        .navigate_tree(json!({ "targetId": "target" }))
        .await
        .unwrap_err();
    assert!(
        error.contains("fixture hook failure"),
        "unexpected error: {error}"
    );

    let failures = hooks.failures.lock().unwrap().clone();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["cancelled"], false);
    assert_eq!(failures[0]["error"], "fixture hook failure");
    assert_eq!(
        harness.session().get_leaf_id().await.unwrap().as_deref(),
        Some("left")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selecting_the_current_entry_is_a_no_op_before_the_hooks() {
    let (harness, hooks) = fixture("plain").await;
    let host = host(harness.clone()).await;
    let result = host
        .navigate_tree(json!({ "targetId": "left" }))
        .await
        .unwrap();
    assert_eq!(result["cancelled"], false);
    assert_eq!(result["status"], "completed");
    assert!(
        hooks.events.lock().unwrap().is_empty(),
        "Pi returns before session_before_tree for the current leaf"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_target_is_rejected_before_the_hooks() {
    let (harness, hooks) = fixture("plain").await;
    let host = host(harness.clone()).await;
    let error = host
        .navigate_tree(json!({ "targetId": "absent" }))
        .await
        .unwrap_err();
    assert!(
        error.contains("Entry absent not found"),
        "unexpected: {error}"
    );
    assert!(hooks.events.lock().unwrap().is_empty());
    assert!(hooks.failures.lock().unwrap().is_empty());
}
