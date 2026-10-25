//! Host/UI requests share one replacement coordinator; no presentation side effects.
use super::*;

#[derive(Debug)]
pub enum HostSessionRequest {
    New,
    Resume(String),
    Import(PathBuf),
    Fork {
        entry_id: Option<String>,
        position: rpi_harness::session::types::ForkPosition,
    },
}

pub async fn replace_host_session(
    harness: &AgentHarness,
    reload: &ReloadContext,
    request: HostSessionRequest,
) -> Result<SessionReplacement, String> {
    let dir = reload
        .args
        .session_dir
        .clone()
        .unwrap_or_else(|| default_session_dir(&reload.cwd));
    let factory = SessionRuntimeFactory::from_reload(reload);
    let lane = harness.lane("main");
    replace_session_request(
        &SessionReplacementContext {
            runtime_factory: Some(&factory),
            harness,
            lane: lane.as_ref(),
            extension_session: &reload.extension_session,
            action_bridge: &reload.action_bridge,
            tool_context: &reload.tool_context,
        },
        &reload.cwd.to_string_lossy(),
        &dir,
        request,
        true,
    )
    .await
}

/// Admit and run the before-hook *before* opening/creating the destination.
/// In particular a veto must not create a new/fork file or try opening a bad
/// resume path. `settle=false` is mandatory for actions invoked by the runtime.
pub(crate) async fn replace_session_request(
    ctx: &SessionReplacementContext<'_>,
    cwd: &str,
    dir: &Path,
    request: HostSessionRequest,
    settle: bool,
) -> Result<SessionReplacement, String> {
    let _admission = ctx
        .runtime_factory
        .map(|factory| factory.try_admit())
        .transpose()?;
    if !settle
        && ctx
            .harness
            .snapshot()
            .await
            .map_err(|e| e.to_string())?
            .active
            .is_some()
    {
        return Err("wait for the active operation to finish before replacing the session".into());
    }
    if ctx.runtime_factory.is_none() && !ctx.extension_session.lock().unwrap().is_empty() {
        return Err("session replacement requires a fresh runtime factory".into());
    }
    let (reason, entry_id, target) = match &request {
        HostSessionRequest::New => (SessionReplacementReason::New, None, None),
        HostSessionRequest::Resume(path) => {
            (SessionReplacementReason::Resume, None, Some(path.as_str()))
        }
        HostSessionRequest::Import(path) => (SessionReplacementReason::Import, None, path.to_str()),
        HostSessionRequest::Fork { entry_id, position } => (
            if *position == rpi_harness::session::types::ForkPosition::Before {
                SessionReplacementReason::ForkBefore
            } else {
                SessionReplacementReason::Fork
            },
            match entry_id {
                Some(id) => Some(id.clone()),
                None => ctx.lane.get_leaf_id().await.map_err(|e| e.to_string())?,
            },
            None,
        ),
    };
    if let Some(reason) = before_session_replacement(ctx, reason, entry_id.as_deref(), target).await
    {
        return Ok(SessionReplacement::Cancelled { reason });
    }
    let destination = match request {
        HostSessionRequest::New => {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            create_jsonl_session(dir, cwd)
                .await
                .map_err(|e| e.to_string())?
        }
        HostSessionRequest::Resume(id) => open_session_by_id_in_dir(&id, cwd, dir)
            .await
            .map_err(|e| e.to_string())?,
        HostSessionRequest::Import(path) => import_session_file(&path, cwd, dir).await?,
        HostSessionRequest::Fork { position, .. } => {
            fork_session_storage_at(ctx.harness, cwd, dir, entry_id.clone(), position).await?
        }
    };
    replace_active_session_inner(ctx, destination, reason, entry_id.as_deref(), settle, true).await
}
