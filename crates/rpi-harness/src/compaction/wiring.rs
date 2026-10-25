//! Host wiring for the Pi 1.3.0 pure compaction decisions in [`super::pure`].
//!
//! [`super::pure`] works on Pi-shaped session JSON. This module is the narrow
//! bridge between the host's typed [`Entry`]/[`AgentMessage`] model and those
//! decisions, so the run loop can use one implementation of
//! `_checkCompaction`/`prepareCompaction` instead of a second, hand-rolled one.
//!
//! Two conversions are needed:
//!
//! - [`pi_path_values`] serializes a host branch path to Pi-shaped entries. Host
//!   compaction entries keep their retained tail *inside* the entry, while Pi
//!   1.3.0 keeps a `firstKeptEntryId` pointing into the path. The conversion
//!   splices the retained tail back in as virtual message entries with a
//!   rewritten parent chain and records the synthetic `firstKeptEntryId`, so the
//!   pure projection replays exactly what the host replays.
//! - [`projected_preparation`] selects the compaction cut with the pure
//!   projected algorithm and then converts the selected host entries with the
//!   host's own message constructors, so custom-message projectors and the
//!   branch/compaction summary envelopes stay host-owned.
//!
//! The automatic-trigger table is pure over the same JSON, so
//! [`auto_compaction_decision`] compares like-for-like with
//! `compaction-trigger-reference.json`.

use std::collections::{BTreeSet, HashMap};

use rpi_agent::message::AgentMessage;
use rpi_ai::types::AssistantMessage;
use serde_json::{json, Value};

use super::compaction::{get_message_from_entry_for_compaction, CompactionPreparation};
use super::pure::{
    build_session_projection, find_projected_cut_point,
    prepare_compaction as pure_prepare_compaction, LeafSelector, PureSettings,
};
use super::tokens::{build_session_context, estimate_context_tokens, FileOperations};
use crate::session::types::{CompactionReason, Entry};
use crate::types::CompactionSettings;

/// Pi-shaped session entries for a host branch path (see the module docs).
pub fn pi_path_values(path: &[Entry]) -> Vec<Value> {
    let mut values: Vec<Value> = Vec::with_capacity(path.len());
    // Parent id assigned to the path entry that originally followed a
    // compaction whose retained tail was spliced in after it.
    let mut pending_parent: Option<String> = None;
    for entry in path {
        if let Entry::Compaction(compaction) = entry {
            let mut value = entry.to_flat_json();
            if !compaction.retained_tail.is_empty() {
                let first_id = format!("{}:retained:0", compaction.base.id);
                value["firstKeptEntryId"] = Value::String(first_id);
                values.push(value);
                let mut parent = compaction.base.id.clone();
                for (index, message) in compaction.retained_tail.iter().enumerate() {
                    let id = format!("{}:retained:{index}", compaction.base.id);
                    values.push(json!({
                        "type": "message",
                        "id": id,
                        "parentId": parent,
                        "seq": compaction.base.seq,
                        "timestamp": message_timestamp(message),
                        "message": serde_json::to_value(message).unwrap_or(Value::Null),
                    }));
                    parent = id;
                }
                pending_parent = Some(parent);
                continue;
            }
            values.push(value);
            continue;
        }
        let mut value = entry.to_flat_json();
        if let Some(parent) = pending_parent.take() {
            value["parentId"] = Value::String(parent);
        }
        values.push(value);
    }
    values
}

fn message_timestamp(message: &AgentMessage) -> i64 {
    match message {
        AgentMessage::User(message) => message.timestamp,
        AgentMessage::Assistant(message) => message.timestamp,
        AgentMessage::ToolResult(message) => message.timestamp,
        AgentMessage::Custom(message) => message.timestamp,
    }
}

/// Host settings in the pure module's shape.
pub fn pure_settings(settings: CompactionSettings) -> PureSettings {
    PureSettings {
        enabled: settings.enabled,
        reserve_tokens: settings.reserve_tokens,
        keep_recent_tokens: settings.keep_recent_tokens,
    }
}

