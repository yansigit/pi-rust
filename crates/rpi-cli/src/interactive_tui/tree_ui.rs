//! `/tree` navigation dialogs and outcome handling.
//!
//! Mirrors Pi 1.3.0 `showTreeSelector` (interactive-mode.js:4660-4720): after a
//! tree entry is selected the user picks "No summary" / "Summarize" /
//! "Summarize with custom prompt"; the custom prompt opens the multi-line
//! editor; Escape from either dialog walks back (custom editor → summary
//! selector → tree selector with the same entry selected); and only then is
//! `AgentLane::navigate_tree` called with the chosen options. The outcome is
//! reported exactly as Pi does: an aborted summary re-opens the tree selector,
//! a hook-declined navigation says "Navigation cancelled", and a failed tree
//! shows the harness error. `session_before_tree`, `session_tree` and the
//! terminal `session_tree_failed` event are emitted by the harness for every
//! navigation reached through this path.
//!
//! The branch-summary skip preference (`branchSummary.skipPrompt`) bypasses the
//! selector and navigates without a summary, like Pi.

use super::*;
use rpi_harness::agent_harness::{NavigateTreeOptions, NavigationOutcome, NavigationResult};
use rpi_harness::result::OperationKind;

pub(super) const SUMMARY_TITLE: &str = "Summarize branch?";
pub(super) const CUSTOM_TITLE: &str = "Custom summarization instructions";
pub(super) const SUMMARY_HINT: &str = "↑↓ navigate · Enter select · Esc/Ctrl+C cancel";
pub(super) const CUSTOM_HINT: &str = "Enter submit · Shift+Enter newline · Esc/Ctrl+C cancel";
pub(super) const STATUS_ALREADY_THERE: &str = "Already at this point";
pub(super) const STATUS_NAVIGATED: &str = "Navigated to selected point";
pub(super) const STATUS_CANCELLED: &str = "Navigation cancelled";
pub(super) const STATUS_SUMMARY_CANCELLED: &str = "Branch summarization cancelled";
pub(super) const BUSY_MESSAGE: &str =
    "Wait for the current compaction or tree navigation to finish before navigating the session tree.";

/// The three answers Pi's "Summarize branch?" selector offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SummaryChoice {
    NoSummary,
    Summarize,
    Custom,
}

impl SummaryChoice {
    /// Stable item value (also the label's key, like Pi's string selectors).
    pub(super) fn value(self) -> &'static str {
        match self {
            SummaryChoice::NoSummary => "No summary",
            SummaryChoice::Summarize => "Summarize",
            SummaryChoice::Custom => "Summarize with custom prompt",
        }
    }
}

/// Map a selector value back to a choice. Unknown values are ignored rather
/// than guessed so a future item cannot silently summarize a branch.
pub(super) fn parse_summary_choice(value: &str) -> Option<SummaryChoice> {
    match value {
        "No summary" => Some(SummaryChoice::NoSummary),
        "Summarize" => Some(SummaryChoice::Summarize),
        "Summarize with custom prompt" => Some(SummaryChoice::Custom),
        _ => None,
    }
}

pub(super) fn summary_items() -> Vec<SelectItem> {
    [
        SummaryChoice::NoSummary,
        SummaryChoice::Summarize,
        SummaryChoice::Custom,
    ]
    .into_iter()
    .map(|choice| SelectItem::new(choice.value(), choice.value()))
    .collect()
}

/// Pi checks `!settingsManager.getBranchSummarySkipPrompt()` before showing the
/// prompt; the skip preference means "always default to no summary".
pub(super) fn should_prompt_for_summary(skip_prompt: bool) -> bool {
    !skip_prompt
}

/// Read the live preference. Mirrors Pi's settings manager read at prompt time
/// (a `/settings` change applies to the next `/tree`), and treats unreadable
/// settings as the upstream default: prompt.
pub(super) fn summary_prompt_skipped() -> bool {
    crate::settings::load_settings()
        .map(|settings| settings.branch_summary_skipped())
        .unwrap_or(false)
}

