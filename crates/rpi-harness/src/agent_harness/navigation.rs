//! Pi `navigateTree`: `session_before_tree` hooks, an optional branch summary,
//! a branch move guarded by the revision captured at admission, then
//! `session_tree`. Cancelled, aborted and failed navigations that reached the
//! before hook end with the native-only `session_tree_failed`.
use super::extension_events::SummaryProvider;
use super::*;
use crate::compaction::branch_summary::{
    collect_entries_for_branch_summary, generate_branch_summary, GenerateBranchSummaryOptions,
};
use crate::compaction::compaction::CompactionErrorCode;
use crate::messages::CUSTOM_ROLE;
use crate::session::context::derive_session_context_state;
use crate::session::types::{AppendTarget, ProvisionedEntry, ProvisionedKind};
use rpi_ai::identity::RequestIdentity;
use serde_json::{json, Map, Value};

/// How a navigation ended without `session_tree`.
enum Ended {
    Cancelled,
    Aborted,
    Failed(OperationError),
}

struct Navigated {
    summary_entry: Option<Entry>,
    editor_text: Option<String>,
    from_extension: bool,
}

fn error(code: &str, message: impl Into<String>) -> OperationError {
    OperationError {
        code: code.into(),
        message: message.into(),
    }
}

/// JavaScript truthiness, for hook results Pi tests with `if (value)`.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Value::String(value) => !value.is_empty(),
        _ => true,
    }
}

/// Pi `contentText(content, "")`.
fn content_text(content: &[rpi_ai::types::Content]) -> String {
    rpi_ai::types::Content::text_only(content, "")
}

impl AgentHarness {
    pub(super) async fn navigate_core(
        &self,
        target_id: Option<&str>,
        options: NavigateTreeOptions,
    ) -> HarnessResult<NavigationResult> {
        let (run_id, signal, _idle) = self.acquire_run(OperationKind::Navigation).await?;
        let _run_lease = ActiveRunLease {
            harness: self,
            run_id: run_id.clone(),
        };
        let recovery_tools = self.get_tools().await?;
        Self::finish_interrupted_operation(&self.session, &self.lane, &recovery_tools, &signal, true)
            .await?;
        let storage = self.session.storage();
        let branch = storage
            .branch_revision(&self.lane)
            .await
            .map_err(session_to_harness_err)?;
        let old_leaf = branch.leaf_id.clone();
        let result = |outcome| NavigationResult {
            run_id: run_id.clone(),
            outcome,
        };
        let target = target_id.filter(|id| !id.is_empty());
        if target.map(str::to_owned) == old_leaf {
            return Ok(result(NavigationOutcome::Completed {
                new_leaf_id: old_leaf,
                summary_entry: None,
                editor_text: None,
            }));
        }
        let Some(target) = target else {
            // A root target has no entry, so Pi's hooks do not apply; rpi's fork
            // path uses it to reset the branch.
            return Ok(result(
                match storage
                    .move_lane_at_revision(&self.lane, branch.revision, None)
                    .await
                {
                    Ok(_) => NavigationOutcome::Completed {
                        new_leaf_id: None,
                        summary_entry: None,
                        editor_text: None,
                    },
                    Err(e) => NavigationOutcome::Failed {
                        leaf_id: old_leaf,
                        error: error("navigation_failed", e.to_string()),
                    },
                },
            ));
        };
        let Some(target_entry) = self
            .session
            .get_entry(target)
            .await
            .map_err(session_to_harness_err)?
        else {
            return Ok(result(NavigationOutcome::Failed {
                leaf_id: old_leaf,
                error: error("entry_not_found", format!("Entry {target} not found")),
            }));
        };

        let summary_entry_id = options.summarize.then(|| self.session.id_generator().next());
        self.write_operation_started(
            &run_id,
            old_leaf.clone(),
            OperationIntent::Navigation {
                target_id: Some(target.to_owned()),
                summarize: options.summarize,
                custom_instructions: options.custom_instructions.clone(),
                label: options.label.clone(),
                summary_entry_id: summary_entry_id.clone(),
            },
        )
        .await?;
        let identity = RequestIdentity {
            session_id: Some(storage.metadata().id),
            operation_id: run_id.clone(),
            purpose: "tree".into(),
            turn_id: None,
            request_id: None,
        };
        let hooks = self.inner.lock().unwrap().provider_hooks.clone();
        let mut began = false;
        let navigated = rpi_ai::identity::scope(
            identity.clone(),
            self.navigate_target(
                target,
                &target_entry,
                &old_leaf,
                branch.revision,
                summary_entry_id,
                &options,
                hooks.as_ref(),
                &identity,
                &signal,
                &mut began,
            ),
        )
        .await;

        let (outcome, finished, finish_error) = match navigated {
            Ok(done) => {
                let new_leaf_id = storage
                    .branch_revision(&self.lane)
                    .await
                    .map_err(session_to_harness_err)?
                    .leaf_id;
                if let Some(hooks) = &hooks {
                    let mut payload = json!({"newLeafId": new_leaf_id, "oldLeafId": old_leaf});
                    if let Some(entry) = &done.summary_entry {
                        payload["summaryEntry"] = json!(entry);
                        payload["fromExtension"] = json!(done.from_extension);
                    }
                    let _ = rpi_ai::identity::scope(
                        identity.clone(),
                        hooks.lifecycle(
                            "session_tree",
                            payload,
                            identity.clone(),
                            CancellationToken::new(),
                        ),
                    )
                    .await;
                }
                (
                    NavigationOutcome::Completed {
                        new_leaf_id,
                        summary_entry: done.summary_entry,
                        editor_text: done.editor_text,
                    },
                    OperationOutcome::Completed,
                    None,
                )
            }
            Err(ended) => {
                if let (Some(hooks), true) = (&hooks, began) {
                    let mut payload = json!({"targetId": target, "oldLeafId": old_leaf,
                        "cancelled": true, "aborted": matches!(ended, Ended::Aborted)});
                    if let Ended::Failed(failure) = &ended {
                        payload["cancelled"] = json!(false);
                        payload["error"] = json!(failure.message);
                    }
                    let _ = rpi_ai::identity::scope(
                        identity.clone(),
                        hooks.lifecycle(
                            "session_tree_failed",
                            payload,
                            identity.clone(),
                            CancellationToken::new(),
                        ),
                    )
                    .await;
                }
                match ended {
                    Ended::Cancelled => (
                        NavigationOutcome::Declined {
                            leaf_id: old_leaf.clone(),
                        },
                        OperationOutcome::Declined,
                        None,
                    ),
                    Ended::Aborted => (
                        NavigationOutcome::Aborted {
                            leaf_id: old_leaf.clone(),
                        },
                        OperationOutcome::Aborted,
                        None,
                    ),
                    Ended::Failed(failure) => (
                        NavigationOutcome::Failed {
                            leaf_id: old_leaf.clone(),
                            error: failure.clone(),
                        },
                        OperationOutcome::Failed,
                        Some(failure),
                    ),
                }
            }
        };
        self.write_operation_finished(&run_id, finished, finish_error)
            .await?;
        self.finish_context_writes(&run_id, None).await?;
        Ok(result(outcome))
    }

