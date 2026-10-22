//! Operation hooks run before mutation; completion carries the committed entry.
use super::*;
use crate::compaction::{CompactResult, CompactionError, CompactionPreparation};
use rpi_ai::{identity::RequestIdentity, ProviderHooks};

pub(super) struct SummaryProvider {
    pub(super) provider: Arc<dyn AiProvider>,
    pub(super) hooks: Arc<dyn ProviderHooks>,
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
    pub(super) fn public_model(model: &Model) -> serde_json::Value {
        serde_json::json!({"id":model.id,"provider":model.provider,"api":model.api,"name":model.name,
            "reasoning":model.reasoning,"thinkingLevelMap":model.thinking_level_map,"input":model.input,
            "cost":model.cost,"contextWindow":model.context_window,"maxTokens":model.max_tokens})
    }

    pub(super) async fn emit_configuration_event(&self, event: &str, data: serde_json::Value) {
        let (hooks, scope) = {
            let inner = self.inner.lock().unwrap();
            (inner.provider_hooks.clone(), inner.execution_scope.clone())
        };
        if let Some(hooks) = hooks {
            let identity =
                RequestIdentity::operation(Some(self.session.storage().metadata().id), event);
            let callback = rpi_ai::identity::scope(
                identity.clone(),
                hooks.lifecycle(event, data, identity, CancellationToken::new()),
            );
            let _ = match scope {
                Some(scope) => scope.run(callback).await,
                None => callback.await,
            };
        }
    }

    pub(super) async fn compaction_preparation_failed(
        &self,
        run_id: &str,
        error: &str,
        aborted: bool,
    ) {
        let hooks = self.inner.lock().unwrap().provider_hooks.clone();
        if let Some(hooks) = hooks {
            let identity = RequestIdentity {
                session_id: Some(self.session.storage().metadata().id),
                operation_id: run_id.into(),
                purpose: "compaction".into(),
                turn_id: None,
                request_id: None,
            };
            let _ = rpi_ai::identity::scope(
                identity.clone(),
                hooks.lifecycle(
                    "session_compact_failed",
                    compaction_failure(CompactionReason::Manual, false, aborted, Some(error)),
                    identity,
                    CancellationToken::new(),
                ),
            )
            .await;
        }
    }

    pub(super) async fn compact_with_extensions(
        &self,
        run_id: &str,
        prep: &CompactionPreparation,
        branch_entries: &[Entry],
        options: &CompactionLlmOptions,
        reason: CompactionReason,
    ) -> Result<Option<(CompactResult, Entry)>, CompactionError> {
        let metadata = self
            .session
            .get_metadata()
            .await
            .map_err(|e| CompactionError::summarization_failed(e.to_string()))?;
        let manual = matches!(reason, CompactionReason::Manual);
        let will_retry = matches!(reason, CompactionReason::Overflow);
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
            let mut from_extension = false;
            let outcome: Result<Option<(CompactResult, Entry)>, CompactionError> = async {
                let override_result = if let Some(hooks) = &hooks {
                    hooks
                        .lifecycle(
                            "session_before_compact",
                            serde_json::json!({"preparation":prep,"branchEntries":branch_entries,
                        "customInstructions":options.custom_instructions,"automatic":!manual,
                        "reason":reason,"willRetry":will_retry}),
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
                    from_extension = true;
                    serde_json::from_value::<CompactResult>(value.clone()).map_err(|_| {
                        CompactionError::summarization_failed("invalid extension compaction result")
                    })?
                } else {
                    let _progress =
                        crate::events::CompactionProgressGuard::new(&self.bus, &self.lane, manual);
                    compact(prep, &options).await?
                };
                if options.signal.is_cancelled() {
                    return Err(CompactionError::aborted("Compaction cancelled"));
                }
                let entry = self
                    .persist_compaction_entry(run_id, &result, reason)
                    .await
                    .map_err(|e| CompactionError::summarization_failed(e.to_string()))?;
                Ok(Some((result, entry)))
            }
            .await;
            if let Some(hooks) = &hooks {
                let (event, payload) = match &outcome {
                    Ok(Some((_, entry))) => (
                        "session_compact",
                        serde_json::json!({"compactionEntry":entry,"automatic":!manual,
                            "reason":reason,"willRetry":will_retry,"fromExtension":from_extension}),
                    ),
                    Err(error) => (
                        "session_compact_failed",
                        compaction_failure(
                            reason,
                            from_extension,
                            options.signal.is_cancelled()
                                || error.code
                                    == crate::compaction::compaction::CompactionErrorCode::Aborted,
                            Some(&error.message),
                        ),
                    ),
                    Ok(None) => (
                        "session_compact_failed",
                        compaction_failure(reason, from_extension, true, None),
                    ),
                };
                let _ = hooks
                    .lifecycle(event, payload, identity, CancellationToken::new())
                    .await;
            }
            outcome
        })
        .await
    }
}

fn compaction_failure(
    reason: CompactionReason,
    from_extension: bool,
    aborted: bool,
    error: Option<&str>,
) -> serde_json::Value {
    let mut payload = serde_json::json!({"reason":reason,"fromExtension":from_extension,
        "aborted":aborted,"willRetry":false,"automatic":reason != CompactionReason::Manual});
    if let Some(error) = error {
        payload["error"] = error.into();
        if !aborted {
            let prefix = match reason {
                CompactionReason::Manual => "Compaction failed",
                CompactionReason::Threshold => "Auto-compaction failed",
                CompactionReason::Overflow => "Context overflow recovery failed",
            };
            payload["errorMessage"] = format!("{prefix}: {error}").into();
        }
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_fields_match_original_pi_compaction_traces() {
        let reference: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../docs/provider-parity/compaction-reference.json"
        ))
        .unwrap();
        let mut checked = 0;
        for row in reference["rows"].as_array().unwrap() {
            let expected = row["events"].as_array().unwrap().last().unwrap();
            if expected["type"] != "session_compact_failed" {
                continue;
            }
            let mode = row["mode"].as_str().unwrap();
            let error = match mode {
                "empty" => "Nothing to compact (session too small)",
                "already" => "Already compacted",
                "signal" | "cancel" => "Compaction cancelled",
                _ => "fixture failure",
            };
            let mut actual = compaction_failure(
                serde_json::from_value(row["reason"].clone()).unwrap(),
                matches!(mode, "signal" | "extensionError"),
                matches!(mode, "signal" | "cancel"),
                Some(error),
            );
            let fields = actual.as_object_mut().unwrap();
            fields.remove("error");
            fields.remove("automatic");
            fields.insert("type".into(), "session_compact_failed".into());
            assert_eq!(&actual, expected, "{} / {mode}", row["reason"]);
            checked += 1;
        }
        assert_eq!(checked, 14);
    }
}
