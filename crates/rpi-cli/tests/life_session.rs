//! Pi 1.3.0 session replacement lifecycle: `session_before_switch` /
//! `session_before_fork` (cancellable), `session_shutdown`, the durable swap,
//! then `session_start` — and the fresh session identity plugin tools see.

use std::sync::{Arc, Mutex};

use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_extensions::{ExtensionRegistry, ExtensionSession, PluginKeepalive};
use rpi_harness::agent_harness::AgentHarness;
use rpi_harness::session::memory::{InMemorySessionStorage, SystemClock};
use rpi_harness::session::session::Session;
use rpi_harness::session::types::SessionMetadata;
use rpi_harness::session::DefaultIdGenerator;
use rpi_harness::types::AgentHarnessOptions;
use rpi_plugin_sdk::{EventTag, StablePluginEvent};

#[derive(Clone)]
struct Recorder {
    events: Arc<Mutex<Vec<String>>>,
    veto: Option<EventTag>,
}

static CONTEXTS: rpi_plugin_sdk::handles::ToolHandles<Recorder> =
    rpi_plugin_sdk::handles::ToolHandles::new();

struct ContextOwner(usize);
impl Drop for ContextOwner {
    fn drop(&mut self) {
        drop(CONTEXTS.remove(self.0 as rpi_plugin_sdk::StepHandle));
    }
}

extern "C" fn record(event: StablePluginEvent, data: *mut std::ffi::c_void) -> i32 {
    let recorder = CONTEXTS.get(data).expect("live test context").lock().clone();
    recorder
        .events
        .lock()
        .unwrap()
        .push(format!("{:?}", event.tag));
    if recorder.veto == Some(event.tag) {
        rpi_plugin_sdk::EVENT_HANDLER_ABORT
    } else {
        0
    }
}

fn extension_session(veto: Option<EventTag>) -> (ExtensionSession, Arc<Mutex<Vec<String>>>) {
    let recorder = Recorder {
        events: Arc::new(Mutex::new(Vec::new())),
        veto,
    };
    let events = recorder.events.clone();
    let context = CONTEXTS.insert(recorder);
    // Leaked deliberately for the duration of one test: the registry keeps the
    // raw context pointer, exactly like a loaded plugin's user_data.
    let _owner: &'static ContextOwner = Box::leak(Box::new(ContextOwner(context as usize)));
    let mut registry = ExtensionRegistry::new();
    for tag in [
        EventTag::SessionBeforeSwitch,
        EventTag::SessionBeforeFork,
        EventTag::SessionShutdown,
        EventTag::SessionStart,
    ] {
        registry.register_event_handler("lifecycle-fixture".into(), tag, record, context);
    }
    let snapshot = Arc::new(registry.snapshot());
    let session = ExtensionSession::from_parts(
        snapshot,
        Arc::new(PluginKeepalive::new(Vec::new(), None)),
        Vec::new(),
        None,
    );
    (session, events)
}

async fn harness(id: &str) -> Arc<AgentHarness> {
    let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
    let storage = Arc::new(InMemorySessionStorage::new(
        SessionMetadata {
            id: id.into(),
            created_at: 0,
            parent_session_id: None,
        },
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    ));
    let options = AgentHarnessOptions {
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
        session: Session::new(storage, None),
        allow_existing_session: false,
        models: vec![provider],
        to_provider_messages: None,
        entry_projectors: Default::default(),
        agent_emitter: None,
        before_tool_call: None,
        after_tool_call: None,
        transform_context: None,
        entry_transforms: Vec::new(),
        provider_hooks: None,
    };
    Arc::new(AgentHarness::create(options).await.expect("harness"))
}

fn session(id: &str) -> Session {
    let storage = Arc::new(InMemorySessionStorage::new(
        SessionMetadata {
            id: id.into(),
            created_at: 0,
            parent_session_id: None,
        },
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    ));
    Session::new(storage, None)
}

async fn session_id(harness: &AgentHarness) -> String {
    harness
        .session()
        .storage()
        .metadata()
        .id
        .clone()
}

#[tokio::test]
async fn replacement_dispatches_before_shutdown_swap_start() {
    let harness = harness("first").await;
    let (ext, events) = extension_session(None);
    let cell = Arc::new(Mutex::new(ext));
    let bridge = Arc::new(Mutex::new(None));
    let tool_context = rpi_extensions::ToolCallContext::new("/tmp");
    tool_context.set_session_id("first");
    let lane = harness.lane("main");
    let outcome = rpi_cli::session::replace_active_session(
        &rpi_cli::session::SessionReplacementContext {
            harness: &harness,
            lane: lane.as_ref(),
            extension_session: &cell,
            action_bridge: &bridge,
            tool_context: &tool_context,
        },
        session("second"),
        rpi_cli::session::SessionReplacementReason::Resume,
        None,
    )
    .await
    .unwrap();
    assert_eq!(outcome, rpi_cli::session::SessionReplacement::Replaced);
    assert_eq!(
        *events.lock().unwrap(),
        vec!["SessionBeforeSwitch", "SessionShutdown", "SessionStart"]
    );
    assert_eq!(session_id(&harness).await, "second");
    // The plugin tool channel names the session now being served.
    assert_eq!(tool_context.session_id().as_deref(), Some("second"));
}

#[tokio::test]
async fn a_cancelled_before_switch_keeps_the_old_session() {
    let harness = harness("first").await;
    let (ext, events) = extension_session(Some(EventTag::SessionBeforeSwitch));
    let cell = Arc::new(Mutex::new(ext));
    let bridge = Arc::new(Mutex::new(None));
    let tool_context = rpi_extensions::ToolCallContext::new("/tmp");
    tool_context.set_session_id("first");
    let lane = harness.lane("main");
    let outcome = rpi_cli::session::replace_active_session(
        &rpi_cli::session::SessionReplacementContext {
            harness: &harness,
            lane: lane.as_ref(),
            extension_session: &cell,
            action_bridge: &bridge,
            tool_context: &tool_context,
        },
        session("second"),
        rpi_cli::session::SessionReplacementReason::New,
        None,
    )
    .await
    .unwrap();
    assert!(matches!(
        outcome,
        rpi_cli::session::SessionReplacement::Cancelled { .. }
    ));
    assert_eq!(*events.lock().unwrap(), vec!["SessionBeforeSwitch"]);
    assert_eq!(session_id(&harness).await, "first");
    assert_eq!(tool_context.session_id().as_deref(), Some("first"));
}

#[tokio::test]
async fn fork_uses_the_before_fork_hook() {
    let harness = harness("first").await;
    let (ext, events) = extension_session(None);
    let cell = Arc::new(Mutex::new(ext));
    let bridge = Arc::new(Mutex::new(None));
    let tool_context = rpi_extensions::ToolCallContext::new("/tmp");
    tool_context.set_session_id("first");
    let lane = harness.lane("main");
    let outcome = rpi_cli::session::replace_active_session(
        &rpi_cli::session::SessionReplacementContext {
            harness: &harness,
            lane: lane.as_ref(),
            extension_session: &cell,
            action_bridge: &bridge,
            tool_context: &tool_context,
        },
        session("forked"),
        rpi_cli::session::SessionReplacementReason::Fork,
        Some("leaf-1"),
    )
    .await
    .unwrap();
    assert_eq!(outcome, rpi_cli::session::SessionReplacement::Replaced);
    assert_eq!(
        *events.lock().unwrap(),
        vec!["SessionBeforeFork", "SessionShutdown", "SessionStart"]
    );
    assert_eq!(session_id(&harness).await, "forked");
}