pub(super) fn already_at_entry(leaf_id: Option<&str>, target_id: &str) -> bool {
    leaf_id == Some(target_id)
}

/// The options one dialog answer produces. Kept as its own value so the
/// keyboard callbacks and the async navigator agree on what was chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TreeNavigateRequest {
    pub(super) target_id: String,
    pub(super) summarize: bool,
    pub(super) custom_instructions: Option<String>,
}

impl TreeNavigateRequest {
    pub(super) fn no_summary(target_id: &str) -> Self {
        Self {
            target_id: target_id.to_string(),
            summarize: false,
            custom_instructions: None,
        }
    }

    pub(super) fn summarize(target_id: &str, custom_instructions: Option<String>) -> Self {
        Self {
            target_id: target_id.to_string(),
            summarize: true,
            custom_instructions,
        }
    }

    pub(super) fn from_choice(
        target_id: &str,
        choice: SummaryChoice,
        custom_instructions: Option<String>,
    ) -> Self {
        match choice {
            SummaryChoice::NoSummary => Self::no_summary(target_id),
            SummaryChoice::Summarize | SummaryChoice::Custom => {
                Self::summarize(target_id, custom_instructions)
            }
        }
    }

    pub(super) fn into_options(self) -> NavigateTreeOptions {
        NavigateTreeOptions {
            summarize: self.summarize,
            custom_instructions: self.custom_instructions,
            replace_instructions: None,
            label: None,
        }
    }
}

/// How a navigation ended, projected for the UI.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TreeNavigationEnd {
    Completed { editor_text: Option<String> },
    Cancelled,
    Aborted,
    Failed(String),
    Error(String),
}

pub(super) fn classify_navigation(
    result: Result<NavigationResult, rpi_harness::result::HarnessError>,
) -> TreeNavigationEnd {
    match result {
        Ok(result) => match result.outcome {
            NavigationOutcome::Completed { editor_text, .. } => {
                TreeNavigationEnd::Completed { editor_text }
            }
            NavigationOutcome::Declined { .. } => TreeNavigationEnd::Cancelled,
            NavigationOutcome::Aborted { .. } => TreeNavigationEnd::Aborted,
            NavigationOutcome::Failed { error, .. } => TreeNavigationEnd::Failed(error.message),
        },
        Err(error) => TreeNavigationEnd::Error(error.to_string()),
    }
}

/// The status line for an outcome, if Pi shows one. A failure/error is rendered
/// as an error and has no status line.
pub(super) fn end_status(end: &TreeNavigationEnd) -> Option<&'static str> {
    match end {
        TreeNavigationEnd::Completed { .. } => Some(STATUS_NAVIGATED),
        TreeNavigationEnd::Cancelled => Some(STATUS_CANCELLED),
        TreeNavigationEnd::Aborted => Some(STATUS_SUMMARY_CANCELLED),
        TreeNavigationEnd::Failed(_) | TreeNavigationEnd::Error(_) => None,
    }
}

/// An aborted summary re-opens the tree selector with the same entry selected,
/// matching Pi's `showTreeSelector(entryId)` retry.
pub(super) fn reopens_tree_selector(end: &TreeNavigationEnd) -> bool {
    matches!(end, TreeNavigationEnd::Aborted)
}

/// Pi only replaces an empty editor: `result.editorText && !editor.getText().trim()`.
pub(super) fn editor_text_to_apply<'a>(
    editor_text: Option<&'a str>,
    current_editor_text: &str,
) -> Option<&'a str> {
    editor_text.filter(|text| !text.is_empty() && current_editor_text.trim().is_empty())
}

/// Build the bordered frame Pi uses for `showExtensionSelector` /
/// `showExtensionEditor`.
fn dialog_frame(title: &str, bold: bool, body: Arc<dyn Component>, hint: &str) -> Arc<Container> {
    let frame = Arc::new(Container::new());
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_title(title, bold));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(body);
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_hint(hint));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame
}