/// The Pi-shaped model descriptor the trigger table reads
/// (`provider`, `id`, `contextWindow`, `maxTokens`).
pub fn model_value(model: &rpi_ai::Model) -> Value {
    json!({
        "provider": model.provider,
        "id": model.id,
        "contextWindow": model.context_window,
        "maxTokens": model.max_tokens,
    })
}

/// Pi-shaped assistant message (`stopReason`, `usage`, `errorMessage`, ...).
pub fn assistant_value(message: &AssistantMessage) -> Value {
    serde_json::to_value(message).unwrap_or(Value::Null)
}

/// Result of [`auto_compaction_decision`], mirroring the cases `_checkCompaction`
/// selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoCompactionDecision {
    /// No compaction this turn.
    None,
    /// Run `_runAutoCompaction` with these arguments.
    Run {
        reason: CompactionReason,
        will_retry: bool,
    },
    /// A second overflow/length recovery attempt was refused; Pi emits
    /// `compaction_end { willRetry: false }` and `session_compact_failed`.
    Exhausted { reason: CompactionReason },
}

/// Inputs for [`auto_compaction_decision`].
pub struct AutoCompactionCheck<'a> {
    /// Persisted branch path (oldest first).
    pub path: &'a [Entry],
    /// Finalized context messages the provider saw (`agent.state.messages` in Pi).
    pub messages: &'a [AgentMessage],
    pub model: &'a rpi_ai::Model,
    pub settings: CompactionSettings,
    /// Persisted entry id for the checked assistant message, when known.
    pub assistant_entry_id: Option<&'a str>,
    /// Pi's `skipAbortedCheck`: `true` for post-run checks, `false` before a run.
    pub skip_aborted_check: bool,
    pub overflow_recovery_attempted: bool,
}

/// `_checkCompaction`'s decision table over host state.
pub fn auto_compaction_decision(check: &AutoCompactionCheck<'_>) -> AutoCompactionDecision {
    let message = check
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            AgentMessage::Assistant(assistant) => Some(assistant_value(assistant)),
            _ => None,
        });
    let Some(message) = message else {
        return AutoCompactionDecision::None;
    };
    let pure = super::pure::CheckCompactionInput {
        settings: pure_settings(check.settings),
        model: model_value(check.model),
        message,
        branch: pi_path_values(check.path),
        agent_messages: check
            .messages
            .iter()
            .map(|message| serde_json::to_value(message).unwrap_or(Value::Null))
            .collect(),
        assistant_entry_id: check.assistant_entry_id.map(ToOwned::to_owned),
        skip_aborted_check: check.skip_aborted_check,
        overflow_recovery_attempted: check.overflow_recovery_attempted,
        run_returns: None,
    };
    let outcome = super::pure::check_compaction(&pure);
    if let Some(step) = outcome
        .steps
        .iter()
        .find(|step| step.starts_with("runAutoCompaction:"))
    {
        let mut parts = step.split(':');
        let _ = parts.next();
        let reason = match parts.next() {
            Some("overflow") => CompactionReason::Overflow,
            _ => CompactionReason::Threshold,
        };
        let will_retry = parts.next() == Some("true");
        return AutoCompactionDecision::Run { reason, will_retry };
    }
    if outcome
        .steps
        .iter()
        .any(|step| step.starts_with("session_compact_failed:"))
    {
        return AutoCompactionDecision::Exhausted {
            reason: CompactionReason::Overflow,
        };
    }
    AutoCompactionDecision::None
}

/// `isContextOverflow` over a host assistant message.
pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<i64>) -> bool {
    super::pure::is_context_overflow(&assistant_value(message), context_window)
}

/// `isRecoverableLength` over a host assistant message.
pub fn is_recoverable_length(message: &AssistantMessage, desired_max_output: i64) -> bool {
    super::pure::is_recoverable_length(&assistant_value(message), desired_max_output)
}

