//! Operation hooks run before mutation; completion carries the committed entry.
use super::*;
use crate::compaction::{CompactResult, CompactionError, CompactionPreparation};
use rpi_ai::{identity::RequestIdentity, ProviderHooks};

struct SummaryProvider {
    provider: Arc<dyn AiProvider>,
    hooks: Arc<dyn ProviderHooks>,
}
#[async_trait::async_trait]
impl AiProvider for SummaryProvider {
    fn id(&self) -> &str {
        self.provider.id()
    }
    fn models(&self) -> &[Model] {
        self.provider.models()
    }
    async fn stream_simple(
        &self,
        model: &Model,
        context: &rpi_ai::Context,
        options: &rpi_ai::SimpleStreamOptions,
    ) -> rpi_ai::AssistantMessageEventStream {
        let mut options = options.clone();
        if let Some(patch) = self.hooks.before_request(model, context, &options) {
            patch.apply(&mut options);
        }
        self.provider.stream_simple(model, context, &options).await
    }
}
impl AgentHarness {
    pub(super) async fn compact_with_extensions(
        &self,
        run_id: &str,
        prep: &CompactionPreparation,
        options: &CompactionLlmOptions,
        reason: CompactionReason,
    ) -> Result<Option<(CompactResult, Entry)>, CompactionError> {
        let metadata = self
            .session
            .get_metadata()
            .await
            .map_err(|e| CompactionError::summarization_failed(e.to_string()))?;
        let manual = matches!(reason, CompactionReason::Manual);
        let identity = RequestIdentity {
            session_id: Some(metadata.id),
            operation_id: if manual {
                run_id.to_owned()
            } else {
                self.session.id_generator().next()
            },
            purpose: "compaction".into(),
            turn_id: None,
            request_id: None,
        };
        let hooks = self.inner.lock().unwrap().provider_hooks.clone();
        let mut options = options.clone();
        if let Some(hooks) = &hooks {
            options.provider = Arc::new(SummaryProvider {
                provider: options.provider.clone(),
                hooks: hooks.clone(),
            });
        }
        rpi_ai::identity::scope(identity.clone(), async {
            let outcome: Result<Option<(CompactResult, Entry)>, CompactionError> = async {
                let override_result = if let Some(hooks) = &hooks {
                    hooks
                        .lifecycle(
                            "session_before_compact",
                            serde_json::json!({"preparation":prep,
                        "customInstructions":options.custom_instructions,"automatic":!manual}),
                            identity.clone(),
                            options.signal.clone(),
                        )
                        .await
                        .map_err(CompactionError::summarization_failed)?
                } else {
                    None
                };
                if override_result
                    .as_ref()
                    .is_some_and(|result| result["cancel"] == true)
                {
                    return Ok(None);
                }
                let result = if let Some(value) = override_result
                    .as_ref()
                    .and_then(|result| result.get("compaction"))
                {
                    serde_json::from_value::<CompactResult>(value.clone()).map_err(|_| {
                        CompactionError::summarization_failed("invalid extension compaction result")
                    })?
                } else {
                    let _progress =
                        crate::events::CompactionProgressGuard::new(&self.bus, &self.lane, manual);
                    compact(prep, &options).await?
                };
                let entry = self
                    .persist_compaction_entry(run_id, &result, reason)
                    .await
                    .map_err(|e| CompactionError::summarization_failed(e.to_string()))?;
                Ok(Some((result, entry)))
            }
            .await;
            if let Some(hooks) = &hooks {
                let completion = match &outcome {
                    Ok(Some((_, entry))) => Some((
                        "session_compact",
                        serde_json::json!({"compactionEntry":entry,"automatic":!manual}),
                    )),
                    Err(error) => Some((
                        "session_compact_failed",
                        serde_json::json!({"error":error.message,"automatic":!manual}),
                    )),
                    Ok(None) => None,
                };
                if let Some((event, payload)) = completion {
                    let _ = hooks
                        .lifecycle(event, payload, identity, CancellationToken::new())
                        .await;
                }
            }
            outcome
        })
        .await
    }
}
