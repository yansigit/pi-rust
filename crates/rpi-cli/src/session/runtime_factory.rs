//! Fresh session authority; library handles alone never authorize a new session.
use super::*;

/// Reusable construction inputs, deliberately without strong extension/bridge or
/// provider-runtime cells. A bridge owns its host, so retaining those cells here
/// would make a library -> bridge -> host -> factory -> library cycle.
#[derive(Clone)]
pub struct SessionRuntimeFactory {
    gate: Arc<tokio::sync::Mutex<()>>,
    cwd: PathBuf,
    args: Args,
    env: Arc<OsExecutionEnv>,
    broadcast: Arc<dyn rpi_agent::AgentEmitter>,
    providers: std::sync::Weak<std::sync::RwLock<Vec<Arc<dyn Provider>>>>,
    provider_runtime:
        std::sync::Weak<std::sync::RwLock<Arc<crate::provider_runtime::ProviderRuntime>>>,
    runtime: tokio::runtime::Handle,
    mailbox: rpi_extensions::ReloadMailbox,
    ui_dialog: rpi_extensions::UiDialogMailbox,
    ext_status: rpi_extensions::ExtensionStatusMailbox,
    editor_text: rpi_extensions::EditorTextMailbox,
}

/// Result of [`SessionRuntimeFactory::rebuild`].
///
/// The durable session is already the replacement by the time a rebuild runs, so
/// a failure can never leave the outgoing authority alive. What differs is
/// whether the harness was touched: a staged failure applies nothing, while a
/// degraded rebuild has installed a partial replacement (B-1/B-2).
pub(crate) enum RebuildOutcome {
    Installed,
    /// Construction failed before the first harness mutation. Nothing was
    /// applied; the durable session stays selected and a retry is safe.
    Failed(String),
    /// A harness mutation landed but a later one failed. The replacement session
    /// has no (or partial) extension authority; the caller must surface that.
    Degraded(String),
}

/// Durable marker written next to an imported log.
///
/// `import_session_file` keeps the source bytes byte-identical (Pi never
/// rewrites an imported archive), so the marker - not the header `cwd` - is what
/// tells import and resume apart (B-3).
pub(crate) fn import_marker_path(session_file: &str) -> String {
    format!("{session_file}.import")
}

/// Provenance of an imported archive, when `session_file` is one.
fn import_provenance(session_file: &str) -> Option<String> {
    let marker = std::fs::read_to_string(import_marker_path(session_file)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&marker).ok()?;
    let source = value.get("source").and_then(|s| s.as_str())?;
    Some(source.to_string())
}

impl SessionRuntimeFactory {
    pub fn from_reload(ctx: &ReloadContext) -> Self {
        Self {
            gate: ctx.replacement_gate.clone(),
            cwd: ctx.cwd.clone(),
            args: ctx.args.clone(),
            env: ctx.execution_env.clone(),
            broadcast: ctx.broadcast.clone(),
            providers: Arc::downgrade(&ctx.providers),
            provider_runtime: Arc::downgrade(&ctx.provider_runtime),
            runtime: ctx.runtime.clone(),
            mailbox: ctx.mailbox.clone(),
            ui_dialog: ctx.ui_dialog.clone(),
            ext_status: ctx.ext_status.clone(),
            editor_text: ctx.editor_text.clone(),
        }
    }

    pub(super) fn try_admit(&self) -> Result<tokio::sync::OwnedMutexGuard<()>, String> {
        self.gate
            .clone()
            .try_lock_owned()
            .map_err(|_| "session replacement is already in progress".into())
    }

