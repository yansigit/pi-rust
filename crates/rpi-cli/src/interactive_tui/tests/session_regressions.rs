use super::*;

fn state_with_root(root: &std::path::Path) -> Arc<TuiState> {
    let mut state = test_tui_state();
    Arc::get_mut(&mut state).unwrap().session_dir = root.to_path_buf();
    state
}

async fn harness_with_root(
    cwd: &std::path::Path,
    root: &std::path::Path,
) -> (AgentHarness, crate::session::ReloadContext) {
    let resolved = crate::provider::resolve(
        Some("anthropic"),
        Some(crate::provider::DEFAULT_MODEL_ID),
        None,
        Some("test-key"),
        None,
    )
    .unwrap();
    let args = crate::args::Args {
        session_dir: Some(root.to_path_buf()),
        no_session: true,
        no_extensions: true,
        no_tools: true,
        no_skills: true,
        no_prompt_templates: true,
        no_context_files: true,
        system_prompt: Some("test".into()),
        ..Default::default()
    };
    let (harness, _events, reload_context) = crate::session::build(&resolved, &args, cwd, false)
        .await
        .unwrap();
    (harness, reload_context)
}

fn assert_session_locked(path: &str) {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(format!("{path}.lock"))
        .unwrap();
    assert!(
        fs2::FileExt::try_lock_exclusive(&lock).is_err(),
        "{path} must remain exclusively locked"
    );
}

#[tokio::test]
async fn tui_fork_locks_the_copy_and_keeps_the_selected_root_for_switch_and_picker() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let root = temporary.path().join("custom sessions");
    let source = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    source
        .append_message(user_message_with_images("custom-root history", vec![]))
        .await
        .unwrap();
    let initial_id = source.storage().metadata().id.clone();
    let (harness, reload_context) = harness_with_root(&cwd, &root).await;
    harness.set_session(source).await.unwrap();
    let state = state_with_root(&root);
    let context = rpi_extensions::ToolCallContext::new(cwd.to_string_lossy());
    fork_session(
        &harness,
        &cwd,
        &state.chat_container,
        &state,
        &context,
        &reload_context,
    )
    .await;
    let fork_id = harness.session().storage().metadata().id.clone();
    assert_ne!(fork_id, initial_id);
    let metadata = crate::session::list_session_metadata_in_dir(&cwd.to_string_lossy(), &root)
        .await
        .unwrap();
    assert_eq!(metadata.len(), 2);
    for row in &metadata {
        assert_session_locked(&row.path);
    }
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    assert!(
        switch_to_session(
            &harness,
            &lane,
            &initial_id,
            &cwd,
            &state.chat_container,
            &state,
            &context,
            &reload_context
        )
        .await
    );
    assert_eq!(harness.session().storage().metadata().id, initial_id);
    let rendered = strip_ansi(&state.chat_container.render(160).join("\n"));
    assert!(rendered.contains("custom-root history"));

    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        true,
        None,
    ));
    let editor_container = Arc::new(Container::new());
    let (tx, mut rx) = mpsc::unbounded_channel();
    open_session_selector(&state, &editor_container, &state.editor, &tui, &tx);
    assert!(
        state.selector_open(),
        "picker must find sessions in the selected root"
    );
    let router = state
        .active_selector
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .0
        .clone();
    router.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let Some(TuiMessage::SwitchSession(selected)) =
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
    else {
        panic!("session selection expected")
    };
    assert!(metadata.iter().any(|row| row.path.contains(&selected)));
    assert!(!cwd.join(".rpi/sessions").exists());
}

