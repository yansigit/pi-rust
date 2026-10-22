//! Provider login prompts for the interactive TUI.
//!
//! Native provider authentication (`register_provider_auth`) is driven from the
//! TUI through the shared native-dialog port: every `AuthInteraction` is turned
//! into a declarative control that the local terminal renders with
//! [`crate::native_dialogs::NativeDialogView`] and that the RPC terminal renders
//! from the same `ui_dialog_request`/`ui_dialog_response` envelope. Both
//! terminals therefore validate, answer and cancel the same awaited control.
//!
//! The provider callback runs on the async runtime, not the key thread. The key
//! loop owns presentation: it drains the session's `UiDialogMailbox`, shows the
//! dialog and answers it, while this task polls the same mailbox and awaits the
//! reply. Cancelling the flow (Esc, session shutdown, or a reload that
//! invalidates the owning registry) cancels the pending prompt so it cannot
//! linger as an orphan.

use super::*;

use rpi_ai::auth::{AuthError, Credential, CredentialStore};
use rpi_extensions::provider_auth::{
    AuthAction, AuthInteraction, AuthResponseKind, ProviderAuthentication,
};
use rpi_extensions::UiDialogMailbox;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// Sink for flow-level notices. `true` marks an error.
pub(super) type AuthReport = Arc<dyn Fn(&str, bool) + Send + Sync>;

/// Whether `provider_id` registered a native authentication callback.
pub(super) fn has_native_auth(
    session: &crate::session::ExtensionSessionCell,
    provider_id: &str,
) -> bool {
    let session = session.lock().unwrap_or_else(|error| error.into_inner());
    rpi_extensions::provider_auth::NativeProviderAuth::from_session(&session, provider_id).is_some()
}

/// Cancels a still-open mailbox request when the awaiting future is dropped, so
/// an aborted login (task abort, session replacement) never leaves a visible
/// prompt behind.
struct PendingAsk {
    mailbox: UiDialogMailbox,
    id: String,
    active: bool,
}

impl Drop for PendingAsk {
    fn drop(&mut self) {
        if self.active {
            let _ = self.mailbox.cancel(&self.id);
        }
    }
}

/// Show one validated native dialog and await its answer. `Ok(None)` is a user
/// or owner cancellation (`null` in the native dialog contract).
async fn ask(
    mailbox: &UiDialogMailbox,
    spec: Value,
    signal: &CancellationToken,
) -> Result<Option<Value>, AuthError> {
    if signal.is_cancelled() {
        return Err(AuthError::Cancelled);
    }
    let request_id = uuid::Uuid::new_v4().to_string();
    mailbox
        .handle(json!({
            "op": "open",
            "requestId": request_id,
            "ui": {"nativeDialog": spec},
        }))
        .map_err(|_| AuthError::Resolve)?;
    let mut pending = PendingAsk {
        mailbox: mailbox.clone(),
        id: request_id.clone(),
        active: true,
    };
    loop {
        if signal.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        match mailbox.poll(&request_id) {
            Ok(value) => match value.get("status").and_then(Value::as_str) {
                Some("answered") => {
                    pending.active = false;
                    return Ok(Some(
                        value.get("answer").cloned().unwrap_or(Value::Null),
                    ));
                }
                Some("cancelled") => {
                    pending.active = false;
                    return Ok(None);
                }
                _ => {}
            },
            // The record was already reclaimed; treat it as cancelled.
            Err(_) => {
                pending.active = false;
                return Ok(None);
            }
        }
        tokio::select! {
            biased;
            _ = signal.cancelled() => return Err(AuthError::Cancelled),
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
        }
    }
}

