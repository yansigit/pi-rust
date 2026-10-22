//! Port of `compaction.ts` token estimation plus the `pi-ai` transcript replay
//! (`getCurrentSystemMessage`) that `estimateProjectedContextTokens` uses for its
//! fallback estimate.

use serde_json::{json, Map, Value};

use super::model::{
    as_i64, entry_id, entry_type, int_field, js_json_stringify, js_len, js_truthy, message_role,
    str_field,
};
use super::projection::Projection;

const ESTIMATED_IMAGE_CHARS: usize = 4800;

/// `estimateTextAndImageContentChars(content)`.
pub fn estimate_text_and_image_content_chars(content: &Value) -> usize {
    if let Some(text) = content.as_str() {
        return js_len(text);
    }
    let mut chars = 0usize;
    if let Some(blocks) = content.as_array() {
        for block in blocks {
            match str_field(block, "type") {
                Some("text") => {
                    if let Some(text) = block.get("text").filter(|text| js_truthy(text)) {
                        chars += js_len(text.as_str().unwrap_or(""));
                    }
                }
                Some("image") => chars += ESTIMATED_IMAGE_CHARS,
                _ => {}
            }
        }
    }
    chars
}

/// `estimateTokens(message)` — conservative `chars / 4`, rounded up.
pub fn estimate_tokens(message: &Value) -> i64 {
    let chars = match message_role(message) {
        "system" => {
            let mut chars = message
                .get("content")
                .map(estimate_text_and_image_content_chars)
                .unwrap_or(0);
            if let Some(sections) = message.get("sections").and_then(Value::as_object) {
                for section in sections.values() {
                    if let Some(text) = section.as_str() {
                        chars += js_len(text);
                    }
                }
            }
            if let Some(tools_added) = message.get("toolsAdded").filter(|tools| js_truthy(tools)) {
                chars += js_len(&js_json_stringify(tools_added));
            }
            chars
        }
        "user" | "custom" | "toolResult" => message
            .get("content")
            .map(estimate_text_and_image_content_chars)
            .unwrap_or(0),
        "assistant" => {
            let mut chars = 0usize;
            if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                for block in blocks {
                    match str_field(block, "type") {
                        Some("text") => {
                            chars += js_len(str_field(block, "text").unwrap_or(""));
                        }
                        Some("thinking") => {
                            chars += js_len(str_field(block, "thinking").unwrap_or(""));
                        }
                        Some("toolCall") => {
                            chars += js_len(str_field(block, "name").unwrap_or(""));
                            if let Some(arguments) = block.get("arguments") {
                                chars += js_len(&js_json_stringify(arguments));
                            }
                        }
                        _ => {}
                    }
                }
            }
            chars
        }
        "bashExecution" => {
            js_len(str_field(message, "command").unwrap_or(""))
                + js_len(str_field(message, "output").unwrap_or(""))
        }
        "branchSummary" | "compactionSummary" => js_len(str_field(message, "summary").unwrap_or("")),
        _ => 0,
    };
    div_ceil(chars as i64, 4)
}

/// `Math.ceil(chars / 4)` without `i64::div_ceil` (MSRV 1.78).
fn div_ceil(value: i64, divisor: i64) -> i64 {
    (value + divisor - 1) / divisor
}

/// `calculateContextTokens(usage)` — `totalTokens || input + output + cacheRead + cacheWrite`.
pub fn calculate_context_tokens(usage: &Value) -> i64 {
    let total = int_field(usage, "totalTokens").unwrap_or(0);
    if total != 0 {
        return total;
    }
    int_field(usage, "input").unwrap_or(0)
        + int_field(usage, "output").unwrap_or(0)
        + int_field(usage, "cacheRead").unwrap_or(0)
        + int_field(usage, "cacheWrite").unwrap_or(0)
}

/// `getAssistantUsage(msg)` — a valid usage object, or `None` for aborted,
/// errored or all-zero usage.
fn get_assistant_usage(message: &Value) -> Option<Value> {
    if message_role(message) != "assistant" {
        return None;
    }
    let usage = message.get("usage")?;
    if usage.is_null() {
        return None;
    }
    let stop_reason = str_field(message, "stopReason").unwrap_or("");
    if stop_reason == "aborted" || stop_reason == "error" {
        return None;
    }
    if calculate_context_tokens(usage) > 0 {
        Some(usage.clone())
    } else {
        None
    }
}

/// `getLastAssistantUsage(entries)` — the last valid assistant usage on a branch.
pub fn get_last_assistant_usage(entries: &[Value]) -> Option<Value> {
    for entry in entries.iter().rev() {
        if entry_type(entry) != "message" {
            continue;
        }
        if let Some(message) = entry.get("message") {
            if let Some(usage) = get_assistant_usage(message) {
                return Some(usage);
            }
        }
    }
    None
}

/// `getLastAssistantUsageInfo(messages)` → `(usage, index)`.
fn get_last_assistant_usage_info(messages: &[Value]) -> Option<(Value, usize)> {
    for (index, message) in messages.iter().enumerate().rev() {
        if let Some(usage) = get_assistant_usage(message) {
            return Some((usage, index));
        }
    }
    None
}

/// Estimated context usage. Mirrors `ContextUsageEstimate` (camelCase keys).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsageEstimate {
    pub tokens: i64,
    pub usage_tokens: i64,
    pub trailing_tokens: i64,
    pub last_usage_index: Option<usize>,
}

