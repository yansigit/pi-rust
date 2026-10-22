//! Port of `session-manager.ts`'s projection pipeline, which Pi 1.3.0's
//! `prepareCompaction` consumes: `buildSessionPath` → `buildContextEntries` →
//! `projectContextEntry` → `buildSessionProjection`.
//!
//! Unlike the host `session` module this works on Pi-shaped JSON entries, so
//! `context_edit` entries and the newest-compaction-wins projection are modelled
//! without changing the host `Entry` enum.

use serde_json::{json, Map, Value};

use super::model::{entry_id, entry_type, message_role, parent_id, str_field, LeafSelector};

/// One selected entry together with the messages it contributes to the model
/// context. Mirrors `{ sourceEntry, messages }`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedEntry {
    pub source: Value,
    pub messages: Vec<Value>,
}

/// Result of `buildSessionProjection`. Mirrors
/// `{ entries, messages, thinkingLevel, model }`.
#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub entries: Vec<ProjectedEntry>,
    pub messages: Vec<Value>,
    pub thinking_level: String,
    pub model: Option<Value>,
}

/// `new Date(timestamp).getTime()`. Fixtures use numeric epoch milliseconds; an
/// ISO string would need date parsing, which this port deliberately leaves to the
/// caller (documented as not tested).
fn timestamp_value(entry: &Value) -> Value {
    match entry.get("timestamp") {
        Some(Value::Number(number)) => {
            if let Some(i) = number.as_i64() {
                json!(i)
            } else if let Some(u) = number.as_u64() {
                json!(u)
            } else {
                Value::Null
            }
        }
        _ => Value::Null,
    }
}

/// `createCustomMessage(customType, content, display, details, timestamp)`.
pub fn create_custom_message(entry: &Value) -> Value {
    let mut map = Map::new();
    map.insert("role".into(), json!("custom"));
    map.insert(
        "customType".into(),
        entry.get("customType").cloned().unwrap_or(Value::Null),
    );
    // `entry.content ?? []` (null and missing both fall back)
    let content = match entry.get("content") {
        Some(Value::Null) | None => json!([]),
        Some(content) => content.clone(),
    };
    map.insert("content".into(), content);
    map.insert(
        "display".into(),
        entry.get("display").cloned().unwrap_or(Value::Null),
    );
    if let Some(details) = entry.get("details") {
        map.insert("details".into(), details.clone());
    }
    map.insert("timestamp".into(), timestamp_value(entry));
    Value::Object(map)
}

/// `createBranchSummaryMessage(summary, fromId, timestamp)`.
pub fn create_branch_summary_message(entry: &Value) -> Value {
    json!({
        "role": "branchSummary",
        "summary": entry.get("summary").cloned().unwrap_or(Value::Null),
        "fromId": entry.get("fromId").cloned().unwrap_or(Value::Null),
        "timestamp": timestamp_value(entry),
    })
}

/// `createCompactionSummaryMessage(summary, tokensBefore, timestamp)`.
pub fn create_compaction_summary_message(entry: &Value) -> Value {
    json!({
        "role": "compactionSummary",
        "summary": entry.get("summary").cloned().unwrap_or(Value::Null),
        "tokensBefore": entry.get("tokensBefore").cloned().unwrap_or(Value::Null),
        "timestamp": timestamp_value(entry),
    })
}

/// `sessionEntryToContextMessages(entry)`. Display/state entries contribute
/// nothing; a compaction entry contributes its replayed system message (if any)
/// followed by its summary; missing message content is normalized the same way
/// the upstream parser does.
pub fn session_entry_to_context_messages(entry: &Value) -> Vec<Value> {
    match entry_type(entry) {
        "message" => {
            let Some(message) = entry.get("message") else {
                return Vec::new();
            };
            let role = message_role(message);
            let content_missing = message.get("content").map(Value::is_null).unwrap_or(true);
            if role == "system" && content_missing {
                let mut copy = message.clone();
                if let Value::Object(map) = &mut copy {
                    map.insert("content".into(), json!(""));
                }
                return vec![copy];
            }
            if (role == "user" || role == "assistant" || role == "toolResult") && content_missing {
                let mut copy = message.clone();
                if let Value::Object(map) = &mut copy {
                    map.insert("content".into(), json!([]));
                }
                return vec![copy];
            }
            vec![message.clone()]
        }
        "custom_message" => vec![create_custom_message(entry)],
        "branch_summary" => {
            let summary = str_field(entry, "summary").unwrap_or("");
            if summary.is_empty() {
                Vec::new()
            } else {
                vec![create_branch_summary_message(entry)]
            }
        }
        "compaction" => {
            let summary = create_compaction_summary_message(entry);
            match entry.get("systemMessage") {
                Some(system) if !system.is_null() => vec![system.clone(), summary],
                _ => vec![summary],
            }
        }
        _ => Vec::new(),
    }
}

/// `buildSessionPath(entries, leafId, byId)`.
pub fn build_session_path(entries: &[Value], leaf: &LeafSelector) -> Vec<Value> {
    let index: std::collections::HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|entry| entry_id(entry).map(|id| (id, entry)))
        .collect();
    if matches!(leaf, LeafSelector::ExplicitNone) {
        return Vec::new();
    }
    let mut current = match leaf {
        LeafSelector::Id(id) => index.get(id.as_str()).copied(),
        _ => None,
    };
    if current.is_none() {
        current = entries.last();
    }
    let Some(mut current) = current else {
        return Vec::new();
    };
    let mut path = Vec::new();
    loop {
        path.push(current.clone());
        match parent_id(current).and_then(|id| index.get(id).copied()) {
            Some(parent) => current = parent,
            None => break,
        }
    }
    path.reverse();
    path
}

