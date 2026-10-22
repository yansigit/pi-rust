//! Host-independent model of `AgentSession._runAutoCompaction`'s ordered
//! operations and emitted events.
//!
//! The fixture drives the unchanged upstream method with injected stubs and
//! records the exact call order; this function reproduces that order from the
//! control flow so the host can assert its own dispatch sequence. `reason` and
//! `willRetry` mirror the upstream arguments, and the flags mirror the stub
//! configuration (`defaultError`, `appendError`, extension cancel/compaction, an
//! aborted signal, queued messages, missing model or preparation).

use serde_json::{json, Value};

/// Stub/scenario inputs for the lifecycle order model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoCompactionInput {
    pub reason: String,
    pub will_retry: bool,
    pub has_model: bool,
    pub has_preparation: bool,
    pub extension_handlers: bool,
    pub extension_cancel: bool,
    pub extension_compaction: bool,
    pub default_error: bool,
    pub append_error: bool,
    pub aborted: bool,
    pub queued: bool,
}

impl AutoCompactionInput {
    pub fn from_value(value: &Value) -> Self {
        let inputs = value.get("inputs").unwrap_or(value);
        AutoCompactionInput {
            reason: super::model::str_field(value, "reason")
                .unwrap_or("threshold")
                .to_string(),
            will_retry: super::model::bool_field(value, "willRetry").unwrap_or(false),
            has_model: super::model::bool_field(inputs, "hasModel").unwrap_or(true),
            has_preparation: super::model::bool_field(inputs, "hasPreparation").unwrap_or(true),
            extension_handlers: super::model::bool_field(inputs, "extensionHandlers")
                .unwrap_or(false),
            extension_cancel: super::model::bool_field(inputs, "extensionCancel").unwrap_or(false),
            extension_compaction: super::model::bool_field(inputs, "extensionCompaction")
                .unwrap_or(false),
            default_error: super::model::bool_field(inputs, "defaultError").unwrap_or(false),
            append_error: super::model::bool_field(inputs, "appendError").unwrap_or(false),
            aborted: super::model::bool_field(inputs, "aborted").unwrap_or(false),
            queued: super::model::bool_field(inputs, "queued").unwrap_or(false),
        }
    }
}

/// Ordered outcome of the lifecycle model.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoCompactionOutcome {
    pub ops: Vec<String>,
    pub events: Vec<Value>,
    pub result: Option<bool>,
    pub threw: bool,
    pub error_message: Option<String>,
}

fn emit_event(
    event_type: &str,
    reason: &str,
    aborted: Option<bool>,
    will_retry: Option<bool>,
    error_message: Option<&str>,
) -> Value {
    json!({
        "type": event_type,
        "reason": reason,
        "aborted": aborted,
        "willRetry": will_retry,
        "errorMessage": error_message,
    })
}

fn extension_event(event_type: &str, reason: &str, will_retry: bool) -> Value {
    json!({
        "type": event_type,
        "reason": reason,
        "willRetry": will_retry,
    })
}

fn failed_event(
    reason: &str,
    aborted: bool,
    will_retry: bool,
    error_message: Option<&str>,
) -> Value {
    json!({
        "type": "session_compact_failed",
        "reason": reason,
        "aborted": aborted,
        "willRetry": will_retry,
        "errorMessage": error_message,
    })
}

/// `_runAutoCompaction(reason, willRetry)` ordered operations.
pub fn auto_compaction_order(input: &AutoCompactionInput) -> AutoCompactionOutcome {
    let mut ops: Vec<String> = Vec::new();
    let mut events: Vec<Value> = Vec::new();
    let reason = input.reason.as_str();

    if !input.has_model || !input.has_preparation {
        ops.push("resolveIdleWaitIfIdle".to_string());
        return AutoCompactionOutcome {
            ops,
            events,
            result: Some(false),
            threw: false,
            error_message: None,
        };
    }

    let failure = |ops: &mut Vec<String>,
                   events: &mut Vec<Value>,
                   aborted: bool,
                   message: Option<String>|
     -> AutoCompactionOutcome {
        let error_message = if aborted {
            None
        } else {
            message.map(|message| {
                if reason == "overflow" {
                    format!("Context overflow recovery failed: {message}")
                } else {
                    format!("Auto-compaction failed: {message}")
                }
            })
        };
        ops.push("emit:compaction_end".to_string());
        events.push(emit_event(
            "compaction_end",
            reason,
            Some(aborted),
            Some(false),
            error_message.as_deref(),
        ));
        ops.push("emitSessionCompactFailed".to_string());
        events.push(failed_event(
            reason,
            aborted,
            false,
            error_message.as_deref(),
        ));
        ops.push("resolveIdleWaitIfIdle".to_string());
        AutoCompactionOutcome {
            ops: ops.clone(),
            events: events.clone(),
            result: Some(false),
            threw: false,
            error_message,
        }
    };

    ops.push("emit:compaction_start".to_string());
    events.push(emit_event("compaction_start", reason, None, None, None));
    if input.aborted {
        return failure(&mut ops, &mut events, true, None);
    }

    let mut from_extension = false;
    if input.extension_handlers {
        ops.push("extension:session_before_compact".to_string());
        events.push(extension_event(
            "session_before_compact",
            reason,
            input.will_retry,
        ));
        if input.extension_cancel {
            return failure(&mut ops, &mut events, true, None);
        }
        if input.extension_compaction {
            from_extension = true;
        }
    }

    if !from_extension {
        ops.push("runDefaultCompaction".to_string());
        if input.default_error {
            return failure(
                &mut ops,
                &mut events,
                false,
                Some("summary failed".to_string()),
            );
        }
    }

    ops.push("appendCompaction".to_string());
    if input.append_error {
        return failure(
            &mut ops,
            &mut events,
            false,
            Some("append failed".to_string()),
        );
    }
    ops.push("getEntries".to_string());
    ops.push("refreshFinalizedContext".to_string());
    ops.push("extension:session_compact".to_string());
    events.push(extension_event("session_compact", reason, input.will_retry));
    ops.push("emit:compaction_end".to_string());
    events.push(emit_event(
        "compaction_end",
        reason,
        Some(false),
        Some(input.will_retry),
        None,
    ));
    if input.will_retry {
        ops.push("resolveIdleWaitIfIdle".to_string());
        return AutoCompactionOutcome {
            ops,
            events,
            result: Some(true),
            threw: false,
            error_message: None,
        };
    }
    ops.push("hasQueuedMessages".to_string());
    ops.push("resolveIdleWaitIfIdle".to_string());
    AutoCompactionOutcome {
        ops,
        events,
        result: Some(input.queued),
        threw: false,
        error_message: None,
    }
}