/// Show Pi's "Summarize branch?" selector for `target_id`.
///
/// Escape returns to the tree selector with the same entry highlighted;
/// selecting an answer and then a submit both send [`TuiMessage::TreeNavigate`]
/// so the async loop performs the navigation after the dialog is gone.
pub(super) fn open_summary_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
    target_id: &str,
) {
    let list = Arc::new(SelectList::new(summary_items(), 3));
    let frame = dialog_frame(SUMMARY_TITLE, true, list.clone(), SUMMARY_HINT);

    let state_select = state.clone();
    let ec_select = editor_container.clone();
    let editor_select = editor.clone();
    let tui_select = tui.clone();
    let tx_select = tx.clone();
    let target_select = target_id.to_string();
    list.on_select(Arc::new(move |item| {
        let Some(choice) = parse_summary_choice(&item.value) else {
            return;
        };
        close_selector(&state_select, &ec_select, &editor_select, &tui_select);
        if choice == SummaryChoice::Custom {
            open_custom_editor(
                &state_select,
                &ec_select,
                &editor_select,
                &tui_select,
                &tx_select,
                &target_select,
            );
            return;
        }
        let request = TreeNavigateRequest::from_choice(&target_select, choice, None);
        let _ = tx_select.send(TuiMessage::TreeNavigate(request));
    }));

    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    let tx_cancel = tx.clone();
    let target_cancel = target_id.to_string();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
        // Re-open through the loop: the tree must be read from durable storage.
        let _ = tx_cancel.send(TuiMessage::OpenTreeAt(target_cancel.clone()));
    }));

    open_selector_with_view(
        state,
        editor_container,
        editor,
        tui,
        SelectorView::List(list),
        frame,
        SelectorKind::TreeSummary,
    );
}