#[tokio::test]
async fn tui_import_preserves_identity_history_and_source_across_cwds() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().join("project");
    let old_cwd = temporary.path().join("original project");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&old_cwd).unwrap();
    let root = temporary.path().join("custom sessions");
    let source_root = temporary.path().join("source");
    let source = crate::session::create_jsonl_session(&source_root, &old_cwd.to_string_lossy())
        .await
        .unwrap();
    source.set_name(Some("Imported history")).await.unwrap();
    source
        .append_message(user_message_with_images(
            "preserved imported prompt",
            vec![],
        ))
        .await
        .unwrap();
    let expected = source.storage().metadata().id.clone();
    let source_path = crate::session::list_session_files_sync(&source_root).remove(0);
    let original = std::fs::read(&source_path).unwrap();
    let (harness, reload_context) = harness_with_root(&cwd, &root).await;
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let state = state_with_root(&root);
    let context = rpi_extensions::ToolCallContext::new(cwd.to_string_lossy());
    import_session(
        &harness,
        &lane,
        &source_path.to_string_lossy(),
        &cwd,
        &state.chat_container,
        &state,
        &context,
        &reload_context,
    )
    .await;
    assert_eq!(harness.session().storage().metadata().id, expected);
    assert_eq!(
        harness.session().get_name().await.unwrap().as_deref(),
        Some("Imported history")
    );
    assert!(strip_ansi(&state.chat_container.render(160).join("\n"))
        .contains("preserved imported prompt"));
    let metadata = crate::session::list_session_metadata_in_dir(&cwd.to_string_lossy(), &root)
        .await
        .unwrap();
    assert_eq!(metadata.len(), 1);
    assert_eq!(metadata[0].cwd, old_cwd.to_string_lossy());
    assert_session_locked(&metadata[0].path);
    let restored =
        crate::session::open_session_by_id_in_dir(&expected, &cwd.to_string_lossy(), &root)
            .await
            .map_err(|error| error.to_string())
            .unwrap();
    assert_eq!(
        restored.get_name().await.unwrap().as_deref(),
        Some("Imported history")
    );
    let imported = std::fs::read(&metadata[0].path).unwrap();
    assert_eq!(imported, original);
    // Both repeated imports and importing a repository's own log must fail
    // without touching either saved history or the original source.
    for path in [&source_path, &std::path::PathBuf::from(&metadata[0].path)] {
        assert!(
            crate::session::import_session_file(path, &cwd.to_string_lossy(), &root)
                .await
                .err()
                .unwrap()
                .contains("already exists")
        );
    }
    assert_eq!(std::fs::read(&source_path).unwrap(), original);
    assert_eq!(std::fs::read(&metadata[0].path).unwrap(), imported);
    assert_eq!(crate::session::list_session_files_sync(&root).len(), 1);
}

#[tokio::test]
async fn invalid_import_leaves_the_active_session_and_repository_untouched() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path();
    let root = cwd.join("selected");
    let invalid = cwd.join("invalid.jsonl");
    std::fs::write(&invalid, "not a session\n").unwrap();
    let (harness, reload_context) = harness_with_root(cwd, &root).await;
    let initial = harness.session().storage().metadata().id.clone();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let state = state_with_root(&root);
    let context = rpi_extensions::ToolCallContext::new(cwd.to_string_lossy());
    import_session(
        &harness,
        &lane,
        &invalid.to_string_lossy(),
        cwd,
        &state.chat_container,
        &state,
        &context,
        &reload_context,
    )
    .await;
    assert_eq!(harness.session().storage().metadata().id, initial);
    assert!(crate::session::list_session_files_sync(&root).is_empty());
    assert!(
        std::fs::read_dir(&root).unwrap().next().is_none(),
        "staging files must be cleaned up"
    );
    assert_eq!(std::fs::read_to_string(invalid).unwrap(), "not a session\n");
}