    /// Refuse a destination this process cannot serve, before any teardown.
    ///
    /// The rule is per reason so `Import` and `Resume` agree on one answer:
    ///
    /// - `Import` copies history into the caller's repository and keeps the
    ///   source header verbatim, so the header `cwd` is provenance. The copy runs
    ///   in *this* cwd, which is what has to exist (Pi's `importFromJsonl` opens
    ///   the copy with a `cwdOverride`, `agent-session-runtime.js`).
    /// - `Resume` of an imported archive is refused explicitly: imports are
    ///   archive-only and Pi v1-v3 logs are not resumable, so the refusal names
    ///   the archive instead of leaking a cross-cwd error from the header.
    /// - Any other reason restores in the session's own cwd. Pi switches the
    ///   runtime to it (`session-cwd.js:2-37` only checks that it exists); rpi v1
    ///   cannot rebuild cwd-bound services, so a different cwd is refused.
    pub(super) fn validate_destination(
        &self,
        file: Option<&str>,
        reason: SessionReplacementReason,
    ) -> Result<(), String> {
        validate_destination_cwd(&self.cwd, file, reason)
    }

    /// Register against a NEW bridge only after outgoing shutdown/invalidation.
    /// Reopening a library is permitted; reusing its closed registration is not.
    ///
    /// Every fallible step runs before the first harness mutation (B-1): the
    /// fresh extension session, its provider runtime and one credential pass are
    /// built first, so a construction failure applies nothing at all. What can
    /// still fail afterwards is a harness setter on a harness that went away,
    /// which is reported as [`RebuildOutcome::Degraded`] rather than as success
    /// (B-2), and never as a silent drop of the new runtime (B-5).
    pub(crate) async fn rebuild(
        &self,
        ctx: &SessionReplacementContext<'_>,
        paths: Vec<PathBuf>,
        old_bridge: Arc<rpi_extensions::ActionBridge>,
    ) -> RebuildOutcome {
        // The replacement build must not outlive its owners.
        let (providers, cell) = match (self.providers.upgrade(), self.provider_runtime.upgrade()) {
            (Some(providers), Some(cell)) => (providers, cell),
            _ => return RebuildOutcome::Failed("session runtime owner was released".into()),
        };
        let providers = providers.read().unwrap_or_else(|e| e.into_inner()).clone();
        let bridge = rpi_extensions::ActionBridge::with_reload_and_ui(
            self.runtime.clone(),
            old_bridge.clone_host(),
            rpi_extensions::reload_callback_from_mailbox(self.mailbox.clone()),
            self.ui_dialog.clone(),
            self.ext_status.clone(),
            self.editor_text.clone(),
        );
        let session = rpi_extensions::load_session_mixed(
            &[],
            &paths,
            Arc::new(NullDiagnostics),
            Some(bridge.clone()),
        );
        if session.loaded_paths().len() != paths.len() {
            session.begin_shutdown();
            bridge.invalidate();
            return RebuildOutcome::Failed(
                "Failed to construct fresh extension session runtime".into(),
            );
        }
        // Any later setup error must revoke this unpublished generation too.
        // Never leave a retained callback with a live bridge on a failed build.
        struct Unpublished {
            session: rpi_extensions::ExtensionSession,
            bridge: Arc<rpi_extensions::ActionBridge>,
            runtime: tokio::runtime::Handle,
            armed: bool,
        }
        impl Unpublished {
            /// Await the teardown on a failure path: `Drop` cannot, and the
            /// credential port would otherwise leak (B-4).
            async fn retire(mut self) {
                self.armed = false;
                self.bridge.invalidate();
                if let Some(snapshot) = self.session.snapshot_arc() {
                    snapshot.active_flag().store(false, Ordering::SeqCst);
                }
                self.session.auth_runtime().shutdown().await;
                self.session.begin_shutdown();
            }
        }
        impl Drop for Unpublished {
            fn drop(&mut self) {
                if self.armed {
                    self.bridge.invalidate();
                    if let Some(snapshot) = self.session.snapshot_arc() {
                        snapshot.active_flag().store(false, Ordering::SeqCst);
                    }
                    // Last resort (a panic in the staged build): hand the
                    // credential-port teardown to the runtime.
                    let session = self.session.clone();
                    self.runtime
                        .spawn(async move { session.auth_runtime().shutdown().await });
                    self.session.begin_shutdown();
                }
            }
        }
        let mut unpublished = Unpublished {
            session: session.clone(),
            bridge: bridge.clone(),
            runtime: self.runtime.clone(),
            armed: true,
        };

        // ---- Staged build: nothing below this point touches the harness. ----
        let initialization =
            match crate::provider_runtime::ProviderRuntime::initialize(&session, &bridge).await {
                Ok(initialization) => initialization,
                Err(error) => {
                    unpublished.retire().await;
                    return RebuildOutcome::Failed(error);
                }
            };
        let discovery = match crate::provider::catalog_with_manifests(
            &crate::provider::manifests_from_session(&session),
        ) {
            Ok(discovery) => discovery,
            Err(error) => {
                unpublished.retire().await;
                return RebuildOutcome::Failed(error.to_string());
            }
        };
        let runtime = match crate::provider_runtime::ProviderRuntime::new(
            &session,
            &bridge,
            build_models_with_extensions_for_reload(&providers, &session, self.runtime.clone()),
            discovery,
        ) {
            Ok(runtime) => runtime,
            Err(error) => {
                unpublished.retire().await;
                return RebuildOutcome::Failed(error);
            }
        };
        // Pi refreshes a fresh model runtime on create (`model-runtime.js:79-108`
        // create -> refresh with `allowModelNetwork:false`). Rebuild is exactly
        // that boundary: without this pass the replaced session publishes no
        // credential state and every provider reads `Unknown` (X-1).
        if let Err(error) = runtime.refresh_availability_on_create().await {
            unpublished.retire().await;
            return RebuildOutcome::Failed(error);
        }
        let models = runtime.providers().await;
        let emitter: Arc<dyn rpi_agent::AgentEmitter> = match session.snapshot_arc() {
            Some(snapshot) => Arc::new(TeeEmitter::new(vec![
                self.broadcast.clone(),
                Arc::new(ExtensionEmitter::new(snapshot, session.keepalive())),
            ])),
            None => self.broadcast.clone(),
        };
        let (before, after) = extension_tool_hooks(
            &session,
            ctx.extension_session.clone(),
            self.ui_dialog.clone(),
        );
        let provider_hooks = rpi_extensions::ExtensionProviderHooks::from_session(&session)
            .map(|h| Arc::new(h) as Arc<dyn rpi_ai::ProviderHooks>);
        let env: Arc<dyn rpi_tools::ExecutionEnv> = self.env.clone();
        let mut_env: Arc<dyn rpi_tools::MutatingEnv> = self.env.clone();
        let tool_ctx = rpi_tools::ExecutionToolContext::new(env, Some(mut_env));
        let mut tools = build_tools(&tool_ctx, &self.args);
        merge_extension_tools(&mut tools, &session, &self.args, ctx.tool_context);
        let active = filter_active_tool_names(active_tool_names(&tools, &self.args), &self.args);

        // ---- Apply: the first harness mutation, and the only remaining
        // failures. A failure here leaves the replacement without authority. ----
        let degraded = |step: &str, error: String| {
            RebuildOutcome::Degraded(format!(
                "failed to install the replacement's {step}: {error}"
            ))
        };
        if let Err(error) = ctx.harness.set_models(models).await {
            return degraded("model runtime", error.to_string());
        }
        if let Err(error) = ctx.harness.set_agent_emitter(Some(emitter)).await {
            return degraded("event emitter", error.to_string());
        }
        if let Err(error) = ctx.harness.set_tool_call_hooks(before, after).await {
            return degraded("tool hooks", error.to_string());
        }
        if let Err(error) = ctx.harness.set_provider_hooks(provider_hooks).await {
            return degraded("provider hooks", error.to_string());
        }
        if let Err(error) = ctx.harness.set_tools(tools, Some(active)).await {
            return degraded("tools", error.to_string());
        }
        // Publish before activate (B-5): the host port must resolve through the
        // cell, otherwise a request can reach a runtime no reader can see.
        *cell.write().unwrap_or_else(|e| e.into_inner()) = runtime.clone();
        runtime.activate();
        drop(initialization);
        unpublished.armed = false;
        *ctx.extension_session.lock().unwrap() = session;
        *ctx.action_bridge.lock().unwrap() = Some(bridge);
        RebuildOutcome::Installed
    }
}

