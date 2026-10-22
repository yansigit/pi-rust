use super::*;

/// The TUI state a dialog test needs. Mirrors the builder in
/// `interactive_tui::tests` (kept local because sibling test modules are
/// private to each other).
fn test_state() -> Arc<TuiState> {
    Arc::new(TuiState {
        current_assistant: Arc::new(std::sync::Mutex::new(None)),
        tool_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        bash_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        hide_thinking: Arc::new(std::sync::Mutex::new(false)),
        tool_outputs_expanded: Arc::new(std::sync::Mutex::new(false)),
        show_terminal_progress: true,
        status: std::sync::Mutex::new(RunStatus::Idle),
        ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
        ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
        editor_text: rpi_extensions::EditorTextMailbox::new(),
        auto_send: std::sync::Mutex::new(None),
        last_editor_text: std::sync::Mutex::new(String::new()),
        programmatic_editor_write: std::sync::atomic::AtomicBool::new(false),
        js_preparation_cancel: std::sync::Mutex::new(None),
        user_bash_cancel: std::sync::Mutex::new(None),
        pending_bash_messages: std::sync::Mutex::new(Vec::new()),
        footer: Arc::new(FooterComponent::new()),
        status_container: Arc::new(Container::new()),
        chat_container: Arc::new(Container::new()),
        loader: Arc::new(Loader::new()),
        editor: Arc::new(Editor::simple()),
        last_assistant_text: std::sync::Mutex::new(String::new()),
        active_selector: std::sync::Mutex::new(None),
        provider_form: std::sync::Mutex::new(None),
        active_provider_input: std::sync::Mutex::new(None),
        provider_auth_flow_id: std::sync::atomic::AtomicU64::new(0),
        provider_auth_generation: std::sync::atomic::AtomicU64::new(0),
        active_provider_select: std::sync::Mutex::new(None),
        active_extension_editor: std::sync::Mutex::new(None),
        active_extension_input: std::sync::Mutex::new(None),
        active_extension_cancel: std::sync::Mutex::new(None),
        autocomplete: AutocompleteManager::new(),
        autocomplete_container: Arc::new(Container::new()),
        autocomplete_max_visible: 5,
        autocomplete_selection: std::sync::Mutex::new(0),
        pending_images: std::sync::Mutex::new(Vec::new()),
        theme_manager: Arc::new(ThemeManager::new()),
        tui: None,
        current_model_id: std::sync::Mutex::new(String::new()),
        show_images: std::sync::Mutex::new(true),
        cache_miss_notices: std::sync::Mutex::new(false),
        history: std::sync::Mutex::new(Vec::new()),
        history_index: std::sync::Mutex::new(-1),
        history_draft: std::sync::Mutex::new(None),
        cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
        scoped_edit: std::sync::Mutex::new(None),
        markdown_transformer: Arc::new(std::sync::Mutex::new(None)),
        extension_claimed_keys: Mutex::new(Default::default()),
        extension_session: Arc::new(std::sync::Mutex::new(
            rpi_extensions::ExtensionSession::none(),
        )),
        search: Arc::new(AltScreenSearch::new()),
        search_bar: Arc::new(SearchBar::new()),
        pending_container: Arc::new(Container::new()),
        dequeue_hint: String::new(),
        pending_snapshot: std::sync::Mutex::new(
            rpi_harness::agent_harness::QueuedMessages::default(),
        ),
        selection_start: std::sync::Mutex::new(None),
        selection_end: std::sync::Mutex::new(None),
    })
}

struct Fixture {
    state: Arc<TuiState>,
    editor_container: Arc<Container>,
    editor: Arc<Editor>,
    tui: Arc<TuiAltScreen>,
    tx: mpsc::UnboundedSender<TuiMessage>,
    rx: mpsc::UnboundedReceiver<TuiMessage>,
}

fn fixture() -> Fixture {
    let state = test_state();
    let editor_container = Arc::new(Container::new());
    let editor = Arc::new(Editor::simple());
    editor_container.add_child(editor.clone());
    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        true,
        None,
    ));
    let (tx, rx) = mpsc::unbounded_channel();
    Fixture {
        state,
        editor_container,
        editor,
        tui,
        tx,
        rx,
    }
}

