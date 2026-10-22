//! Port of `compaction.ts`'s preparation path: the projected cut-point search,
//! `extractFileOperations`, `prepareCompaction` and `getSummarizationFailure`.

use serde_json::Value;

use super::file_ops::{extract_file_ops_from_message, FileOperations};
use super::model::{entry_id, entry_type, js_truthy, message_role, str_field, LeafSelector, PureSettings};
use super::projection::{
    build_session_projection, is_intrinsically_visible, is_omitted, ProjectedEntry,
};
use super::tokens::{estimate_projected_context_tokens, estimate_tokens};

/// Cut point selected over the projected entry list. Mirrors `CutPointResult`,
/// with `turn_start_index: None` standing in for TS `-1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectedCutPoint {
    pub first_kept_entry_index: usize,
    pub turn_start_index: Option<usize>,
    pub is_split_turn: bool,
}

/// `isCutPointMessage(message)` — never a tool result.
pub fn is_cut_point_message(message: &Value) -> bool {
    matches!(
        message_role(message),
        "user" | "assistant" | "bashExecution" | "custom" | "branchSummary" | "compactionSummary"
    )
}

/// `isTurnStartMessage(message)`.
pub fn is_turn_start_message(message: &Value) -> bool {
    matches!(
        message_role(message),
        "user" | "bashExecution" | "custom" | "branchSummary" | "compactionSummary"
    )
}

/// `isProjectedTurnStart(entry)`.
pub fn is_projected_turn_start(entry: &ProjectedEntry) -> bool {
    if entry_type(&entry.source) == "compaction" {
        return false;
    }
    entry.messages.iter().any(is_turn_start_message)
}

/// `findProjectedTurnStartIndex(entries, entryIndex, startIndex)`.
pub fn find_projected_turn_start_index(
    entries: &[ProjectedEntry],
    entry_index: usize,
    start_index: usize,
) -> Option<usize> {
    if entry_index < start_index || entry_index >= entries.len() {
        return None;
    }
    for index in (start_index..=entry_index).rev() {
        if is_projected_turn_start(&entries[index]) {
            return Some(index);
        }
    }
    None
}

fn replacement_is_null(entry: &Value) -> bool {
    matches!(entry.get("replacement"), Some(Value::Null))
}

/// `findProjectedCutPoint(entries, startIndex, endIndex, keepRecentTokens)`.
///
/// Walks backwards accumulating estimated tokens, picks the closest valid cut at
/// or after the budget point, then applies the recovery-omission advancement rule
/// and the trailing-metadata walk-back.
pub fn find_projected_cut_point(
    entries: &[ProjectedEntry],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: i64,
) -> ProjectedCutPoint {
    let end = end_index.min(entries.len());
    let start = start_index.min(entries.len());
    let cut_points: Vec<usize> = (start..end)
        .filter(|index| {
            let entry = &entries[*index];
            entry_type(&entry.source) != "compaction"
                && entry.messages.iter().any(is_cut_point_message)
        })
        .collect();
    if cut_points.is_empty() {
        return ProjectedCutPoint {
            first_kept_entry_index: start_index,
            turn_start_index: None,
            is_split_turn: false,
        };
    }

    let mut accumulated_tokens = 0i64;
    let mut exceeded_budget = false;
    let mut cut_index = cut_points[0];
    if end > start {
        let mut index = end;
        while index > start {
            index -= 1;
            let message_tokens: i64 = entries[index].messages.iter().map(estimate_tokens).sum();
            if message_tokens == 0 {
                continue;
            }
            accumulated_tokens += message_tokens;
            if accumulated_tokens >= keep_recent_tokens {
                exceeded_budget = true;
                cut_index = cut_points
                    .iter()
                    .copied()
                    .find(|candidate| *candidate >= index)
                    .unwrap_or(*cut_points.last().unwrap());
                break;
            }
        }
    }

    // Recovery attempt and its omission edits are context-invisible after the last
    // visible input; advance only for a closed suffix containing an omitted
    // assistant attempt.
    let suffix = if cut_index + 1 <= end {
        &entries[cut_index + 1..end]
    } else {
        &[]
    };
    let omitted_suffix_ids: Vec<&str> = suffix
        .iter()
        .filter(|entry| is_omitted(entry))
        .filter_map(|entry| entry_id(&entry.source))
        .collect();
    let has_external_replacement = suffix.iter().any(|entry| {
        entry_type(&entry.source) == "context_edit"
            && !replacement_is_null(&entry.source)
            && !str_field(&entry.source, "targetId")
                .map(|target| omitted_suffix_ids.contains(&target))
                .unwrap_or(false)
    });
    let is_recovery_omission_suffix = exceeded_budget
        && !has_external_replacement
        && suffix.iter().any(|entry| {
            entry_type(&entry.source) == "message"
                && entry
                    .source
                    .get("message")
                    .map(message_role)
                    .unwrap_or("")
                    == "assistant"
                && is_omitted(entry)
        })
        && suffix.iter().all(|entry| {
            entry_type(&entry.source) != "compaction"
                && (!is_intrinsically_visible(&entry.source) || is_omitted(entry))
        });
    if is_recovery_omission_suffix {
        cut_index += 1;
    }

    while cut_index > start {
        let previous = &entries[cut_index - 1];
        if entry_type(&previous.source) == "compaction" || !previous.messages.is_empty() {
            break;
        }
        cut_index -= 1;
    }

    let starts_turn = is_projected_turn_start(&entries[cut_index]);
    let turn_start_index = if starts_turn {
        None
    } else {
        find_projected_turn_start_index(entries, cut_index, start_index)
    };
    ProjectedCutPoint {
        first_kept_entry_index: cut_index,
        turn_start_index,
        is_split_turn: !starts_turn && turn_start_index.is_some(),
    }
}