    #[allow(clippy::too_many_arguments)]
    async fn navigate_target(
        &self,
        target: &str,
        target_entry: &Entry,
        old_leaf: &Option<String>,
        revision: u64,
        summary_entry_id: Option<String>,
        options: &NavigateTreeOptions,
        hooks: Option<&Arc<dyn rpi_ai::ProviderHooks>>,
        identity: &RequestIdentity,
        signal: &CancellationToken,
        began: &mut bool,
    ) -> Result<Navigated, Ended> {
        let view = self.session.view(&self.lane);
        let collected = collect_entries_for_branch_summary(&*view, old_leaf.as_deref(), target)
            .await
            .map_err(|e| Ended::Failed(error("navigation_failed", e.to_string())))?;
        let mut custom_instructions = options.custom_instructions.clone();
        let mut replace_instructions = options.replace_instructions;
        let mut label = options.label.clone();

        let mut extension_summary = None;
        if let Some(hooks) = hooks {
            let mut preparation = Map::new();
            preparation.insert("targetId".into(), json!(target));
            preparation.insert("oldLeafId".into(), json!(old_leaf));
            preparation.insert(
                "commonAncestorId".into(),
                json!(collected.common_ancestor_id),
            );
            preparation.insert("entriesToSummarize".into(), json!(collected.entries));
            preparation.insert("userWantsSummary".into(), json!(options.summarize));
            if let Some(value) = &custom_instructions {
                preparation.insert("customInstructions".into(), json!(value));
            }
            if let Some(value) = replace_instructions {
                preparation.insert("replaceInstructions".into(), json!(value));
            }
            if let Some(value) = &label {
                preparation.insert("label".into(), json!(value));
            }
            *began = true;
            let returned = hooks
                .lifecycle(
                    "session_before_tree",
                    json!({"preparation": preparation}),
                    identity.clone(),
                    signal.clone(),
                )
                .await
                .map_err(|e| Ended::Failed(error("hook_failed", e)))?;
            if let Some(returned) = returned {
                if returned["cancel"] == true {
                    return Err(Ended::Cancelled);
                }
                if options.summarize && truthy(&returned["summary"]) {
                    extension_summary = Some(returned["summary"].clone());
                }
                // Pi applies any defined override, including an explicit null.
                if let Some(value) = returned.get("customInstructions") {
                    custom_instructions = value.as_str().map(str::to_owned);
                }
                if let Some(value) = returned.get("replaceInstructions") {
                    replace_instructions = value.as_bool();
                }
                if let Some(value) = returned.get("label") {
                    label = value.as_str().map(str::to_owned);
                }
            }
        }

        let mut from_extension = false;
        let (summary, details, usage) = if let Some(summary) = extension_summary {
            from_extension = true;
            let text = match &summary["summary"] {
                Value::String(text) => Some(text.clone()),
                Value::Null => None,
                _ => {
                    return Err(Ended::Failed(error(
                        "summary_failed",
                        "Extension tree summary must be a string",
                    )))
                }
            };
            let usage = match summary.get("usage").filter(|value| !value.is_null()) {
                Some(value) => Some(serde_json::from_value(value.clone()).map_err(|_| {
                    Ended::Failed(error("summary_failed", "Invalid extension summary usage"))
                })?),
                None => None,
            };
            (
                text,
                summary.get("details").filter(|v| !v.is_null()).cloned(),
                usage,
            )
        } else if options.summarize && !collected.entries.is_empty() {
            let snap = self
                .snapshot_config()
                .map_err(|e| Ended::Failed(error("summary_failed", e.to_string())))?;
            let mut provider = Self::resolve_provider(&snap.models, &snap.model)
                .map_err(|e| Ended::Failed(error("summary_failed", e.to_string())))?;
            if let Some(hooks) = hooks {
                provider = Arc::new(SummaryProvider {
                    provider,
                    hooks: hooks.clone(),
                });
            }
            let mut summary_options = GenerateBranchSummaryOptions::new(CompactionLlmOptions {
                provider,
                model: snap.model.clone(),
                api_key: None,
                signal: signal.clone(),
                thinking_level: Some(snap.thinking_level),
                retry: snap.retry.enabled.then(|| snap.retry.clone()),
                custom_instructions: custom_instructions.clone(),
            });
            summary_options.replace_instructions = replace_instructions.unwrap_or(false);
            if let Some(tokens) = self.inner.lock().unwrap().branch_summary_reserve_tokens {
                summary_options.reserve_tokens = tokens;
            }
            match generate_branch_summary(&collected.entries, &summary_options).await {
                Ok(generated) => (
                    Some(generated.summary),
                    Some(json!({"readFiles": generated.read_files,
                        "modifiedFiles": generated.modified_files})),
                    generated.usage,
                ),
                Err(e) if e.code == CompactionErrorCode::Aborted => return Err(Ended::Aborted),
                Err(e) => return Err(Ended::Failed(error("summary_failed", e.message))),
            }
        } else {
            (None, None, None)
        };
        let summary = summary.filter(|text| !text.is_empty());

        // A user or custom message is re-edited: the branch moves to its parent.
        let (new_leaf, editor_text) = match target_entry {
            Entry::Message(entry) => match &entry.message {
                AgentMessage::User(user) => (
                    entry.base.parent_id.clone(),
                    Some(match &user.content {
                        rpi_ai::types::UserContent::Text(text) => text.clone(),
                        rpi_ai::types::UserContent::Blocks(blocks) => content_text(blocks),
                    }),
                ),
                AgentMessage::Custom(custom) if custom.role == CUSTOM_ROLE => (
                    entry.base.parent_id.clone(),
                    Some(content_text(&custom.content)),
                ),
                _ => (Some(target.to_owned()), None),
            },
            _ => (Some(target.to_owned()), None),
        };

        let storage = self.session.storage();
        let moved = storage
            .move_lane_at_revision(&self.lane, revision, new_leaf.as_deref())
            .await
            .map_err(|e| Ended::Failed(error("navigation_failed", e.to_string())))?;
        let summary_entry = match (summary, summary_entry_id) {
            (Some(summary), Some(id)) => {
                let entry = ProvisionedEntry {
                    id,
                    kind: ProvisionedKind::BranchSummary {
                        from_id: old_leaf.clone().unwrap_or_else(|| "root".into()),
                        summary,
                        details,
                        usage,
                        from_hook: Some(from_extension),
                    },
                };
                let committed = storage
                    .commit_entry(
                        entry,
                        AppendTarget::Current {
                            lane: self.lane.clone(),
                            branch_revision: moved.revision,
                        },
                    )
                    .await
                    .map_err(|e| Ended::Failed(error("navigation_failed", e.to_string())))?;
                Some(committed.entry)
            }
            _ => None,
        };
        if let Some(label) = label.filter(|label| !label.is_empty()) {
            let labelled = summary_entry
                .as_ref()
                .map_or(target, |entry| entry.base().id.as_str());
            self.session
                .set_label(labelled, Some(&label))
                .await
                .map_err(|e| Ended::Failed(error("navigation_failed", e.to_string())))?;
        }
        self.restore_tools_from_branch().await;
        Ok(Navigated {
            from_extension: summary_entry.is_some() && from_extension,
            summary_entry,
            editor_text,
        })
    }

    /// Pi `_restoreToolsFromTranscript`: the new branch's latest tool selection
    /// becomes the active set.
    async fn restore_tools_from_branch(&self) {
        let Ok(path) = self.branch_path_oldest_first().await else {
            return;
        };
        if let Some(names) = derive_session_context_state(&path).active_tool_names {
            let mut inner = self.inner.lock().unwrap();
            if !inner.closed {
                let _ = inner.select_tools(names, Value::Null);
            }
        }
    }
}
