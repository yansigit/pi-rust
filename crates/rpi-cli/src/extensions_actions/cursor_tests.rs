//! Opt-in acceptance of the installed native library and its real Node worker.
use super::*;
use rpi_ai::{Context, SimpleStreamOptions};
use rpi_harness::types::{AgentHarnessOptions, HarnessTool};
use serde_json::json;

#[cfg(target_os = "linux")]
fn node_children() -> std::collections::BTreeSet<String> {
    let mut children = std::collections::BTreeSet::new();
    for task in std::fs::read_dir("/proc/self/task").unwrap() {
        let Ok(task) = task else {
            continue;
        };
        let Ok(ids) = std::fs::read_to_string(task.path().join("children")) else {
            continue;
        };
        for id in ids.split_whitespace() {
            if std::fs::read_link(format!("/proc/{id}/exe"))
                .ok()
                .and_then(|path| path.file_name().map(|name| name == "node"))
                .unwrap_or(false)
            {
                children.insert(id.into());
            }
        }
    }
    children
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RPI_TEST_CURSOR_EXTENSION_DIR with the current DLL and staged runtime, Node 24+, and isolated RPI_CODING_AGENT_DIR"]
async fn installed_cursor_registers_commands_tools_and_streams_without_ambient_auth() {
    let directory = PathBuf::from(
        std::env::var_os("RPI_TEST_CURSOR_EXTENSION_DIR")
            .expect("set staged native Cursor directory"),
    );
    let agent_dir =
        std::env::var_os("RPI_CODING_AGENT_DIR").expect("use isolated rpi configuration");
    assert!(PathBuf::from(&agent_dir)
        .join("native-fixture-marker")
        .is_file());
    let cwd = tempfile::tempdir().unwrap();
    let session = crate::session::create_jsonl_session(cwd.path(), cwd.path().to_str().unwrap())
        .await
        .unwrap();
    let session_id = session.storage().metadata().id;
    let (host, cell) = HarnessActionHost::new_empty(
        vec![],
        cwd.path().into(),
        Handle::current(),
        BTreeMap::new(),
        Default::default(),
    );
    host.set_native_mode("print");
    host.set_native_project_trust(false, true);
    let bridge = rpi_extensions::ActionBridge::new(Handle::current(), Arc::new(host));
    let extensions = rpi_extensions::load_session(
        &[directory],
        Arc::new(rpi_extensions::NullDiagnostics),
        Some(bridge.clone()),
    );
    assert_eq!(
        extensions.loaded_paths().len(),
        1,
        "native Cursor failed to load"
    );
    let snapshot = extensions.snapshot_arc().unwrap();
    assert_eq!(snapshot.commands().len(), 10);
    assert!(
        snapshot
            .tools()
            .iter()
            .any(|tool| tool.tool.name == "cursor_ask_question"),
        "set PI_CURSOR_ASK_QUESTION=1 for the opt-in question-tool fixture"
    );
    assert!(snapshot
        .flags()
        .iter()
        .any(|flag| flag.name == "cursor-no-fast"));
    let providers = rpi_extensions::PluggableProvider::from_session(&extensions, Handle::current());
    assert_eq!(providers.len(), 1);
    let model = providers[0].models()[0].clone();
    let tools = snapshot
        .tools()
        .iter()
        .map(|tool| {
            HarnessTool::new(Arc::new(
                rpi_extensions::PluginToolAdapter::new(
                    tool.tool.clone(),
                    tool.handle(),
                    extensions.keepalive(),
                )
                .with_metadata(tool.metadata.clone()),
            ))
        })
        .collect();
    let harness = AgentHarness::create(AgentHarnessOptions {
        session,
        model: model.clone(),
        models: providers.clone(),
        tools,
        active_tool_names: snapshot
            .tools()
            .iter()
            .map(|tool| tool.tool.name.clone())
            .collect(),
        ..Default::default()
    })
    .await
    .unwrap();
    let inventory = harness.inspect_tools().unwrap();
    let question = inventory
        .iter()
        .find(|tool| tool["name"] == "cursor_ask_question")
        .unwrap();
    assert_eq!(question["label"], "Cursor question");
    assert_eq!(question["executionMode"], "sequential");
    assert_eq!(question["exposure"], "direct");
    assert!(question["promptSnippet"]
        .as_str()
        .unwrap()
        .contains("unless they asked"));
    assert_eq!(question["promptGuidelines"].as_array().unwrap().len(), 2);
    HarnessActionHost::set_harness(&cell, Arc::new(harness.clone()));
    let runtime = crate::provider_runtime::ProviderRuntime::new(
        &extensions,
        &bridge,
        providers.clone(),
        vec![],
    )
    .unwrap();
    runtime.activate();
    let authentication =
        rpi_extensions::provider_auth::NativeProviderAuth::from_session(&extensions, "cursor")
            .unwrap();
    use rpi_extensions::provider_auth::{AuthAction, AuthResponseKind};
    let login = authentication
        .request(
            AuthAction::Login,
            json!({"methodId":"api_key"}),
            Default::default(),
        )
        .await
        .unwrap();
    assert!(matches!(login.kind, AuthResponseKind::Interaction));
    let credential = authentication
        .request(
            AuthAction::Continue,
            json!({"continuation":login.payload["continuation"],"value":"native-test-fixture-key"}),
            Default::default(),
        )
        .await
        .unwrap();
    assert!(matches!(credential.kind, AuthResponseKind::Credential));
    assert_eq!(credential.payload["key"], "native-test-fixture-key");
    let identity =
        rpi_ai::identity::RequestIdentity::operation(Some(session_id.clone()), "session_start");
    let result = rpi_ai::identity::scope(
        identity,
        rpi_extensions::dispatch_lifecycle_event(
            &snapshot,
            rpi_plugin_sdk::EventTag::SessionStart,
            extensions.keepalive(),
        ),
    )
    .await;
    assert_eq!(result, None);
    let files = vec![rpi_harness::context_files::ContextFile {
        path: cwd.path().join("project&one/AGENTS.md"),
        content: "captured native instructions".into(),
    }];
    let original_prompt = format!(
        "native base{}",
        rpi_harness::context_files::format_project_context(&files)
    );
    let prompt_options = json!({"cwd":cwd.path(),"contextFiles":files.into_iter().map(rpi_harness::types::PromptContextFile::from).collect::<Vec<_>>(),"skills":[]});
    let hooks = rpi_extensions::ExtensionProviderHooks::from_session(&extensions).unwrap();
    let mut prompt_context = Context::new(vec![]);
    prompt_context.system_prompt = Some(original_prompt);
    let patch = rpi_ai::ProviderHooks::begin_operation_with_prompt_options(
        &hooks,
        &model,
        &prompt_context,
        &Default::default(),
        prompt_options,
    )
    .await
    .unwrap();
    assert_eq!(
        patch.system_prompt.as_deref(),
        Some("native base"),
        "the actual DLL must apply retained context deduplication to the host prompt"
    );
    let command = snapshot
        .commands()
        .iter()
        .find(|command| command.name == "cursor-mode")
        .unwrap();
    command
        .native
        .as_ref()
        .unwrap()
        .start(
            &extensions,
            &command.name,
            "plan".into(),
            Default::default(),
        )
        .unwrap()
        .wait()
        .await
        .unwrap();
    let entries = harness
        .session()
        .storage()
        .snapshot()
        .await
        .unwrap()
        .entries;
    assert!(entries.iter().any(|entry|matches!(entry,rpi_harness::session::types::Entry::Custom(entry) if entry.custom_type.contains("mode"))));
    let question = serde_json::from_value(json!({"id":"question-one","name":"cursor_ask_question","arguments":{"question":"offline question"}})).unwrap();
    let result = harness
        .invoke_tool(question, Default::default())
        .await
        .unwrap();
    assert!(result.is_error);
    assert!(
        serde_json::to_string(&result)
            .unwrap()
            .contains("UI is unavailable"),
        "{result:?}"
    );
    #[cfg(target_os = "linux")]
    let parent_processes = node_children();
    let child_session =
        crate::session::create_jsonl_session(cwd.path(), cwd.path().to_str().unwrap())
            .await
            .unwrap();
    let child = harness
        .create_child(
            child_session,
            Arc::new(rpi_agent::CollectorEmitter::new().0),
            1,
            None,
        )
        .await
        .unwrap();
    child.execution_scope.run(async {
        let question = serde_json::from_value(json!({"id":"child-question","name":"cursor_ask_question","arguments":{"question":"offline child question"}})).unwrap();
        let result = child.harness.invoke_tool(question,Default::default()).await.unwrap();
        assert!(result.is_error);
        assert!(serde_json::to_string(&result).unwrap().contains("UI is unavailable"),"{result:?}");
    }).await;
    #[cfg(target_os = "linux")]
    assert_eq!(
        node_children().difference(&parent_processes).count(),
        1,
        "child must own a fresh Node process"
    );
    child.harness.close().await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        child.execution_scope.wait_idle(),
    )
    .await
    .expect("child worker did not release its runtime lease");
    #[cfg(target_os = "linux")]
    assert_eq!(
        node_children(),
        parent_processes,
        "child must reap its own worker while retaining the parent"
    );
    command
        .native
        .as_ref()
        .unwrap()
        .start(
            &extensions,
            &command.name,
            "agent".into(),
            Default::default(),
        )
        .unwrap()
        .wait()
        .await
        .unwrap();
    for kind in [
        rpi_ai::provider::StreamKind::Full,
        rpi_ai::provider::StreamKind::Simple,
    ] {
        let options = SimpleStreamOptions {
            stream_kind: kind,
            authentication: rpi_ai::auth::ReplacementAuth::from_private_request(
                "cursor".into(),
                Default::default(),
            ),
            ..Default::default()
        };
        let identity =
            rpi_ai::identity::RequestIdentity::operation(Some(session_id.clone()), "prompt");
        let mut stream = rpi_ai::identity::scope(
            identity,
            providers[0].stream_simple(&model, &Context::new(vec![]), &options),
        )
        .await;
        while stream.next().await.is_some() {}
        let message = stream.result().await.unwrap();
        assert!(
            message
                .error_message
                .as_deref()
                .unwrap_or_default()
                .contains("API key"),
            "{message:?}"
        );
    }
    assert_eq!(
        rpi_extensions::dispatch_lifecycle_event(
            &snapshot,
            rpi_plugin_sdk::EventTag::SessionShutdown,
            extensions.keepalive()
        )
        .await,
        None
    );
    bridge.finish_native_writes().await;
    extensions.begin_shutdown();
    harness.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires staged Cursor DLL/runtime and isolated configuration; run installed_cursor tests with --test-threads=1"]
async fn installed_cursor_reload_starts_the_new_binding_and_reaps_the_old_worker() {
    let directory = PathBuf::from(std::env::var_os("RPI_TEST_CURSOR_EXTENSION_DIR").unwrap());
    let agent_dir = PathBuf::from(std::env::var_os("RPI_CODING_AGENT_DIR").unwrap());
    assert!(agent_dir.join("native-fixture-marker").is_file());
    let cwd = tempfile::tempdir().unwrap();
    let args = crate::args::Args {
        trust_override: Some(false),
        api_key: Some("offline-test-key".into()),
        extensions_dir: vec![directory],
        session_dir: Some(cwd.path().join("sessions")),
        no_skills: true,
        no_prompt_templates: true,
        no_context_files: true,
        ..Default::default()
    };
    let resolved = crate::provider::resolve(
        Some("anthropic"),
        Some(crate::provider::DEFAULT_MODEL_ID),
        None,
        Some("offline-test-key"),
        None,
    )
    .unwrap();
    let (harness, _events, context) = crate::session::build(&resolved, &args, cwd.path(), false)
        .await
        .unwrap();
    context
        .action_bridge
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .clone_host()
        .set_native_mode("print");
    assert_eq!(
        context
            .extension_session
            .lock()
            .unwrap()
            .loaded_paths()
            .len(),
        1
    );
    assert_eq!(
        crate::session::dispatch_session_event_async(
            &context,
            rpi_plugin_sdk::EventTag::SessionStart
        )
        .await,
        None
    );
    #[cfg(target_os = "linux")]
    let original = node_children();
    #[cfg(target_os = "linux")]
    assert!(
        !original.is_empty(),
        "the initial Cursor worker must be running"
    );
    let outcome = crate::session::reload_extension_resources(&harness, &context).await;
    assert!(
        !outcome.had_errors,
        "{} {:?}",
        outcome.summary, outcome.details
    );
    assert_eq!(
        context
            .extension_session
            .lock()
            .unwrap()
            .loaded_paths()
            .len(),
        1
    );
    #[cfg(target_os = "linux")]
    {
        let current = node_children();
        assert_eq!(
            current.difference(&original).count(),
            1,
            "reload must create a fresh worker"
        );
        assert_eq!(
            original.difference(&current).count(),
            1,
            "reload must reap the old worker"
        );
    }
    let providers = rpi_extensions::PluggableProvider::from_session(
        &context.extension_session.lock().unwrap(),
        Handle::current(),
    );
    let provider = providers
        .iter()
        .find(|provider| provider.id() == "cursor")
        .unwrap();
    let model = provider.models()[0].clone();
    let options = SimpleStreamOptions {
        authentication: rpi_ai::auth::ReplacementAuth::from_private_request(
            "cursor".into(),
            Default::default(),
        ),
        ..Default::default()
    };
    let mut stream = provider
        .stream_simple(&model, &Context::new(vec![]), &options)
        .await;
    while stream.next().await.is_some() {}
    let message = stream.result().await.unwrap();
    assert!(
        message
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains("API key"),
        "the new binding must be active: {message:?}"
    );
    assert_eq!(
        crate::session::dispatch_session_event_async(
            &context,
            rpi_plugin_sdk::EventTag::SessionShutdown
        )
        .await,
        None
    );
    context.extension_session.lock().unwrap().begin_shutdown();
    harness.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires staged Cursor DLL/runtime and isolated configuration; run installed_cursor tests with --test-threads=1"]
async fn installed_cursor_shutdown_reaps_when_session_context_is_unavailable() {
    let directory = PathBuf::from(std::env::var_os("RPI_TEST_CURSOR_EXTENSION_DIR").unwrap());
    let agent_dir = PathBuf::from(std::env::var_os("RPI_CODING_AGENT_DIR").unwrap());
    assert!(agent_dir.join("native-fixture-marker").is_file());
    let cwd = tempfile::tempdir().unwrap();
    let (host, _cell) = HarnessActionHost::new_empty(
        vec![],
        cwd.path().into(),
        Handle::current(),
        BTreeMap::new(),
        Default::default(),
    );
    // Startup failed before the application could bind a harness or run mode.
    let bridge = rpi_extensions::ActionBridge::new(Handle::current(), Arc::new(host));
    #[cfg(target_os = "linux")]
    let before = node_children();
    let extensions = rpi_extensions::load_session(
        &[directory],
        Arc::new(rpi_extensions::NullDiagnostics),
        Some(bridge.clone()),
    );
    assert_eq!(extensions.loaded_paths().len(), 1);
    let snapshot = extensions.snapshot_arc().unwrap();
    #[cfg(target_os = "linux")]
    assert_eq!(node_children().difference(&before).count(), 1);
    for _ in 0..2 {
        rpi_extensions::dispatch_lifecycle_event_with_data(
            &snapshot,
            rpi_plugin_sdk::EventTag::SessionShutdown,
            extensions.keepalive(),
            json!({"reason":"quit"}),
        )
        .await;
    }
    #[cfg(target_os = "linux")]
    assert_eq!(
        node_children(),
        before,
        "shutdown must reap even while the library stays loaded"
    );
    bridge.finish_native_writes().await;
    extensions.begin_shutdown();
}