/// [`SessionRuntimeFactory::validate_destination`] over an explicit cwd, so the
/// rule for each replacement reason is exercisable without a live factory.
pub(super) fn validate_destination_cwd(
    cwd: &Path,
    file: Option<&str>,
    reason: SessionReplacementReason,
) -> Result<(), String> {
    // Check existence before canonicalizing: a missing directory would otherwise
    // surface as a bare io error instead of the actionable message.
    if !cwd.is_dir() {
        return Err(format!(
            "Session working directory does not exist: {}",
            cwd.display()
        ));
    }
    let cwd = cwd.canonicalize().map_err(|e| e.to_string())?;
    if let Some(source) = file.and_then(import_provenance) {
        if reason == SessionReplacementReason::Import {
            return Ok(());
        }
        return Err(format!(
            "imported session from {source} is archive-only: Pi v1-v3 session logs \
             are not resumable; start a new session or re-import it"
        ));
    }
    // Native CLI sessions are JSONL. The shared SessionMetadata intentionally
    // has no cwd; validate the durable header rather than assuming the host cwd.
    use std::io::BufRead;
    let header = match file {
        Some(file) => {
            let line =
                std::io::BufReader::new(std::fs::File::open(file).map_err(|e| e.to_string())?)
                    .lines()
                    .next()
                    .transpose()
                    .map_err(|e| e.to_string())?
                    .ok_or("session file is empty")?;
            serde_json::from_str::<serde_json::Value>(&line).map_err(|e| e.to_string())?
        }
        None => serde_json::Value::Null,
    };
    let header_cwd = header
        .get("cwd")
        .and_then(|cwd| cwd.as_str())
        .unwrap_or_else(|| cwd.to_str().unwrap_or(""));
    let destination = Path::new(header_cwd);
    if !destination.is_dir() {
        return Err(format!(
            "Session working directory does not exist: {header_cwd}"
        ));
    }
    if destination.canonicalize().map_err(|e| e.to_string())? != cwd {
        return Err("cross-cwd session replacement requires rebuilding cwd-bound services".into());
    }
    Ok(())
}

