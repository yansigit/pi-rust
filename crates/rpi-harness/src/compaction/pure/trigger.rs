//! Port of the automatic-compaction trigger decisions:
//! `pi-ai/overflow.ts` (`isContextOverflow`, `isRecoverableLength`),
//! `compaction.ts` (`shouldCompact`) and `agent-session.ts`
//! (`_checkCompaction`'s case table and its ordered side effects).
//!
//! The overflow classifier is hand-ported from the upstream regex list (the
//! harness crate does not depend on `regex`). It is validated against a corpus
//! generated from the unchanged upstream module in
//! `compaction-trigger-reference.json`; see `compaction-pure-evidence.json` for
//! the explicit limits.

use serde_json::Value;

use super::model::{
    entry_id, entry_type, int_field, message_role, str_field, LeafSelector, PureSettings,
};
use super::projection::{build_session_projection, get_latest_compaction_entry};
use super::tokens::{
    calculate_context_tokens, estimate_context_tokens, estimate_projected_context_tokens,
};

/// `shouldCompact(contextTokens, contextWindow, settings)`.
pub fn should_compact(context_tokens: i64, context_window: i64, settings: &PureSettings) -> bool {
    if !settings.enabled {
        return false;
    }
    context_tokens > context_window - settings.reserve_tokens
}

fn lower(text: &str) -> String {
    text.to_lowercase()
}

fn count_digits_commas(text: &str) -> usize {
    text.chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',')
        .count()
}

fn digits_then(text: &str, prefix: &str, suffix: &str) -> bool {
    if let Some(rest) = text.find(prefix).map(|pos| &text[pos + prefix.len()..]) {
        let digits = count_digits_commas(rest);
        if digits > 0 {
            return rest[digits..].starts_with(suffix);
        }
    }
    false
}