impl Fixture {
    fn selector_list(&self) -> Arc<SelectList> {
        let (view, _) = self
            .state
            .active_selector
            .lock()
            .unwrap()
            .clone()
            .expect("selector open");
        match view {
            SelectorView::List(list) => list,
            _ => panic!("expected a plain list"),
        }
    }

    fn press(&self, code: KeyCode) {
        self.selector_list()
            .handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }
}

#[test]
fn summary_choices_are_the_three_pi_answers() {
    let items = summary_items();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].value, "No summary");
    assert_eq!(items[1].value, "Summarize");
    assert_eq!(items[2].value, "Summarize with custom prompt");
    assert_eq!(
        parse_summary_choice("No summary"),
        Some(SummaryChoice::NoSummary)
    );
    assert_eq!(
        parse_summary_choice("Summarize"),
        Some(SummaryChoice::Summarize)
    );
    assert_eq!(
        parse_summary_choice("Summarize with custom prompt"),
        Some(SummaryChoice::Custom)
    );
    assert_eq!(parse_summary_choice("unexpected"), None);
}

#[test]
fn dialog_answers_map_to_pi_navigate_options() {
    let none = TreeNavigateRequest::from_choice("target", SummaryChoice::NoSummary, None);
    assert_eq!(
        none.clone().into_options().summarize,
        false,
        "No summary keeps summarize false"
    );
    assert_eq!(none.clone().into_options().custom_instructions, None);

    let summary = TreeNavigateRequest::from_choice("target", SummaryChoice::Summarize, None);
    assert!(summary.into_options().summarize);

    let custom = TreeNavigateRequest::from_choice(
        "target",
        SummaryChoice::Custom,
        Some("keep the API notes".into()),
    );
    let options = custom.into_options();
    assert!(options.summarize);
    assert_eq!(
        options.custom_instructions.as_deref(),
        Some("keep the API notes")
    );
}

#[test]
fn skip_prompt_defaults_to_no_summary_without_a_dialog() {
    assert!(should_prompt_for_summary(false));
    assert!(!should_prompt_for_summary(true));
    assert!(already_at_entry(Some("left"), "left"));
    assert!(!already_at_entry(Some("left"), "target"));
    assert!(!already_at_entry(None, "target"));
}

#[test]
fn editor_text_only_fills_an_empty_draft() {
    assert_eq!(
        editor_text_to_apply(Some("edit this"), ""),
        Some("edit this")
    );
    assert_eq!(
        editor_text_to_apply(Some("edit this"), "   \n"),
        Some("edit this")
    );
    assert_eq!(editor_text_to_apply(Some("edit this"), "my draft"), None);
    assert_eq!(editor_text_to_apply(Some(""), ""), None);
    assert_eq!(editor_text_to_apply(None, ""), None);
}

#[test]
fn navigation_outcomes_map_to_pi_status_and_retry() {
    let completed = TreeNavigationEnd::Completed {
        editor_text: Some("edit this".into()),
    };
    assert_eq!(end_status(&completed), Some(STATUS_NAVIGATED));
    assert!(!reopens_tree_selector(&completed));

    assert_eq!(
        end_status(&TreeNavigationEnd::Cancelled),
        Some(STATUS_CANCELLED)
    );
    assert!(!reopens_tree_selector(&TreeNavigationEnd::Cancelled));

    assert_eq!(
        end_status(&TreeNavigationEnd::Aborted),
        Some(STATUS_SUMMARY_CANCELLED)
    );
    assert!(
        reopens_tree_selector(&TreeNavigationEnd::Aborted),
        "an aborted summary returns to the same tree selection"
    );

    assert_eq!(end_status(&TreeNavigationEnd::Failed("nope".into())), None);
    assert_eq!(end_status(&TreeNavigationEnd::Error("closed".into())), None);
}

