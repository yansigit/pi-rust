//! Port of `branch-summarization.ts`'s pure entry collection and budgeted
//! preparation (`collectEntriesForBranchSummary`, `getMessageFromEntry`,
//! `prepareBranchEntries`). Summary *generation* (prompts, LLM call) is already
//! covered by `branch-summary-reference.json` and is not duplicated here.

use serde_json::Value;

use super::file_ops::{extract_file_ops_from_message, FileOperations};
use super::model::{entry_id, entry_type, js_truthy, message_role, parent_id, LeafSelector};
use super::projection::{
    build_session_path, create_branch_summary_message, create_compaction_summary_message,
    create_custom_message,
};
use super::tokens::estimate_tokens;

/// `collectEntriesForBranchSummary` result.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectEntriesResult {
    pub entries: Vec<Value>,
    pub common_ancestor_id: Option<String>,
}

/// `getMessageFromEntry(entry)` — the branch-summarization variant, which also
/// converts compaction entries and skips tool results.
pub fn get_message_from_entry(entry: &Value) -> Option<Value> {
    match entry_type(entry) {
        "message" => {
            let message = entry.get("message")?;
            if message_role(message) == "toolResult" {
                None
            } else {
                Some(message.clone())
            }
        }
        "custom_message" => Some(create_custom_message(entry)),
        "branch_summary" => Some(create_branch_summary_message(entry)),
        "compaction" => Some(create_compaction_summary_message(entry)),
        _ => None,
    }
}

/// `collectEntriesForBranchSummary(session, oldLeafId, targetId)`.
///
/// Walks from `old_leaf_id` back to the deepest common ancestor with
/// `target_id`, in chronological order. Unlike compaction, compaction boundaries
/// are included so their summaries become context.
pub fn collect_entries_for_branch_summary(
    entries: &[Value],
    old_leaf_id: Option<&str>,
    target_id: Option<&str>,
) -> CollectEntriesResult {
    let Some(old_leaf_id) = old_leaf_id else {
        return CollectEntriesResult {
            entries: Vec::new(),
            common_ancestor_id: None,
        };
    };
    let index: std::collections::HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|entry| entry_id(entry).map(|id| (id, entry)))
        .collect();
    let old_path = build_session_path(entries, &LeafSelector::Id(old_leaf_id.to_string()));
    let old_ids: Vec<&str> = old_path.iter().filter_map(|entry| entry_id(entry)).collect();
    let target_path = match target_id {
        Some(target) => build_session_path(entries, &LeafSelector::Id(target.to_string())),
        None => build_session_path(entries, &LeafSelector::ExplicitNone),
    };
    let mut common_ancestor_id: Option<String> = None;
    for entry in target_path.iter().rev() {
        if let Some(id) = entry_id(entry) {
            if old_ids.contains(&id) {
                common_ancestor_id = Some(id.to_string());
                break;
            }
        }
    }
    let mut collected: Vec<Value> = Vec::new();
    let mut current = Some(old_leaf_id.to_string());
    while let Some(id) = current.clone() {
        if Some(id.as_str()) == common_ancestor_id.as_deref() {
            break;
        }
        let Some(entry) = index.get(id.as_str()) else {
            break;
        };
        collected.push((*entry).clone());
        current = parent_id(entry).map(str::to_string);
    }
    collected.reverse();
    CollectEntriesResult {
        entries: collected,
        common_ancestor_id,
    }
}

/// `prepareBranchEntries(entries, tokenBudget)` result.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchPreparation {
    pub messages: Vec<Value>,
    pub file_ops: FileOperations,
    pub total_tokens: i64,
}

/// `prepareBranchEntries(entries, tokenBudget = 0)`.
///
/// First pass collects cumulative file tracking from Pi-generated
/// `branch_summary` details (even for entries that do not fit the budget); second
/// pass walks newest→oldest adding messages until the budget is reached, with the
/// compaction/branch-summary grace rule.
pub fn prepare_branch_entries(entries: &[Value], token_budget: i64) -> BranchPreparation {
    let mut messages: Vec<Value> = Vec::new();
    let mut file_ops = FileOperations::default();
    let mut total_tokens = 0i64;

    for entry in entries {
        if entry_type(entry) == "branch_summary"
            && !entry.get("fromHook").map(js_truthy).unwrap_or(false)
        {
            if let Some(details) = entry.get("details").filter(|details| js_truthy(details)) {
                if let Some(read_files) = details.get("readFiles").and_then(Value::as_array) {
                    for file in read_files.iter().filter_map(Value::as_str) {
                        file_ops.add_read(file);
                    }
                }
                if let Some(modified_files) = details.get("modifiedFiles").and_then(Value::as_array) {
                    for file in modified_files.iter().filter_map(Value::as_str) {
                        file_ops.add_edited(file);
                    }
                }
            }
        }
    }

    for entry in entries.iter().rev() {
        let Some(message) = get_message_from_entry(entry) else {
            continue;
        };
        extract_file_ops_from_message(&message, &mut file_ops);
        let tokens = estimate_tokens(&message);
        if token_budget > 0 && total_tokens + tokens > token_budget {
            let kind = entry_type(entry);
            if kind == "compaction" || kind == "branch_summary" {
                if (total_tokens as f64) < (token_budget as f64) * 0.9 {
                    messages.insert(0, message);
                    total_tokens += tokens;
                }
            }
            break;
        }
        messages.insert(0, message);
        total_tokens += tokens;
    }

    BranchPreparation {
        messages,
        file_ops,
        total_tokens,
    }
}