/// B-3: an imported archive keeps its source bytes (and therefore its source
/// `cwd`), so import must be validated by the cwd the copy runs in and a later
/// resume must be refused as an archive — explicitly, not by leaking a
/// cross-cwd error from the header. Both paths go through
/// `SessionRuntimeFactory::validate_destination`.
#[tokio::test]
async fn imported_archive_is_archive_only_for_import_and_resume() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().join("project");
    let old_cwd = temporary.path().join("original project");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&old_cwd).unwrap();
    let root = temporary.path().join("custom sessions");
    let source_root = temporary.path().join("source");
    let source = crate::session::create_jsonl_session(&source_root, &old_cwd.to_string_lossy())
        .await
        .unwrap();
    source
        .append_message(user_message_with_images("archived prompt", vec![]))
        .await
        .unwrap();
    let archived_id = source.storage().metadata().id.clone();
    let source_path = crate::session::list_session_files_sync(&source_root).remove(0);
    let original = std::fs::read(&source_path).unwrap();
    let (harness, reload_context) = harness_with_root(&cwd, &root).await;

    // Import is allowed even though the archive carries another project's cwd:
    // the copy runs here, and the durable log keeps the source bytes verbatim.
    match crate::session::replace_host_session(
        &harness,
        &reload_context,
        crate::session::HostSessionRequest::Import(source_path.clone()),
    )
    .await
    {
        Ok(crate::session::SessionReplacement::Replaced) => {}
        other => panic!("import must be accepted: {other:?}"),
    }
    assert_eq!(harness.session().storage().metadata().id, archived_id);
    let stored = crate::session::list_session_files_sync(&root).remove(0);
    assert_eq!(std::fs::read(&stored).unwrap(), original);
    assert!(
        std::path::Path::new(&format!("{}.import", stored.display())).is_file(),
        "the archive must carry durable import provenance"
    );

    // Resume of the same archive is refused, and the refusal is the archive
    // rule rather than the header cwd: it happens before any teardown.
    let error = crate::session::replace_host_session(
        &harness,
        &reload_context,
        crate::session::HostSessionRequest::Resume(archived_id.clone()),
    )
    .await
    .expect_err("resuming an imported archive must be refused");
    assert!(error.contains("archive-only"), "{error}");
    assert!(!error.contains("cross-cwd"), "{error}");
    assert_eq!(
        harness.session().storage().metadata().id,
        archived_id,
        "a refused resume keeps the imported session live"
    );
    assert_eq!(std::fs::read(&stored).unwrap(), original);

    // Import is validated, not bypassed: with the project gone another archive
    // is refused before any teardown instead of being copied into a cwd this
    // process cannot serve (B-3).
    let second = crate::session::create_jsonl_session(&source_root, &old_cwd.to_string_lossy())
        .await
        .unwrap();
    second
        .append_message(user_message_with_images("second prompt", vec![]))
        .await
        .unwrap();
    let second_source = crate::session::list_session_files_sync(&source_root)
        .into_iter()
        .find(|path| path != &source_path)
        .unwrap();
    std::fs::remove_dir_all(&cwd).unwrap();
    let error = crate::session::replace_host_session(
        &harness,
        &reload_context,
        crate::session::HostSessionRequest::Import(second_source),
    )
    .await
    .expect_err("import must validate the destination cwd");
    assert!(error.contains("does not exist"), "{error}");
}

/// B-1/B-2: a rebuild failure after `set_active_session` must be reported as a
/// degraded replacement, never as a clean success with dead extension authority.
/// Every fallible construction step is staged before the first harness mutation;
/// the remaining failures are harness setters on a harness that went away.
#[tokio::test]
async fn rebuild_failure_after_the_swap_is_reported_as_degraded() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let root = temporary.path().join("custom sessions");
    let (harness, reload_context) = harness_with_root(&cwd, &root).await;
    let factory = crate::session::SessionRuntimeFactory::from_reload(&reload_context);
    let replacement = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let replacement_id = replacement.storage().metadata().id.clone();
    // The replacement coordinator swaps the durable session *before* rebuilding
    // the extension authority; do the same, then make the harness go away.
    crate::session::set_active_session(&harness, replacement, Some(&reload_context.tool_context))
        .await
        .unwrap();
    assert_eq!(harness.session().storage().metadata().id, replacement_id);
    harness.close().await.unwrap();

    let lane = harness.lane("main");
    let context = crate::session::SessionReplacementContext {
        runtime_factory: Some(&factory),
        harness: &harness,
        lane: lane.as_ref(),
        extension_session: &reload_context.extension_session,
        action_bridge: &reload_context.action_bridge,
        tool_context: &reload_context.tool_context,
    };
    let old_bridge = reload_context
        .action_bridge
        .lock()
        .unwrap()
        .clone()
        .expect("a built session owns a bridge");
    let outcome = factory.rebuild(&context, vec![], old_bridge.clone()).await;
    match outcome {
        crate::session::RebuildOutcome::Installed => {
            panic!("a closed harness cannot install a replacement")
        }
        crate::session::RebuildOutcome::Failed(error) => {
            panic!("failures before the first harness mutation must not reach this point: {error}")
        }
        crate::session::RebuildOutcome::Degraded(reason) => {
            assert!(reason.contains("model runtime"), "{reason}");
        }
    }
    // Nothing of the failed install was published: the cells still name the
    // outgoing authority, and the degraded signal is the only record (B-1/B-2).
    let published = reload_context
        .action_bridge
        .lock()
        .unwrap()
        .clone()
        .expect("bridge stays published");
    assert!(std::sync::Arc::ptr_eq(&published, &old_bridge));
}