/// Show Pi's multi-line "Custom summarization instructions" editor.
///
/// The editor borrows the extension-dialog input slot so Escape/Ctrl+C reach
/// the same routing as a plugin dialog; its cancel callback re-opens the
/// summary selector (Pi loops back to it when the editor is cancelled).
pub(super) fn open_custom_editor(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
    target_id: &str,
) {
    let custom = Arc::new(Editor::new(
        EditorOptions {
            padding_x: 1,
            autocomplete_max_visible: 0,
            placeholder: Some("Instructions for the branch summary".to_string()),
            initial_text: None,
        },
        EditorStyle {
            prompt: "> ".to_string(),
            placeholder: String::new(),
        },
        Arc::new(rpi_tui::Keybindings::new()),
    ));
    custom.set_focused(true);
    let frame = dialog_frame(CUSTOM_TITLE, false, custom.clone(), CUSTOM_HINT);

    editor_container.clear();
    editor_container.add_child(frame);
    *state.active_extension_editor.lock().unwrap() = Some(custom.clone());
    *state.active_extension_input.lock().unwrap() = None;

    let state_submit = state.clone();
    let ec_submit = editor_container.clone();
    let editor_submit = editor.clone();
    let tui_submit = tui.clone();
    let tx_submit = tx.clone();
    let target_submit = target_id.to_string();
    custom.on_submit(Arc::new(move |text: &str| {
        close_extension_editor(&state_submit, &ec_submit, &editor_submit, &tui_submit);
        let request = TreeNavigateRequest::summarize(&target_submit, Some(text.to_string()));
        let _ = tx_submit.send(TuiMessage::TreeNavigate(request));
    }));

    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    let tx_cancel = tx.clone();
    let target_cancel = target_id.to_string();
    *state.active_extension_cancel.lock().unwrap() = Some(Arc::new(move || {
        close_extension_editor(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
        open_summary_selector(
            &state_cancel,
            &ec_cancel,
            &editor_cancel,
            &tui_cancel,
            &tx_cancel,
            &target_cancel,
        );
    }));

    tui.set_focus(Some(custom));
    tui.request_render(false);
}

/// The first stage after the tree selector: short-circuit the current leaf,
/// honour `branchSummary.skipPrompt`, or ask the summary question.
#[allow(clippy::too_many_arguments)]
pub(super) async fn select_tree_entry(
    harness: &AgentHarness,
    lane: &Arc<dyn AgentLane>,
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
    entry_id: &str,
) {
    match lane.get_leaf_id().await {
        Ok(leaf_id) => {
            if already_at_entry(leaf_id.as_deref(), entry_id) {
                add_note_message(&state.chat_container, STATUS_ALREADY_THERE);
                tui.request_render(false);
                return;
            }
        }
        Err(error) => {
            add_error_message(
                &state.chat_container,
                &format!("Could not read the current session entry: {error}"),
            );
            tui.request_render(false);
            return;
        }
    }

    if !should_prompt_for_summary(summary_prompt_skipped()) {
        navigate(
            harness,
            lane,
            state,
            editor_container,
            editor,
            tui,
            tx,
            TreeNavigateRequest::no_summary(entry_id),
        )
        .await;
        return;
    }

    open_summary_selector(state, editor_container, editor, tui, tx, entry_id);
}

/// Perform the navigation chosen in the dialogs.
///
/// Mirrors Pi's ordering: Stop an active response first (restoring queued
/// messages), then re-check for a concurrent compaction/tree navigation, then
/// navigate. A summary shows the working indicator while it runs so Escape can
/// cancel it and land on the same "Branch summarization cancelled" retry.
#[allow(clippy::too_many_arguments)]
pub(super) async fn navigate(
    harness: &AgentHarness,
    lane: &Arc<dyn AgentLane>,
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
    request: TreeNavigateRequest,
) {
    if *state.status.lock().unwrap() != RunStatus::Idle {
        restore_queued_messages_to_editor(lane.clone(), editor.clone(), state.clone(), tui.clone())
            .await;
        let _ = lane.abort().await;
    }

    // Recheck after the dialogs and streaming abort, before touching another
    // operation's UI (Pi's `isCompacting` guard).
    let busy = lane
        .snapshot()
        .await
        .ok()
        .and_then(|snapshot| snapshot.active)
        .map(|active| {
            matches!(
                active.kind,
                OperationKind::Compaction | OperationKind::Navigation
            )
        })
        .unwrap_or(false);
    if busy {
        add_error_message(&state.chat_container, BUSY_MESSAGE);
        tui.request_render(false);
        return;
    }

    let summarizing = request.summarize;
    let target_id = request.target_id.clone();
    if summarizing {
        state.set_status(RunStatus::Working);
        tui.request_render(false);
    }
    let result = lane
        .navigate_tree_with(Some(&target_id), request.into_options())
        .await;
    if summarizing {
        state.set_status(RunStatus::Idle);
    }

    let end = classify_navigation(result);
    match &end {
        TreeNavigationEnd::Completed { editor_text } => {
            state.chat_container.clear();
            add_welcome_message(&state.chat_container);
            render_session_history(
                harness,
                &state.chat_container,
                state.markdown_transformer(),
                Some(state.extension_session.clone()),
                state.images_visible(),
            )
            .await;
            if let Some(text) = editor_text_to_apply(editor_text.as_deref(), &editor.get_text()) {
                set_editor_text_caret_at_end(editor, text);
            }
        }
        TreeNavigationEnd::Failed(message) => {
            add_error_message(&state.chat_container, message);
        }
        TreeNavigationEnd::Error(message) => {
            add_error_message(
                &state.chat_container,
                &format!("Could not navigate session tree: {message}"),
            );
        }
        TreeNavigationEnd::Cancelled | TreeNavigationEnd::Aborted => {}
    }
    if let Some(status) = end_status(&end) {
        add_note_message(&state.chat_container, status);
    }
    if reopens_tree_selector(&end) {
        open_tree_selector(
            harness,
            state,
            editor_container,
            editor,
            tui,
            &state.chat_container,
            tx,
            Some(&target_id),
        )
        .await;
    }
    tui.request_render(false);
}

#[cfg(test)]
mod tests;