/// `estimateContextTokens(messages)`.
pub fn estimate_context_tokens(messages: &[Value]) -> ContextUsageEstimate {
    match get_last_assistant_usage_info(messages) {
        None => {
            let estimated: i64 = messages.iter().map(estimate_tokens).sum();
            ContextUsageEstimate {
                tokens: estimated,
                usage_tokens: 0,
                trailing_tokens: estimated,
                last_usage_index: None,
            }
        }
        Some((usage, index)) => {
            let usage_tokens = calculate_context_tokens(&usage);
            let trailing_tokens: i64 = messages.iter().skip(index + 1).map(estimate_tokens).sum();
            ContextUsageEstimate {
                tokens: usage_tokens + trailing_tokens,
                usage_tokens,
                trailing_tokens,
                last_usage_index: Some(index),
            }
        }
    }
}

/// `estimateProjectedContextTokens(projection, branchEntries)` — distrust usage
/// captured before a later edit or compaction.
pub fn estimate_projected_context_tokens(
    projection: &Projection,
    branch_entries: &[Value],
) -> ContextUsageEstimate {
    let estimate = estimate_context_tokens(&projection.messages);
    if let Some(last_usage_index) = estimate.last_usage_index {
        let mut projected_message_index = 0usize;
        let mut usage_entry_id: Option<String> = None;
        for entry in &projection.entries {
            let next_message_index = projected_message_index + entry.messages.len();
            if last_usage_index < next_message_index {
                usage_entry_id = entry_id(&entry.source).map(str::to_string);
                break;
            }
            projected_message_index = next_message_index;
        }
        let usage_entry_index = usage_entry_id
            .as_deref()
            .and_then(|id| branch_entries.iter().position(|entry| entry_id(entry) == Some(id)))
            .map(|index| index as i64)
            .unwrap_or(-1);
        let latest_invalidating_entry_index = branch_entries
            .iter()
            .rposition(|entry| {
                let kind = entry_type(entry);
                kind == "context_edit" || kind == "compaction"
            })
            .map(|index| index as i64)
            .unwrap_or(-1);
        if usage_entry_index > latest_invalidating_entry_index {
            return estimate;
        }
    }
    let current_system = get_current_system_message(&projection.messages);
    let mut tokens = current_system.as_ref().map(estimate_tokens).unwrap_or(0);
    for message in &projection.messages {
        if message_role(message) != "system" {
            tokens += estimate_tokens(message);
        }
    }
    ContextUsageEstimate {
        tokens,
        usage_tokens: 0,
        trailing_tokens: tokens,
        last_usage_index: None,
    }
}

/// `contentText(content, separator = "\n")`.
pub fn content_text(content: &Value, separator: &str) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| str_field(block, "type") == Some("text"))
                .map(|block| str_field(block, "text").unwrap_or("").to_string())
                .collect::<Vec<_>>()
                .join(separator)
        })
        .unwrap_or_default()
}

fn is_system_message(message: &Value) -> bool {
    message_role(message) == "system"
}

/// `getCurrentTools(messages)` — replay `toolsRemoved`/`toolsAdded` deltas.
pub fn get_current_tools(messages: &[Value]) -> Vec<Value> {
    let mut tools: Vec<Value> = Vec::new();
    for message in messages {
        if !is_system_message(message) {
            continue;
        }
        if let Some(removed) = message.get("toolsRemoved").and_then(Value::as_array) {
            for tool in removed {
                let name = str_field(tool, "name");
                tools.retain(|existing| str_field(existing, "name") != name);
            }
        }
        if let Some(added) = message.get("toolsAdded").and_then(Value::as_array) {
            for tool in added {
                let name = str_field(tool, "name");
                if let Some(slot) = tools
                    .iter_mut()
                    .find(|existing| str_field(existing, "name") == name)
                {
                    *slot = tool.clone();
                } else {
                    tools.push(tool.clone());
                }
            }
        }
    }
    tools
}

/// `getCurrentSystemMessage(messages)` — fold every system message into one.
pub fn get_current_system_message(messages: &[Value]) -> Option<Value> {
    let mut content: Vec<String> = Vec::new();
    let mut sections: Vec<(String, Value)> = Vec::new();
    let mut timestamp: Option<i64> = None;
    for message in messages {
        if !is_system_message(message) {
            continue;
        }
        if timestamp.is_none() {
            timestamp = message.get("timestamp").and_then(as_i64);
        }
        let text = content_text(
            message.get("content").unwrap_or(&Value::Null),
            "\n",
        );
        if !text.is_empty() {
            content.push(text);
        }
        if let Some(entry_sections) = message.get("sections").and_then(Value::as_object) {
            for (name, value) in entry_sections {
                if value.is_null() {
                    sections.retain(|(existing, _)| existing != name);
                } else if let Some(slot) = sections.iter_mut().find(|(existing, _)| existing == name) {
                    slot.1 = value.clone();
                } else {
                    sections.push((name.clone(), value.clone()));
                }
            }
        }
    }
    let tools = get_current_tools(messages);
    if timestamp.is_none() && tools.is_empty() {
        return None;
    }
    let mut map = Map::new();
    map.insert("role".into(), json!("system"));
    map.insert("content".into(), json!(content.join("\n\n")));
    if !sections.is_empty() {
        let mut section_map = Map::new();
        for (name, value) in sections {
            section_map.insert(name, value);
        }
        map.insert("sections".into(), Value::Object(section_map));
    }
    if !tools.is_empty() {
        map.insert("toolsAdded".into(), Value::Array(tools));
    }
    map.insert("timestamp".into(), json!(timestamp.unwrap_or(0)));
    Some(Value::Object(map))
}
