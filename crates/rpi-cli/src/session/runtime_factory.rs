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

    pub(super) fn validate_destination(&self, file: Option<&str>) -> Result<(), String> {
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
        let cwd = header
            .get("cwd")
            .and_then(|cwd| cwd.as_str())
            .unwrap_or_else(|| self.cwd.to_str().unwrap_or(""));
        let destination = Path::new(cwd);
        if !destination.is_dir() {
            return Err(format!("Session working directory does not exist: {cwd}"));
        }
        if destination.canonicalize().map_err(|e| e.to_string())?
            != self.cwd.canonicalize().map_err(|e| e.to_string())?
        {
            return Err(
                "cross-cwd session replacement requires rebuilding cwd-bound services".into(),
            );
        }
        Ok(())
    }

    /// Register against a NEW bridge only after outgoing shutdown/invalidation.
    /// Reopening a library is permitted; reusing its closed registration is not.
    pub(super) async fn rebuild(
        &self,
        ctx: &SessionReplacementContext<'_>,
        paths: Vec<PathBuf>,
        old_bridge: Arc<rpi_extensions::ActionBridge>,
    ) -> Result<(), String> {
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
            return Err("Failed to construct fresh extension session runtime".into());
        }
        let initialization =
            crate::provider_runtime::ProviderRuntime::initialize(&session, &bridge).await?;
        let providers = self
            .providers
            .upgrade()
            .ok_or("session runtime owner was released")?;
        let providers = providers.read().unwrap_or_else(|e| e.into_inner()).clone();
        let runtime = crate::provider_runtime::ProviderRuntime::new(
            &session,
            &bridge,
            build_models_with_extensions_for_reload(&providers, &session, self.runtime.clone()),
            crate::provider::catalog_with_manifests(&crate::provider::manifests_from_session(
                &session,
            ))
            .map_err(|e| e.to_string())?,
        )?;
        ctx.harness
            .set_models(runtime.providers().await)
            .await
            .map_err(|e| e.to_string())?;
        runtime.activate();
        if let Some(cell) = self.provider_runtime.upgrade() {
            *cell.write().unwrap_or_else(|e| e.into_inner()) = runtime;
        }
        drop(initialization);
        let emitter: Arc<dyn rpi_agent::AgentEmitter> = match session.snapshot_arc() {
            Some(snapshot) => Arc::new(TeeEmitter::new(vec![
                self.broadcast.clone(),
                Arc::new(ExtensionEmitter::new(snapshot, session.keepalive())),
            ])),
            None => self.broadcast.clone(),
        };
        ctx.harness
            .set_agent_emitter(Some(emitter))
            .await
            .map_err(|e| e.to_string())?;
        let (before, after) = extension_tool_hooks(
            &session,
            ctx.extension_session.clone(),
            self.ui_dialog.clone(),
        );
        ctx.harness
            .set_tool_call_hooks(before, after)
            .await
            .map_err(|e| e.to_string())?;
        ctx.harness
            .set_provider_hooks(
                rpi_extensions::ExtensionProviderHooks::from_session(&session)
                    .map(|h| Arc::new(h) as Arc<dyn rpi_ai::ProviderHooks>),
            )
            .await
            .map_err(|e| e.to_string())?;
        let env: Arc<dyn rpi_tools::ExecutionEnv> = self.env.clone();
        let mut_env: Arc<dyn rpi_tools::MutatingEnv> = self.env.clone();
        let tool_ctx = rpi_tools::ExecutionToolContext::new(env, Some(mut_env));
        let mut tools = build_tools(&tool_ctx, &self.args);
        merge_extension_tools(&mut tools, &session, &self.args, ctx.tool_context);
        let active = filter_active_tool_names(active_tool_names(&tools, &self.args), &self.args);
        ctx.harness
            .set_tools(tools, Some(active))
            .await
            .map_err(|e| e.to_string())?;
        *ctx.extension_session.lock().unwrap() = session;
        *ctx.action_bridge.lock().unwrap() = Some(bridge);
        Ok(())
    }
}