fn input_description(message: Option<&str>, url: Option<&str>) -> Option<String> {
    let mut lines = Vec::new();
    if let Some(message) = message.filter(|text| !text.trim().is_empty()) {
        lines.push(message.to_string());
    }
    if let Some(url) = url.filter(|text| !text.trim().is_empty()) {
        lines.push(format!("Open: {url}"));
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// Present one provider interaction through the native dialog port and return
/// the continuation payload the provider expects.
async fn present(
    interaction: AuthInteraction,
    mailbox: &UiDialogMailbox,
    signal: &CancellationToken,
    report: &AuthReport,
) -> Result<Value, AuthError> {
    match interaction {
        AuthInteraction::Input {
            title,
            message,
            url,
            secret,
            allow_empty,
            continuation,
        } => {
            let spec = json!({
                "kind": "input",
                "title": title,
                "description": input_description(message.as_deref(), url.as_deref()),
                "masked": secret,
                "placeholder": if secret { "Paste the secret value" } else { "Type the value" },
            });
            loop {
                let Some(answer) = ask(mailbox, spec.clone(), signal).await? else {
                    return Err(AuthError::Cancelled);
                };
                let value = answer
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if allow_empty || !value.is_empty() {
                    return Ok(json!({"value": value, "continuation": continuation}));
                }
                report("A value is required.", true);
            }
        }
        AuthInteraction::Select {
            title,
            choices,
            continuation,
        } => {
            let spec = json!({
                "kind": "select",
                "title": title,
                "searchable": true,
                "choices": choices
                    .iter()
                    .map(|choice| json!({"value": choice.id, "label": choice.label}))
                    .collect::<Vec<_>>(),
            });
            let Some(answer) = ask(mailbox, spec, signal).await? else {
                return Err(AuthError::Cancelled);
            };
            let value = answer
                .get("values")
                .and_then(Value::as_array)
                .and_then(|values| values.first())
                .and_then(Value::as_str)
                .ok_or(AuthError::Resolve)?
                .to_string();
            Ok(json!({"value": value, "continuation": continuation}))
        }
        AuthInteraction::Wait {
            message,
            poll_after_ms,
            continuation,
        } => {
            if let Some(message) = message.filter(|text| !text.trim().is_empty()) {
                report(&message, false);
            }
            tokio::select! {
                biased;
                _ = signal.cancelled() => Err(AuthError::Cancelled),
                _ = tokio::time::sleep(std::time::Duration::from_millis(poll_after_ms)) => {
                    Ok(json!({"continuation": continuation}))
                }
            }
        }
    }
}

/// Drive a non-native (legacy OAuth or host API-key) registration through the
/// same awaited presentation callback as native login.
async fn drive<F, Fut>(
    auth: &ProviderAuthentication,
    method: &str,
    store: Arc<dyn CredentialStore>,
    signal: CancellationToken,
    mut interact: F,
) -> Result<(), AuthError>
where
    F: FnMut(AuthInteraction) -> Fut,
    Fut: std::future::Future<Output = Result<Value, AuthError>>,
{
    let mut action = AuthAction::Login;
    let mut input = json!({"methodId": method});
    loop {
        let response = auth.request(action, input, signal.clone()).await?;
        match response.kind {
            AuthResponseKind::Credential => {
                let credential: Credential =
                    serde_json::from_value(response.payload).map_err(|_| AuthError::Resolve)?;
                let manifest = auth.manifest();
                if !matches!(&credential, Credential::Oauth(_) if manifest.oauth)
                    && !matches!(&credential, Credential::ApiKey { .. } if manifest.api_key)
                {
                    return Err(AuthError::Resolve);
                }
                let mut transaction = store.lock(&manifest.id, signal).await?;
                return transaction.replace(credential).await;
            }
            AuthResponseKind::Interaction => {
                let interaction = AuthInteraction::parse(response.payload)?;
                input = tokio::select! {
                    biased;
                    _ = signal.cancelled() => return Err(AuthError::Cancelled),
                    result = interact(interaction) => result?,
                };
                action = AuthAction::Continue;
            }
            _ => return Err(AuthError::Resolve),
        }
    }
}

async fn select_method(
    auth: &ProviderAuthentication,
    methods: Vec<rpi_extensions::provider_auth::AuthMethod>,
    mailbox: &UiDialogMailbox,
    signal: &CancellationToken,
) -> Result<String, AuthError> {
    match methods.as_slice() {
        [only] => Ok(only.id.clone()),
        [] => Err(AuthError::Resolve),
        _ => {
            let spec = json!({
                "kind": "select",
                "title": format!("Log in to {}", auth.manifest().display_name),
                "searchable": true,
                "choices": methods
                    .iter()
                    .map(|method| json!({"value": method.id, "label": method.name}))
                    .collect::<Vec<_>>(),
            });
            let Some(answer) = ask(mailbox, spec, signal).await? else {
                return Err(AuthError::Cancelled);
            };
            answer
                .get("values")
                .and_then(Value::as_array)
                .and_then(|values| values.first())
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or(AuthError::Resolve)
        }
    }
}

/// Run one provider login end to end, presenting every interaction as a native
/// dialog. Only a committed credential returns `Ok`.
async fn login_with_dialogs(
    session: &rpi_extensions::ExtensionSession,
    provider: &str,
    mailbox: &UiDialogMailbox,
    signal: &CancellationToken,
    report: &AuthReport,
) -> Result<(), AuthError> {
    let auth = ProviderAuthentication::for_registered_provider(session, provider);
    let store: Arc<dyn CredentialStore> = Arc::new(
        crate::config::auth_store::CompatibilityCredentialStore::new(
            crate::config::auth_path().map_err(|_| AuthError::Store)?,
        )?,
    );
    let methods = match auth.as_ref() {
        ProviderAuthentication::Native(native) => native.methods(signal.clone()).await?,
        other => other.manifest().methods.clone(),
    };
    let method = select_method(auth.as_ref(), methods, mailbox, signal).await?;

    let mailbox_present = mailbox.clone();
    let signal_present = signal.clone();
    let report_present = report.clone();
    let interact = move |interaction: AuthInteraction| {
        let mailbox = mailbox_present.clone();
        let signal = signal_present.clone();
        let report = report_present.clone();
        async move { present(interaction, &mailbox, &signal, &report).await }
    };
    match auth.as_ref() {
        ProviderAuthentication::Native(native) => {
            native
                .login(&method, store, signal.clone(), interact)
                .await
        }
        _ => drive(auth.as_ref(), &method, store, signal.clone(), interact).await,
    }
}

/// Start an interactive provider login for `provider_id`. Native registrations
/// and host API-key providers go through the shared dialog driver; legacy OAuth
/// registrations keep their existing form in `settings.rs`.
pub(super) fn begin_auth_login(ctx: &CommandContext, provider_id: &str) {
    let state = ctx.state.clone();
    let tui = ctx.tui.clone();
    let chat = ctx.state.chat_container.clone();
    let tx = ctx.tx.clone();
    let editor = ctx.editor.clone();
    let editor_container = ctx.editor_container.clone();
    let session = ctx
        .reload_context
        .extension_session
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let mailbox = ctx.reload_context.ui_dialog.clone();
    let runtime = ctx.reload_context.runtime.clone();

    let flow_id = state
        .provider_auth_generation
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
        .wrapping_add(1);
    state
        .provider_auth_flow_id
        .store(flow_id, std::sync::atomic::Ordering::Release);
    show_provider_auth_status(
        &state,
        &editor_container,
        &editor,
        &tui,
        &format!("Starting login for {provider_id}…"),
    );

    let provider = provider_id.to_string();
    runtime.clone().spawn(async move {
        let signal = CancellationToken::new();
        let watchdog = {
            let state = state.clone();
            let signal = signal.clone();
            let snapshot = session.snapshot_arc();
            let runtime = runtime.clone();
            runtime.spawn(async move {
                loop {
                    if state.provider_auth_flow_id.load(std::sync::atomic::Ordering::Acquire)
                        != flow_id
                        || !snapshot.as_ref().is_some_and(|snapshot| snapshot.is_active())
                        || signal.is_cancelled()
                    {
                        signal.cancel();
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            })
        };
        let chat_report = chat.clone();
        let report: AuthReport = Arc::new(move |text, error| {
            if error {
                add_error_message(&chat_report, text);
            } else {
                add_note_message(&chat_report, text);
            }
        });
        let result = login_with_dialogs(&session, &provider, &mailbox, &signal, &report).await;
        watchdog.abort();
        if state
            .provider_auth_flow_id
            .load(std::sync::atomic::Ordering::Acquire)
            == flow_id
        {
            finish_provider_login(&state, &editor_container, &editor, &tui);
            match result {
                Ok(()) => {
                    add_note_message(
                        &chat,
                        &format!("Credentials saved for {provider}; reloading providers…"),
                    );
                    let _ = tx.send(TuiMessage::ReloadExtensions);
                }
                Err(AuthError::Cancelled) => {
                    add_note_message(&chat, &format!("Login for {provider} cancelled."));
                }
                Err(error) => {
                    add_error_message(&chat, &format!("Login failed for {provider}: {error}"));
                }
            }
        }
        tui.request_render(false);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_extensions::provider_auth::AuthChoice;
    use rpi_extensions::{ExtensionRegistry, ExtensionSession, PluginKeepalive};
    use std::sync::Mutex;

    /// Serializes `RPI_CODING_AGENT_DIR` and restores it on drop.
    struct TestConfigDir {
        _guard: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        prev: Option<std::ffi::OsString>,
    }

    impl TestConfigDir {
        fn new() -> Self {
            let guard = crate::config::test_support::env_lock().lock().unwrap();
            let prev = std::env::var_os(crate::config::CONFIG_DIR_ENV);
            let tmp = tempfile::tempdir().unwrap();
            std::env::set_var(crate::config::CONFIG_DIR_ENV, tmp.path());
            Self {
                _guard: guard,
                _tmp: tmp,
                prev,
            }
        }
    }

    impl Drop for TestConfigDir {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
            }
        }
    }

    fn test_session() -> ExtensionSession {
        ExtensionSession::from_parts(
            Arc::new(ExtensionRegistry::new().snapshot()),
            PluginKeepalive::empty(),
            vec![],
            None,
        )
    }

    fn no_report() -> AuthReport {
        Arc::new(|_, _| {})
    }

    async fn next_dialog(mailbox: &UiDialogMailbox) -> rpi_extensions::UiDialogRequest {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(request) = mailbox.take_pending() {
                    return request;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("a native dialog was presented")
    }

    fn spawn_login(
        session: ExtensionSession,
        mailbox: &UiDialogMailbox,
        signal: &CancellationToken,
    ) -> tokio::task::JoinHandle<Result<(), AuthError>> {
        let mailbox = mailbox.clone();
        let signal = signal.clone();
        let report = no_report();
        tokio::spawn(async move {
            login_with_dialogs(&session, "example", &mailbox, &signal, &report).await
        })
    }

    #[test]
    fn native_auth_detection_ignores_unregistered_providers() {
        let cell = Arc::new(Mutex::new(test_session()));
        assert!(!has_native_auth(&cell, "example"));
    }

    #[tokio::test]
    async fn api_key_login_presents_a_masked_dialog_and_persists_the_credential() {
        let _cfg = TestConfigDir::new();
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        let signal = CancellationToken::new();
        let task = spawn_login(test_session(), &mailbox, &signal);

        let request = next_dialog(&mailbox).await;
        let spec = request.ui.get("nativeDialog").expect("native dialog");
        assert_eq!(spec["kind"], "input");
        assert_eq!(spec["masked"], true);
        assert_eq!(spec["title"], "API key");
        assert!(!spec.to_string().contains("private-key"));
        mailbox
            .respond(
                &request.request_id,
                json!({"type": "input", "value": "private-key"}),
            )
            .unwrap();

        assert!(task.await.unwrap().is_ok());
        assert!(matches!(
            crate::config::read_auth().unwrap().get("example"),
            Some(crate::config::Credential::ApiKey { key: Some(key), .. }) if key == "private-key"
        ));
        assert!(mailbox.take_pending().is_none());
    }

    #[tokio::test]
    async fn cancelling_the_prompt_returns_cancelled_without_persisting() {
        let _cfg = TestConfigDir::new();
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        let signal = CancellationToken::new();
        let task = spawn_login(test_session(), &mailbox, &signal);

        let request = next_dialog(&mailbox).await;
        mailbox.cancel(&request.request_id).unwrap();
        assert!(matches!(
            task.await.unwrap(),
            Err(AuthError::Cancelled)
        ));
        assert!(!crate::config::read_auth().unwrap().contains_key("example"));
        assert!(!mailbox.is_pending(&request.request_id));
        assert_eq!(mailbox.pending_len(), 0);
    }

    #[tokio::test]
    async fn cancelling_the_flow_signal_releases_the_pending_prompt() {
        let _cfg = TestConfigDir::new();
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        let signal = CancellationToken::new();
        let task = spawn_login(test_session(), &mailbox, &signal);

        let request = next_dialog(&mailbox).await;
        signal.cancel();
        assert!(matches!(
            task.await.unwrap(),
            Err(AuthError::Cancelled)
        ));
        assert!(!mailbox.is_pending(&request.request_id));
        assert_eq!(mailbox.pending_len(), 0);
    }

    #[tokio::test]
    async fn detaching_the_ui_releases_the_pending_prompt() {
        let _cfg = TestConfigDir::new();
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        let signal = CancellationToken::new();
        let task = spawn_login(test_session(), &mailbox, &signal);

        let request = next_dialog(&mailbox).await;
        mailbox.detach();
        assert!(matches!(
            task.await.unwrap(),
            Err(AuthError::Cancelled)
        ));
        assert!(!mailbox.is_pending(&request.request_id));
        assert_eq!(mailbox.pending_len(), 0);
    }

    #[tokio::test]
    async fn select_interaction_uses_a_searchable_stable_id_dialog() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        let signal = CancellationToken::new();
        let report = no_report();
        let mailbox_task = mailbox.clone();
        let signal_task = signal.clone();
        let task = tokio::spawn(async move {
            present(
                AuthInteraction::Select {
                    title: "Pick an account".into(),
                    choices: vec![
                        AuthChoice {
                            id: "a".into(),
                            label: "First".into(),
                        },
                        AuthChoice {
                            id: "b".into(),
                            label: "Second".into(),
                        },
                    ],
                    continuation: json!({"state": "private"}),
                },
                &mailbox_task,
                &signal_task,
                &report,
            )
            .await
        });

        let request = next_dialog(&mailbox).await;
        let spec = request.ui.get("nativeDialog").expect("native dialog");
        assert_eq!(spec["kind"], "select");
        assert_eq!(spec["searchable"], true);
        assert_eq!(spec["choices"][1]["value"], "b");
        assert_eq!(spec["choices"][1]["label"], "Second");
        assert!(!spec.to_string().contains("private"));
        mailbox
            .respond(
                &request.request_id,
                json!({"type": "select", "values": ["b"], "query": "sec"}),
            )
            .unwrap();
        assert_eq!(
            task.await.unwrap().unwrap(),
            json!({"value": "b", "continuation": {"state": "private"}})
        );
    }

    #[tokio::test]
    async fn wait_interaction_reports_its_message_and_returns_the_continuation() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        let signal = CancellationToken::new();
        let notes = Arc::new(Mutex::new(Vec::<(String, bool)>::new()));
        let report: AuthReport = {
            let notes = notes.clone();
            Arc::new(move |text, error| notes.lock().unwrap().push((text.to_string(), error)))
        };
        let value = present(
            AuthInteraction::Wait {
                message: Some("Waiting for the provider…".into()),
                poll_after_ms: 1,
                continuation: json!({"token": "private"}),
            },
            &mailbox,
            &signal,
            &report,
        )
        .await
        .unwrap();
        assert_eq!(value, json!({"continuation": {"token": "private"}}));
        assert_eq!(
            notes.lock().unwrap().as_slice(),
            [("Waiting for the provider…".to_string(), false)]
        );
        assert!(mailbox.take_pending().is_none());
    }
}