fn digits_after(text: &str, prefix: &str) -> bool {
    text.find(prefix)
        .map(|pos| {
            text[pos + prefix.len()..]
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

fn sequence(text: &str, parts: &[&str]) -> bool {
    let mut offset = 0usize;
    for part in parts {
        match text[offset..].find(part) {
            Some(pos) => offset += pos + part.len(),
            None => return false,
        }
    }
    true
}

/// `/exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))/i`
fn matches_max_context_length(text: &str) -> bool {
    const BASES: [&str; 4] = [
        "exceeds maximum context length",
        "exceeds the maximum context length",
        "exceeds model's maximum context length",
        "exceeds the model's maximum context length",
    ];
    for base in BASES {
        if let Some(rest) = text.find(base).map(|pos| &text[pos + base.len()..]) {
            if digits_then(rest, " of ", " token") {
                return true;
            }
            let trimmed = rest.trim_start();
            if let Some(after_paren) = trimmed.strip_prefix('(') {
                let digits = count_digits_commas(after_paren);
                if digits > 0 && after_paren[digits..].starts_with(')') {
                    return true;
                }
            }
        }
    }
    false
}

/// `/input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)/i`
fn matches_input_longer_than_model(text: &str) -> bool {
    let Some(pos) = text.find("input (") else {
        return false;
    };
    let rest = &text[pos + "input (".len()..];
    let digits = count_digits_commas(rest);
    if digits == 0 {
        return false;
    }
    let rest = &rest[digits..];
    let Some(rest) = rest.strip_prefix(" tokens) is longer than the model") else {
        return false;
    };
    let rest = rest.strip_prefix('\'').unwrap_or(rest);
    let Some(rest) = rest.strip_prefix("s context length (") else {
        return false;
    };
    let digits = count_digits_commas(rest);
    digits > 0 && rest[digits..].starts_with(" tokens)")
}

/// `/prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?/i`
fn matches_prompt_has_configured_size(text: &str) -> bool {
    let Some(pos) = text.find("prompt has ") else {
        return false;
    };
    let rest = &text[pos + "prompt has ".len()..];
    let digits = count_digits_commas(rest);
    if digits == 0 {
        return false;
    }
    let Some(rest) = rest[digits..].strip_prefix(" token") else {
        return false;
    };
    let rest = rest.strip_prefix('s').unwrap_or(rest);
    let Some(rest) = rest.strip_prefix(", but the configured context size is ") else {
        return false;
    };
    let digits = count_digits_commas(rest);
    if digits == 0 {
        return false;
    }
    rest[digits..].starts_with(" token")
}

/// `/prompt too long; exceeded (?:max )?context length/i`
fn matches_ollama_overflow(text: &str) -> bool {
    let Some(rest) = text
        .find("prompt too long; exceeded ")
        .map(|pos| &text[pos + "prompt too long; exceeded ".len()..])
    else {
        return false;
    };
    let rest = rest.strip_prefix("max ").unwrap_or(rest);
    rest.starts_with("context length")
}

/// `/context[_ ]length[_ ]exceeded/i`
fn matches_context_length_exceeded(text: &str) -> bool {
    [
        "context_length_exceeded",
        "context length_exceeded",
        "context_length exceeded",
        "context length exceeded",
    ]
    .iter()
    .any(|pattern| text.contains(pattern))
}

/// `OVERFLOW_PATTERNS.some(...)` over a lowercased message.
fn matches_overflow_pattern(text: &str) -> bool {
    text.contains("prompt too long")
        || text.contains("prompt is too long")
        || text.contains("prompt exceeds max length")
        || text.contains("request_too_large")
        || text.contains("input is too long for requested model")
        || text.contains("exceeds the context window")
        || matches_max_context_length(text)
        || sequence(text, &["input token count", "exceeds the maximum"])
        || digits_after(text, "maximum prompt length is ")
        || text.contains("reduce the length of the messages")
        || digits_then(text, "maximum context length is ", " tokens")
        || digits_after(text, "exceeds maximum allowed input length of ")
        || digits_after(text, "exceeds the maximum allowed input length of ")
        || matches_input_longer_than_model(text)
        || digits_after(text, "exceeds the limit of ")
        || text.contains("exceeds the available context size")
        || text.contains("greater than the context length")
        || text.contains("context window exceeds limit")
        || text.contains("exceeded model token limit")
        || digits_after(text, "too large for model with ")
        || matches_prompt_has_configured_size(text)
        || text.contains("model_context_window_exceeded")
        || matches_ollama_overflow(text)
        || text.contains("range of input length should be")
        || matches_context_length_exceeded(text)
        || text.contains("too many tokens")
        || text.contains("token limit exceeded")
}

/// `NON_OVERFLOW_PATTERNS.some(...)`.
fn matches_non_overflow_pattern(text: &str) -> bool {
    text.starts_with("throttling error:")
        || text.starts_with("service unavailable:")
        || text.contains("rate limit")
        || text.contains("too many requests")
}

/// `/^4(?:00|13)\s*(?:status code)?\s*\(no body\)/i`
fn matches_cerebras_bodiless(text: &str) -> bool {
    let rest = if let Some(rest) = text.strip_prefix("400") {
        rest
    } else if let Some(rest) = text.strip_prefix("413") {
        rest
    } else {
        return false;
    };
    let rest = rest.trim_start();
    let rest = rest
        .strip_prefix("status code")
        .map(str::trim_start)
        .unwrap_or(rest);
    rest.starts_with("(no body)")
}

fn usage_tokens(message: &Value, key: &str) -> i64 {
    message
        .get("usage")
        .and_then(|usage| int_field(usage, key))
        .unwrap_or(0)
}

/// `isContextOverflow(message, contextWindow?)`.
pub fn is_context_overflow(message: &Value, context_window: Option<i64>) -> bool {
    if str_field(message, "stopReason") == Some("error") {
        if let Some(error_message) = message
            .get("errorMessage")
            .filter(|value| value.as_str().map(|text| !text.is_empty()).unwrap_or(false))
        {
            let text = lower(error_message.as_str().unwrap_or(""));
            if !matches_non_overflow_pattern(&text) {
                if matches_overflow_pattern(&text) {
                    return true;
                }
                if str_field(message, "provider") == Some("cerebras")
                    && matches_cerebras_bodiless(&text)
                {
                    return true;
                }
            }
        }
    }
    let window = context_window.filter(|window| *window != 0);
    if let Some(window) = window {
        let stop_reason = str_field(message, "stopReason").unwrap_or("");
        let input_tokens = usage_tokens(message, "input") + usage_tokens(message, "cacheRead");
        if stop_reason == "stop" && input_tokens > window {
            return true;
        }
        if stop_reason == "length" && usage_tokens(message, "output") == 0 {
            if input_tokens as f64 >= window as f64 * 0.99 {
                return true;
            }
        }
    }
    false
}

/// `isRecoverableLength(message, desiredMaxOutput)`.
pub fn is_recoverable_length(message: &Value, desired_max_output: i64) -> bool {
    str_field(message, "stopReason") == Some("length")
        && desired_max_output > 0
        && usage_tokens(message, "output") < desired_max_output
}

/// Which assistant message the recovery path marked failed, mirroring
/// `_failedResponse === assistantMessage` plus `assistantEntryId ?? true`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailedResponse {
    EntryId(String),
    Unidentified,
}

/// Inputs for the `_checkCompaction` decision table. The persisted-entry lookup
/// (`_findPersistedMessageEntryId`) is injected as `assistant_entry_id`.
#[derive(Debug, Clone)]
pub struct CheckCompactionInput {
    pub settings: PureSettings,
    pub model: Value,
    pub message: Value,
    pub branch: Vec<Value>,
    pub agent_messages: Vec<Value>,
    pub assistant_entry_id: Option<String>,
    pub skip_aborted_check: bool,
    pub overflow_recovery_attempted: bool,
    /// Stubbed `_runAutoCompaction` return; `None` means the upstream default
    /// (`willRetry` for overflow, `false` otherwise).
    pub run_returns: Option<bool>,
}

/// Ordered outcome of the decision table.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckCompactionOutcome {
    pub steps: Vec<String>,
    pub result: bool,
    pub overflow_recovery_attempted: bool,
    pub failed_response: Option<FailedResponse>,
}

fn model_matches(message: &Value, model: &Value) -> bool {
    str_field(model, "provider") == str_field(message, "provider")
        && str_field(model, "id") == str_field(message, "model")
}

fn replacement_is_null(entry: &Value) -> bool {
    matches!(entry.get("replacement"), Some(Value::Null))
}

/// `_checkCompaction(assistantMessage, skipAbortedCheck, toolResults)`.
///
/// Records the ordered side effects (`_omitRecoveryAttempt`, `_runAutoCompaction`,
/// `_emit`, `_emitSessionCompactFailed`) so the fixture can compare the complete
/// sequence, not just the boolean.
pub fn check_compaction(input: &CheckCompactionInput) -> CheckCompactionOutcome {
    let mut steps: Vec<String> = Vec::new();
    let mut overflow_recovery_attempted = input.overflow_recovery_attempted;
    let mut failed_response: Option<FailedResponse> = None;

    let settings = input.settings;
    let message = &input.message;
    let model = &input.model;
    let stop_reason = str_field(message, "stopReason").unwrap_or("");

    let run = |steps: &mut Vec<String>, reason: &str, will_retry: bool| -> bool {
        steps.push(format!("runAutoCompaction:{reason}:{will_retry}"));
        input.run_returns.unwrap_or(if reason == "overflow" {
            will_retry
        } else {
            false
        })
    };
    let skip = |result: bool,
                overflow_recovery_attempted: bool,
                failed_response: Option<FailedResponse>|
     -> CheckCompactionOutcome {
        CheckCompactionOutcome {
            steps: Vec::new(),
            result,
            overflow_recovery_attempted,
            failed_response,
        }
    };

    if !settings.enabled {
        return skip(false, overflow_recovery_attempted, failed_response);
    }
    if input.skip_aborted_check && stop_reason == "aborted" {
        return skip(false, overflow_recovery_attempted, failed_response);
    }

    let same_model = model_matches(message, model);
    let context_window = int_field(model, "contextWindow").unwrap_or(0);

    let latest_compaction = get_latest_compaction_entry(&input.branch).cloned();
    if let Some(compaction) = &latest_compaction {
        let message_timestamp = message.get("timestamp").and_then(|v| v.as_i64());
        let compaction_timestamp = compaction.get("timestamp").and_then(|v| v.as_i64());
        let before_compaction = match (message_timestamp, compaction_timestamp) {
            (Some(message), Some(compaction)) => message <= compaction,
            _ => false,
        };
        if before_compaction {
            return skip(false, overflow_recovery_attempted, failed_response);
        }
    }

    let projection = build_session_projection(&input.branch, &LeafSelector::Default);
    let assistant_is_projected = match &input.assistant_entry_id {
        None => true,
        Some(id) => projection.entries.iter().any(|entry| {
            entry_id(&entry.source) == Some(id.as_str())
                && entry
                    .messages
                    .iter()
                    .any(|message| message_role(message) == "assistant")
        }),
    };
    let assistant_index = input.assistant_entry_id.as_deref().and_then(|id| {
        input
            .branch
            .iter()
            .position(|entry| entry_id(entry) == Some(id))
    });
    let entries_after: &[Value] = match assistant_index {
        Some(index) if index + 1 <= input.branch.len() => &input.branch[index + 1..],
        _ => &[],
    };
    let has_post_assistant_context_edit = entries_after
        .iter()
        .any(|entry| entry_type(entry) == "context_edit");
    let latest_assistant_edit = entries_after
        .iter()
        .filter(|entry| {
            entry_type(entry) == "context_edit"
                && str_field(entry, "targetId") == input.assistant_entry_id.as_deref()
        })
        .next_back();
    let assistant_retained_for_explicit_recovery = match &input.assistant_entry_id {
        None => true,
        Some(_) => {
            !entries_after
                .iter()
                .any(|entry| entry_type(entry) == "compaction")
                && latest_assistant_edit
                    .map(|entry| !replacement_is_null(entry))
                    .unwrap_or(true)
        }
    };
    let assistant_usage_matches_projection =
        assistant_is_projected && !has_post_assistant_context_edit;

    let explicit_overflow = stop_reason == "error" && is_context_overflow(message, None);
    let context_overflow = same_model
        && ((explicit_overflow && assistant_retained_for_explicit_recovery)
            || (assistant_usage_matches_projection
                && is_context_overflow(message, Some(context_window))));
    let recoverable_length = same_model
        && assistant_is_projected
        && is_recoverable_length(message, int_field(model, "maxTokens").unwrap_or(0));

    if context_overflow || recoverable_length {
        let will_retry = stop_reason != "stop";
        if !will_retry {
            let result = run(&mut steps, "overflow", false);
            return CheckCompactionOutcome {
                steps,
                result,
                overflow_recovery_attempted,
                failed_response,
            };
        }
        if overflow_recovery_attempted {
            let reason = if context_overflow {
                "overflow"
            } else {
                "length"
            };
            steps.push("emit:compaction_end".to_string());
            steps.push(format!(
                "session_compact_failed:{reason}:aborted=false:willRetry=false"
            ));
            return CheckCompactionOutcome {
                steps,
                result: false,
                overflow_recovery_attempted,
                failed_response,
            };
        }
        overflow_recovery_attempted = true;
        steps.push("omitRecoveryAttempt".to_string());
        let retry = run(&mut steps, "overflow", will_retry);
        if retry {
            failed_response = Some(match &input.assistant_entry_id {
                Some(id) => FailedResponse::EntryId(id.clone()),
                None => FailedResponse::Unidentified,
            });
        }
        return CheckCompactionOutcome {
            steps,
            result: retry,
            overflow_recovery_attempted,
            failed_response,
        };
    }

    let has_context_edits = projection
        .entries
        .iter()
        .any(|entry| entry_type(&entry.source) == "context_edit");
    let direct_context_tokens = message
        .get("usage")
        .map(calculate_context_tokens)
        .unwrap_or(0);
    let context_tokens = if has_context_edits {
        estimate_projected_context_tokens(&projection, &input.branch).tokens
    } else if stop_reason == "error" || direct_context_tokens == 0 {
        let estimate = estimate_context_tokens(&input.agent_messages);
        if let Some(index) = estimate.last_usage_index {
            if let Some(usage_message) = input.agent_messages.get(index) {
                let usage_timestamp = usage_message.get("timestamp").and_then(|v| v.as_i64());
                let compaction_timestamp = latest_compaction
                    .as_ref()
                    .and_then(|compaction| compaction.get("timestamp"))
                    .and_then(|v| v.as_i64());
                if latest_compaction.is_some() && message_role(usage_message) == "assistant" {
                    if let (Some(usage_timestamp), Some(compaction_timestamp)) =
                        (usage_timestamp, compaction_timestamp)
                    {
                        if usage_timestamp <= compaction_timestamp {
                            return skip(false, overflow_recovery_attempted, failed_response);
                        }
                    }
                }
            }
        }
        estimate.tokens
    } else {
        direct_context_tokens
    };

    if should_compact(context_tokens, context_window, &settings) {
        let result = run(&mut steps, "threshold", false);
        return CheckCompactionOutcome {
            steps,
            result,
            overflow_recovery_attempted,
            failed_response,
        };
    }
    skip(false, overflow_recovery_attempted, failed_response)
}