#[cfg(test)]
mod validate_destination_tests {
    use super::*;

    /// B-3: `Import` is validated, not bypassed — the cwd the copy runs in must
    /// exist — and it accepts the archive it is creating. `Resume` of the same
    /// archive is refused by the archive rule, never by the header cwd that the
    /// verbatim copy preserves.
    #[test]
    fn import_is_validated_and_only_import_accepts_an_archive() {
        let temporary = tempfile::tempdir().unwrap();
        let cwd = temporary.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let archive = temporary.path().join("archive.jsonl");
        std::fs::write(&archive, "{\"cwd\":\"/elsewhere\"}\n").unwrap();
        std::fs::write(
            import_marker_path(&archive.to_string_lossy()),
            "{\"source\":\"/elsewhere/log.jsonl\"}",
        )
        .unwrap();
        let archive = archive.to_string_lossy().into_owned();

        assert_eq!(
            validate_destination_cwd(&cwd, Some(&archive), SessionReplacementReason::Import),
            Ok(())
        );
        let refused =
            validate_destination_cwd(&cwd, Some(&archive), SessionReplacementReason::Resume)
                .expect_err("resuming an archive must be refused");
        assert!(refused.contains("archive-only"), "{refused}");
        assert!(!refused.contains("cross-cwd"), "{refused}");

        // With the destination gone the same archive is refused for import too:
        // import goes through validation rather than around it (B-3).
        let missing = temporary.path().join("gone");
        let refused =
            validate_destination_cwd(&missing, Some(&archive), SessionReplacementReason::Import)
                .expect_err("import must validate the destination cwd");
        assert!(refused.contains("does not exist"), "{refused}");
    }
}