/// `getLatestCompactionEntry(entries)`.
pub fn get_latest_compaction_entry(entries: &[Value]) -> Option<&Value> {
    entries.iter().rev().find(|entry| entry_type(entry) == "compaction")
}

/// `getSessionContextSettings(path)`.
pub fn get_session_context_settings(path: &[Value]) -> (String, Option<Value>) {
    let mut thinking_level = String::from("off");
    let mut model: Option<Value> = None;
    for entry in path {
        match entry_type(entry) {
            "thinking_level_change" => {
                if let Some(level) = str_field(entry, "thinkingLevel") {
                    thinking_level = level.to_string();
                }
            }
            "model_change" => {
                model = Some(json!({
                    "provider": entry.get("provider").cloned().unwrap_or(Value::Null),
                    "modelId": entry.get("modelId").cloned().unwrap_or(Value::Null),
                }));
            }
            "message" => {
                if let Some(message) = entry.get("message") {
                    if message_role(message) == "assistant" {
                        model = Some(json!({
                            "provider": message.get("provider").cloned().unwrap_or(Value::Null),
                            "modelId": message.get("model").cloned().unwrap_or(Value::Null),
                        }));
                    }
                }
            }
            _ => {}
        }
    }
    (thinking_level, model)
}

/// `buildContextEntries(entries, leafId, byId)` — the newest compaction replaces
/// everything before it, then the kept range and all later entries follow.
pub fn build_context_entries(entries: &[Value], leaf: &LeafSelector) -> Vec<Value> {
    let path = build_session_path(entries, leaf);
    let Some(compaction) = get_latest_compaction_entry(&path) else {
        return path;
    };
    let compaction_id = entry_id(compaction);
    let Some(compaction_idx) = path
        .iter()
        .position(|entry| entry_id(entry) == compaction_id)
    else {
        return path;
    };
    let first_kept = str_field(compaction, "firstKeptEntryId");
    let mut context_entries = vec![compaction.clone()];
    let mut found_first_kept = false;
    for entry in path.iter().take(compaction_idx) {
        if first_kept.is_some() && entry_id(entry) == first_kept {
            found_first_kept = true;
        }
        let is_system_message =
            entry_type(entry) == "message" && entry.get("message").map(message_role) == Some("system");
        if found_first_kept && !is_system_message {
            context_entries.push(entry.clone());
        }
    }
    context_entries.extend(path.iter().skip(compaction_idx + 1).cloned());
    context_entries
}

/// `projectContextEntry(entry, edit)`. A `null` replacement omits the entry;
/// otherwise user/assistant/toolResult/custom content is replaced, with
/// assistant/toolResult string replacements wrapped as a single text block.
pub fn project_context_entry(entry: &Value, edit: Option<&Value>) -> Vec<Value> {
    let messages = session_entry_to_context_messages(entry);
    let Some(edit) = edit else {
        return messages;
    };
    let replacement = edit.get("replacement").cloned().unwrap_or(Value::Null);
    if replacement.is_null() {
        return Vec::new();
    }
    let replacement_content = replacement.get("content").cloned().unwrap_or(Value::Null);
    messages
        .into_iter()
        .map(|message| {
            let role = message_role(&message);
            if role != "user" && role != "assistant" && role != "toolResult" && role != "custom" {
                return message;
            }
            let content = if (role == "assistant" || role == "toolResult")
                && replacement_content.is_string()
            {
                json!([{ "type": "text", "text": replacement_content.as_str().unwrap_or("") }])
            } else {
                replacement_content.clone()
            };
            let mut copy = message;
            if let Value::Object(map) = &mut copy {
                map.insert("content".into(), content);
            }
            copy
        })
        .collect()
}

/// `buildSessionProjection(entries, leafId, byId)`.
pub fn build_session_projection(entries: &[Value], leaf: &LeafSelector) -> Projection {
    let path = build_session_path(entries, leaf);
    let (thinking_level, model) = get_session_context_settings(&path);
    let context_entries = build_context_entries(entries, leaf);
    let mut edits: Vec<(String, Value)> = Vec::new();
    for entry in &context_entries {
        if entry_type(entry) == "context_edit" {
            if let Some(target) = str_field(entry, "targetId") {
                edits.retain(|(id, _)| id != target);
                edits.push((target.to_string(), entry.clone()));
            }
        }
    }
    let mut projected = Vec::with_capacity(context_entries.len());
    for (index, source) in context_entries.into_iter().enumerate() {
        let messages = if entry_type(&source) == "compaction" && index > 0 {
            Vec::new()
        } else {
            let edit = entry_id(&source)
                .and_then(|id| edits.iter().find(|(target, _)| target == id))
                .map(|(_, edit)| edit);
            project_context_entry(&source, edit)
        };
        projected.push(ProjectedEntry { source, messages });
    }
    let messages = projected
        .iter()
        .flat_map(|entry| entry.messages.iter().cloned())
        .collect();
    Projection {
        entries: projected,
        messages,
        thinking_level,
        model,
    }
}

/// `true` when the entry intrinsically produces model-visible messages (before any
/// `context_edit` is applied). Mirrors `isIntrinsicallyVisible`.
pub fn is_intrinsically_visible(entry: &Value) -> bool {
    entry_type(entry) != "context_edit" && !session_entry_to_context_messages(entry).is_empty()
}

/// `true` when an intrinsically visible entry was omitted by a replacement edit.
pub fn is_omitted(projected: &ProjectedEntry) -> bool {
    is_intrinsically_visible(&projected.source) && projected.messages.is_empty()
}
