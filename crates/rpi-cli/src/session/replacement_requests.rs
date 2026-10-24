//! Host/UI requests share one replacement coordinator; no presentation side effects.
use super::*;

#[derive(Debug)]
pub enum HostSessionRequest {
    New,
    Resume(String),
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
    let cwd = reload.cwd.to_string_lossy();
    let dir = reload
        .args
        .session_dir
        .clone()
        .unwrap_or_else(|| default_session_dir(&reload.cwd));
    let (destination, reason, entry_id) = match request {
        HostSessionRequest::New => {
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            (
                create_jsonl_session(&dir, &cwd)
                    .await
                    .map_err(|e| e.to_string())?,
                SessionReplacementReason::New,
                None,
            )
        }
        HostSessionRequest::Resume(id) => (
            open_session_by_id_in_dir(&id, &cwd, &dir)
                .await
                .map_err(|e| e.to_string())?,
            SessionReplacementReason::Resume,
            None,
        ),
        HostSessionRequest::Fork { entry_id, position } => {
            let entry_id = match entry_id {
                Some(id) => Some(id),
                None => harness
                    .lane("main")
                    .get_leaf_id()
                    .await
                    .map_err(|e| e.to_string())?,
            };
            let destination =
                fork_session_storage_at(harness, &cwd, &dir, entry_id.clone(), position).await?;
            let reason = if position == rpi_harness::session::types::ForkPosition::Before {
                SessionReplacementReason::ForkBefore
            } else {
                SessionReplacementReason::Fork
            };
            (destination, reason, entry_id)
        }
    };
    let factory = SessionRuntimeFactory::from_reload(reload);
    let lane = harness.lane("main");
    replace_active_session(
        &SessionReplacementContext {
            runtime_factory: Some(&factory),
            harness,
            lane: lane.as_ref(),
            extension_session: &reload.extension_session,
            action_bridge: &reload.action_bridge,
            tool_context: &reload.tool_context,
        },
        destination,
        reason,
        entry_id.as_deref(),
    )
    .await
}
