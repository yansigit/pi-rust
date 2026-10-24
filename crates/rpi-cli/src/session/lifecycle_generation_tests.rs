use super::*;

struct RestoreConfig(Option<std::ffi::OsString>);
impl Drop for RestoreConfig {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
            None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
        }
    }
}

#[tokio::test]
async fn loaded_replacement_retires_old_authority_and_constructs_a_fresh_registration() {
    let temporary = tempfile::tempdir().unwrap();
    let _env = crate::config::test_support::SessionDirEnv::new(None);
    let _config = RestoreConfig(std::env::var_os(crate::config::CONFIG_DIR_ENV));
    let agent = temporary.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);
    let source = temporary.path().join("plugin.rs");
    std::fs::write(
        &source,
        r#"
#[no_mangle]
pub extern "C" fn rpi_plugin_register(_: *const u8) -> i32 { 0 }
"#,
    )
    .unwrap();
    let library = temporary.path().join(format!(
        "{}generation{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    let output = std::process::Command::new("rustc")
        .args(["--crate-type=cdylib", "--edition=2021"])
        .arg(&source)
        .arg("-o")
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let resolved = crate::provider::resolve(
        Some("anthropic"),
        Some(crate::provider::DEFAULT_MODEL_ID),
        None,
        Some("test-key"),
        None,
    )
    .unwrap();
    let args = Args {
        extension: vec![library],
        session_dir: Some(temporary.path().join("sessions")),
        no_tools: true,
        no_skills: true,
        no_prompt_templates: true,
        no_context_files: true,
        system_prompt: Some("test".into()),
        ..Default::default()
    };
    let cwd = temporary.path();
    let prepared = prepare_extensions(&args, cwd, false).await.unwrap();
    let (harness, _events, context) = build_prepared(&resolved, &args, cwd, false, prepared)
        .await
        .unwrap();
    let old_session = context.extension_session.lock().unwrap().clone();
    assert_eq!(old_session.loaded_paths().len(), 1);
    let old_snapshot = old_session.snapshot_arc().unwrap();
    let old_bridge = context.action_bridge.lock().unwrap().clone().unwrap();
    let host = old_bridge.clone_host();
    let old_id = context.tool_context.session_id();
    let factory = SessionRuntimeFactory::from_reload(&context);
    let admission = factory.try_admit().unwrap();
    assert!(host
        .new_session(serde_json::json!({}))
        .await
        .unwrap_err()
        .contains("already in progress"));
    drop(admission);
    assert!(old_bridge.is_active());
    assert_eq!(old_id, context.tool_context.session_id());
    let invalid = create_jsonl_session(
        &temporary.path().join("other-sessions"),
        temporary.path().join("missing-cwd").to_str().unwrap(),
    )
    .await
    .unwrap();
    let lane = harness.lane("main");
    let ctx = SessionReplacementContext {
        harness: &harness,
        lane: lane.as_ref(),
        extension_session: &context.extension_session,
        action_bridge: &context.action_bridge,
        tool_context: &context.tool_context,
        runtime_factory: Some(&factory),
    };
    assert!(
        replace_active_session(&ctx, invalid, SessionReplacementReason::Resume, None)
            .await
            .unwrap_err()
            .contains("working directory does not exist")
    );
    assert!(
        old_bridge.is_active(),
        "destination preparation must precede retirement"
    );
    assert!(!old_session.shutdown_signal().is_cancelled());
    host.new_session(serde_json::json!({})).await.unwrap();
    assert_ne!(old_id, context.tool_context.session_id());
    assert!(
        !old_bridge.is_active(),
        "old command/native contexts must be stale"
    );
    assert!(!old_snapshot.active_flag().load(Ordering::SeqCst));
    assert!(old_session.shutdown_signal().is_cancelled());
    let fresh = context.extension_session.lock().unwrap().clone();
    assert_eq!(fresh.loaded_paths().len(), 1);
    assert!(!fresh.shutdown_signal().is_cancelled());
    assert!(fresh
        .snapshot_arc()
        .unwrap()
        .active_flag()
        .load(Ordering::SeqCst));
    let fresh_bridge = context.action_bridge.lock().unwrap().clone().unwrap();
    assert!(fresh_bridge.is_active());
    assert!(!Arc::ptr_eq(&old_bridge, &fresh_bridge));
    // A second replacement is also constructed from the factory, not the closed runtime.
    fresh_bridge
        .clone_host()
        .new_session(serde_json::json!({}))
        .await
        .unwrap();
    assert!(!fresh_bridge.is_active());
    assert!(context
        .action_bridge
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .is_active());
    let source_id = harness.session().storage().metadata().id.clone();
    let anchor = harness
        .session()
        .append_message(rpi_agent::AgentMessage::User(rpi_ai::UserMessage::new(
            "fork-anchor",
            0,
        )))
        .await
        .unwrap();
    harness.session().sync().await.unwrap();
    replace_host_session(
        &harness,
        &context,
        HostSessionRequest::Fork {
            entry_id: Some(anchor.clone()),
            position: rpi_harness::session::types::ForkPosition::Before,
        },
    )
    .await
    .unwrap();
    assert!(harness
        .session()
        .get_entry(&anchor)
        .await
        .unwrap()
        .is_none());
    replace_host_session(&harness, &context, HostSessionRequest::Resume(source_id))
        .await
        .unwrap();
    replace_host_session(
        &harness,
        &context,
        HostSessionRequest::Fork {
            entry_id: Some(anchor.clone()),
            position: rpi_harness::session::types::ForkPosition::At,
        },
    )
    .await
    .unwrap();
    assert!(harness
        .session()
        .get_entry(&anchor)
        .await
        .unwrap()
        .is_some());
}