/// `prepareCompaction` with Pi 1.3.0's projected cut point, converted back to
/// the host preparation type.
///
/// Returns `None` exactly when Pi's `prepareCompaction` returns `undefined`
/// (already compacted, nothing to summarize, no first kept entry).
pub fn projected_preparation(
    path: &[Entry],
    settings: CompactionSettings,
) -> Option<CompactionPreparation> {
    let values = pi_path_values(path);
    // The pure entry point is the compatibility oracle for "is there anything
    // to compact" (already compacted, empty summarize range, missing kept id).
    let pure = pure_prepare_compaction(&values, &pure_settings(settings))?;
    let first_kept = pure.first_kept_entry_id.clone();
    let by_id: HashMap<&str, &Entry> = path.iter().map(|entry| (entry.id(), entry)).collect();

    let projection = build_session_projection(&values, &LeafSelector::Default);
    let boundary_start = projection
        .entries
        .iter()
        .position(|entry| {
            entry.source.get("type").and_then(Value::as_str) == Some("compaction")
                && !entry.messages.is_empty()
        })
        .map(|index| index + 1)
        .unwrap_or(0);
    let boundary_end = projection.entries.len();
    let cut = find_projected_cut_point(
        &projection.entries,
        boundary_start,
        boundary_end,
        settings.keep_recent_tokens,
    );
    let history_end = if cut.is_split_turn {
        cut.turn_start_index.unwrap_or(0)
    } else {
        cut.first_kept_entry_index
    };

    let mut messages_to_summarize = Vec::new();
    if history_end > boundary_start && history_end <= projection.entries.len() {
        for projected in &projection.entries[boundary_start..history_end] {
            messages_to_summarize.extend(host_messages(projected, &by_id));
        }
    }
    let mut turn_prefix_messages = Vec::new();
    if cut.is_split_turn {
        let turn_start = cut.turn_start_index.unwrap_or(0);
        if cut.first_kept_entry_index > turn_start {
            for projected in &projection.entries[turn_start..cut.first_kept_entry_index] {
                turn_prefix_messages.extend(host_messages(projected, &by_id));
            }
        }
    }
    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return None;
    }

    let mut retained_tail = Vec::new();
    if let Some(index) = projection.entries.iter().position(|entry| {
        entry.source.get("id").and_then(Value::as_str) == Some(first_kept.as_str())
    }) {
        for projected in &projection.entries[index..] {
            retained_tail.extend(host_messages(projected, &by_id));
        }
    }

    let tokens_before = estimate_context_tokens(&build_session_context(path).messages).tokens;
    Some(CompactionPreparation {
        messages_to_summarize,
        turn_prefix_messages,
        retained_tail,
        is_split_turn: cut.is_split_turn,
        tokens_before,
        previous_summary: pure.previous_summary,
        file_ops: file_ops_from_pure(&pure.file_ops),
        settings,
    })
}

fn file_ops_from_pure(ops: &super::pure::FileOperations) -> FileOperations {
    FileOperations {
        read: ops.read.iter().cloned().collect::<BTreeSet<_>>(),
        written: ops.written.iter().cloned().collect::<BTreeSet<_>>(),
        edited: ops.edited.iter().cloned().collect::<BTreeSet<_>>(),
    }
}

/// Host messages contributed by a projected entry, in Pi's order. Compaction
/// entries contribute nothing (`getMessagesFromProjectedEntryForCompaction`).
fn host_messages(
    projected: &super::pure::ProjectedEntry,
    by_id: &HashMap<&str, &Entry>,
) -> Vec<AgentMessage> {
    if projected.source.get("type").and_then(Value::as_str) == Some("compaction") {
        return Vec::new();
    }
    // Message content comes from the finalized projection, never from the raw
    // source: otherwise omissions/replacements resurrect in the summary/tail.
    if projected.source.get("type").and_then(Value::as_str) == Some("message") {
        return projected
            .messages
            .iter()
            .filter_map(|message| {
                let original =
                    serde_json::from_value(projected.source.get("message")?.clone()).ok()?;
                crate::session::context::replace_message_content(&original, message)
            })
            .collect();
    }
    let Some(id) = projected.source.get("id").and_then(Value::as_str) else {
        return Vec::new();
    };
    by_id
        .get(id)
        .and_then(|entry| get_message_from_entry_for_compaction(entry))
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests;