#[test]
fn summary_selector_emits_a_no_summary_navigation() {
    let mut fx = fixture();
    open_summary_selector(
        &fx.state,
        &fx.editor_container,
        &fx.editor,
        &fx.tui,
        &fx.tx,
        "target",
    );
    assert!(fx.state.selector_open());
    assert!(matches!(
        fx.state.active_selector.lock().unwrap().as_ref().unwrap().1,
        SelectorKind::TreeSummary
    ));
    fx.press(KeyCode::Enter);
    match fx.rx.try_recv().expect("navigation request") {
        TuiMessage::TreeNavigate(request) => {
            assert_eq!(request.target_id, "target");
            assert!(!request.summarize);
            assert_eq!(request.custom_instructions, None);
        }
        _ => panic!("expected TreeNavigate"),
    }
    assert!(!fx.state.selector_open(), "the selector closes first");
}

#[test]
fn summary_selector_emits_a_summarize_navigation() {
    let mut fx = fixture();
    open_summary_selector(
        &fx.state,
        &fx.editor_container,
        &fx.editor,
        &fx.tui,
        &fx.tx,
        "target",
    );
    fx.press(KeyCode::Down);
    fx.press(KeyCode::Enter);
    match fx.rx.try_recv().expect("navigation request") {
        TuiMessage::TreeNavigate(request) => {
            assert_eq!(request.target_id, "target");
            assert!(request.summarize);
            assert_eq!(request.custom_instructions, None);
        }
        _ => panic!("expected TreeNavigate"),
    }
}

#[test]
fn custom_choice_opens_the_instruction_editor() {
    let mut fx = fixture();
    open_summary_selector(
        &fx.state,
        &fx.editor_container,
        &fx.editor,
        &fx.tui,
        &fx.tx,
        "target",
    );
    fx.press(KeyCode::Down);
    fx.press(KeyCode::Down);
    fx.press(KeyCode::Enter);
    assert!(!fx.state.selector_open());
    assert!(fx.state.extension_editor_open());
    assert!(fx.state.active_extension_cancel.lock().unwrap().is_some());
    assert!(fx.rx.try_recv().is_err(), "no navigation before submit");
}

#[test]
fn custom_instructions_submit_navigates_with_the_text() {
    let mut fx = fixture();
    open_custom_editor(
        &fx.state,
        &fx.editor_container,
        &fx.editor,
        &fx.tui,
        &fx.tx,
        "target",
    );
    let custom = fx
        .state
        .active_extension_editor
        .lock()
        .unwrap()
        .clone()
        .expect("custom editor");
    custom.set_text("focus on the API decisions");
    custom.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    match fx.rx.try_recv().expect("navigation request") {
        TuiMessage::TreeNavigate(request) => {
            assert_eq!(request.target_id, "target");
            assert!(request.summarize);
            assert_eq!(
                request.custom_instructions.as_deref(),
                Some("focus on the API decisions")
            );
        }
        _ => panic!("expected TreeNavigate"),
    }
    assert!(!fx.state.extension_editor_open());
}

#[test]
fn cancelling_custom_instructions_returns_to_the_summary_selector() {
    let mut fx = fixture();
    open_custom_editor(
        &fx.state,
        &fx.editor_container,
        &fx.editor,
        &fx.tui,
        &fx.tx,
        "target",
    );
    assert!(run_extension_cancel(&fx.state));
    assert!(!fx.state.extension_editor_open());
    assert!(fx.state.selector_open());
    assert!(matches!(
        fx.state.active_selector.lock().unwrap().as_ref().unwrap().1,
        SelectorKind::TreeSummary
    ));
    assert!(fx.rx.try_recv().is_err(), "cancelling does not navigate");
}

#[test]
fn cancelling_the_summary_selector_requests_the_tree_with_the_same_entry() {
    let mut fx = fixture();
    open_summary_selector(
        &fx.state,
        &fx.editor_container,
        &fx.editor,
        &fx.tui,
        &fx.tx,
        "target",
    );
    fx.press(KeyCode::Esc);
    match fx.rx.try_recv().expect("tree reopen request") {
        TuiMessage::OpenTreeAt(entry_id) => assert_eq!(entry_id, "target"),
        _ => panic!("expected OpenTreeAt"),
    }
    assert!(!fx.state.selector_open());
    assert!(fx.state.active_extension_cancel.lock().unwrap().is_none());
}