/// `getMessagesFromProjectedEntryForCompaction(entry)`.
pub fn get_messages_from_projected_entry_for_compaction(entry: &ProjectedEntry) -> Vec<Value> {
    if entry_type(&entry.source) == "compaction" {
        return Vec::new();
    }
    entry
        .messages
        .iter()
        .filter(|message| message_role(message) != "system")
        .cloned()
        .collect()
}

/// `extractFileOperations(messages, entries, prevCompactionIndex)` — seeds from a
/// previous Pi-generated compaction's details, then extracts from the summarized
/// messages.
pub fn extract_file_operations(
    messages: &[Value],
    entries: &[&Value],
    prev_compaction_index: Option<usize>,
) -> FileOperations {
    let mut ops = FileOperations::default();
    if let Some(index) = prev_compaction_index {
        if let Some(entry) = entries.get(index) {
            if entry_type(entry) == "compaction"
                && !entry
                    .get("fromHook")
                    .map(js_truthy)
                    .unwrap_or(false)
            {
                if let Some(details) = entry.get("details").filter(|details| js_truthy(details)) {
                    if let Some(read_files) = details.get("readFiles").and_then(Value::as_array) {
                        for file in read_files.iter().filter_map(Value::as_str) {
                            ops.add_read(file);
                        }
                    }
                    if let Some(modified_files) =
                        details.get("modifiedFiles").and_then(Value::as_array)
                    {
                        for file in modified_files.iter().filter_map(Value::as_str) {
                            ops.add_edited(file);
                        }
                    }
                }
            }
        }
    }
    for message in messages {
        extract_file_ops_from_message(message, &mut ops);
    }
    ops
}

/// Prepared compaction inputs. Mirrors TS `CompactionPreparation` (camelCase
/// keys, `previousSummary` omitted when absent like `JSON.stringify`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionPreparation {
    pub first_kept_entry_id: String,
    pub messages_to_summarize: Vec<Value>,
    pub turn_prefix_messages: Vec<Value>,
    pub is_split_turn: bool,
    pub tokens_before: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_summary: Option<String>,
    pub file_ops: FileOperations,
    pub settings: PureSettings,
}

/// `prepareCompaction(pathEntries, settings)` — `None` when compaction is not
/// applicable (already compacted, no summarizable messages, or no kept entry).
pub fn prepare_compaction(
    path_entries: &[Value],
    settings: &PureSettings,
) -> Option<CompactionPreparation> {
    if let Some(last) = path_entries.last() {
        if entry_type(last) == "compaction" {
            return None;
        }
    }
    let projection = build_session_projection(path_entries, &LeafSelector::Default);
    let projected = &projection.entries;
    let source_entries: Vec<&Value> = projected.iter().map(|entry| &entry.source).collect();

    let prev_compaction_index = projected.iter().position(|entry| {
        entry_type(&entry.source) == "compaction" && !entry.messages.is_empty()
    });
    let (previous_summary, boundary_start) = match prev_compaction_index {
        Some(index) => (
            Some(
                str_field(&projected[index].source, "summary")
                    .unwrap_or("")
                    .to_string(),
            ),
            index + 1,
        ),
        None => (None, 0),
    };
    let boundary_end = projected.len();
    let tokens_before = estimate_projected_context_tokens(&projection, path_entries).tokens;
    let cut_point = find_projected_cut_point(
        projected,
        boundary_start,
        boundary_end,
        settings.keep_recent_tokens,
    );
    let first_kept_entry_id = projected
        .get(cut_point.first_kept_entry_index)
        .and_then(|entry| entry_id(&entry.source))
        .map(str::to_string)?;

    let history_end = if cut_point.is_split_turn {
        cut_point.turn_start_index.unwrap_or(0)
    } else {
        cut_point.first_kept_entry_index
    };
    let mut messages_to_summarize: Vec<Value> = Vec::new();
    if history_end > boundary_start && history_end <= projected.len() {
        for entry in &projected[boundary_start..history_end] {
            messages_to_summarize.extend(get_messages_from_projected_entry_for_compaction(entry));
        }
    }
    let mut turn_prefix_messages: Vec<Value> = Vec::new();
    if cut_point.is_split_turn {
        let turn_start = cut_point.turn_start_index.unwrap_or(0);
        if cut_point.first_kept_entry_index > turn_start {
            for entry in &projected[turn_start..cut_point.first_kept_entry_index] {
                turn_prefix_messages.extend(get_messages_from_projected_entry_for_compaction(entry));
            }
        }
    }
    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return None;
    }

    let mut file_ops =
        extract_file_operations(&messages_to_summarize, &source_entries, prev_compaction_index);
    if cut_point.is_split_turn {
        for message in &turn_prefix_messages {
            extract_file_ops_from_message(message, &mut file_ops);
        }
    }

    Some(CompactionPreparation {
        first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        is_split_turn: cut_point.is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings: *settings,
    })
}

/// `getSummarizationFailure(response, label)`.
pub fn get_summarization_failure(response: &Value, label: &str) -> Option<String> {
    match str_field(response, "stopReason").unwrap_or("") {
        "error" => {
            let message = str_field(response, "errorMessage").unwrap_or("");
            let message = if message.is_empty() {
                "Unknown error"
            } else {
                message
            };
            Some(format!("{label} failed: {message}"))
        }
        "length" => Some(format!(
            "{label} failed: generation hit the token cap and the summary is incomplete"
        )),
        _ => None,
    }
}
