//! Private native provider wire format. Never forward these options to observer events.
//! Shared by both directions of delegation so request options and compact events agree.
use crate::{AssistantMessage, AssistantMessageEvent, Model, SimpleStreamOptions};
use std::{sync::Arc, time::Duration};
mod decoder;
pub use decoder::StreamMapper;

pub fn encode_options(opts: &SimpleStreamOptions) -> serde_json::Value {
    serde_json::json!({
        "apiKey": opts.api_key,
        "timeoutMs": opts.timeout.map(|d| d.as_millis() as u64),
        "maxRetries": opts.max_retries,
        "maxRetryDelayMs": opts.max_retry_delay.map(|d| d.as_millis() as u64),
        "headers": opts.headers,
        "env": opts.env,
        "authMode": match opts.authentication {
            crate::auth::AttemptAuth::Ambient => "ambient",
            crate::auth::AttemptAuth::Replace(_) => "replace",
        },
        "responseMetadata": opts.response_observer.is_some(),
        "metadata": opts.metadata,
        "cacheRetention": format!("{:?}", opts.cache_retention),
        "sessionId": opts.session_id,
        "reasoning": opts.reasoning.map(|r| format!("{r:?}")),
        "maxTokens": opts.max_tokens,
        "temperature": opts.temperature,
        "thinkingBudgets": opts.thinking_budgets,
    })
}

pub fn decode_options(value: &serde_json::Value, model: &Model) -> Result<SimpleStreamOptions, ()> {
    let mut options = SimpleStreamOptions::default();
    options.api_key = value["apiKey"].as_str().map(str::to_string);
    options.timeout = value["timeoutMs"].as_u64().map(Duration::from_millis);
    options.max_retry_delay = value["maxRetryDelayMs"].as_u64().map(Duration::from_millis);
    options.max_retries = value["maxRetries"].as_u64().and_then(|n| n.try_into().ok());
    options.max_tokens = value["maxTokens"].as_u64();
    options.temperature = value["temperature"].as_f64();
    options.session_id = value["sessionId"].as_str().map(str::to_string);
    options.headers = serde_json::from_value(value["headers"].clone()).ok();
    options.env = if value["env"].is_null() {
        Default::default()
    } else {
        serde_json::from_value(value["env"].clone()).map_err(|_| ())?
    };
    options.metadata = serde_json::from_value(value["metadata"].clone()).ok();
    options.reasoning = value["reasoning"]
        .as_str()
        .and_then(|s| serde_json::from_value(serde_json::json!(s.to_ascii_lowercase())).ok());
    options.thinking_budgets = serde_json::from_value(value["thinkingBudgets"].clone()).ok();
    options.cache_retention = match value["cacheRetention"].as_str() {
        Some("None") => crate::CacheRetention::None,
        Some("Long") => crate::CacheRetention::Long,
        _ => crate::CacheRetention::Short,
    };
    match value.get("authMode") {
        None => {}
        Some(serde_json::Value::String(mode)) if mode == "ambient" => {}
        Some(serde_json::Value::String(mode)) if mode == "replace" => {
            options.authentication = crate::auth::ReplacementAuth::from_private_request(
                model.provider.clone(),
                crate::auth::RequestAuth {
                    api_key: options.api_key.clone(),
                    headers: options.headers.clone().unwrap_or_default(),
                    base_url: Some(model.base_url.clone()),
                    env: options.env.clone(),
                    source: None,
                },
            );
        }
        _ => return Err(()),
    }
    Ok(options)
}

/// Compare every field without constructing a candidate message or JSON tree.
fn reconstructs_text(
    previous: &AssistantMessage,
    next: &AssistantMessage,
    index: usize,
    delta: &str,
) -> bool {
    let AssistantMessage {
        role,
        content,
        api,
        provider,
        model,
        response_model,
        response_id,
        usage,
        stop_reason,
        deferred,
        error_message,
        raw_stop_reason,
        end_turn,
        timestamp,
    } = previous;
    if role != &next.role
        || api != &next.api
        || provider != &next.provider
        || model != &next.model
        || response_model != &next.response_model
        || response_id != &next.response_id
        || usage != &next.usage
        || stop_reason != &next.stop_reason
        || deferred != &next.deferred
        || error_message != &next.error_message
        || raw_stop_reason != &next.raw_stop_reason
        || end_turn != &next.end_turn
        || timestamp != &next.timestamp
        || content.len() != next.content.len()
    {
        return false;
    }
    let (Some(crate::Content::Text(before)), Some(crate::Content::Text(after))) =
        (content.get(index), next.content.get(index))
    else {
        return false;
    };
    let crate::TextContent {
        kind,
        text,
        text_signature,
    } = before;
    kind == &after.kind
        && text_signature == &after.text_signature
        && after.text.strip_prefix(text) == Some(delta)
        && content
            .iter()
            .zip(&next.content)
            .enumerate()
            .all(|(i, (a, b))| i == index || a == b)
}

pub fn encode_event(
    event: &AssistantMessageEvent,
    previous: &mut Option<(usize, Arc<AssistantMessage>)>,
) -> serde_json::Value {
    let compact = match (event, previous.as_ref()) {
        (
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                partial,
            },
            Some((index, before)),
        ) => index == content_index && reconstructs_text(before, partial, *index, delta),
        _ => false,
    };
    *previous = match event {
        AssistantMessageEvent::TextDelta {
            content_index,
            partial,
            ..
        } => Some((*content_index, Arc::clone(partial))),
        _ => None,
    };
    if compact {
        let AssistantMessageEvent::TextDelta {
            content_index,
            delta,
            ..
        } = event
        else {
            unreachable!()
        };
        return serde_json::json!({"type":"text_delta","contentIndex":content_index,"delta":delta});
    }
    let mut chunk = serde_json::to_value(event).expect("provider event JSON");
    // The SDK uses flat tool-call identity fields.
    let tool = chunk.get("toolCall").cloned().or_else(|| {
        if event.type_tag() != "toolcall_start" {
            return None;
        }
        let index = chunk["contentIndex"].as_u64()? as usize;
        chunk.get("partial")?.get("content")?.get(index).cloned()
    });
    if let Some(tool) = tool {
        for field in ["id", "name", "arguments"] {
            if let Some(value) = tool.get(field) {
                chunk[field] = value.clone();
            }
        }
    }
    chunk
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StopReason;
    #[test]
    fn compaction_requires_exact_text_and_metadata_and_consecutive_deltas() {
        let mut before = AssistantMessage::empty(crate::Api::Faux, "faux", "m", 42);
        before.content = vec![crate::Content::text("α"), crate::Content::thinking("hmm")];
        let mut after = before.clone();
        let crate::Content::Text(text) = &mut after.content[0] else {
            unreachable!()
        };
        text.text.push_str("🐈");
        assert!(reconstructs_text(&before, &after, 0, "🐈"));
        assert!(reconstructs_text(&before, &before, 0, ""));
        assert!(!reconstructs_text(&before, &after, 0, "α🐈"));
        assert!(!reconstructs_text(&before, &after, 1, "🐈"));
        let mutations: &[fn(&mut AssistantMessage)] = &[
            |m| m.api = crate::Api::AnthropicMessages,
            |m| m.provider.push('x'),
            |m| m.model.push('x'),
            |m| m.response_model = Some("r".into()),
            |m| m.response_id = Some("id".into()),
            |m| m.timestamp += 1,
            |m| m.usage.output += 1,
            |m| m.stop_reason = StopReason::Stop,
            |m| m.error_message = Some("e".into()),
            |m| m.raw_stop_reason = Some("raw".into()),
            |m| m.end_turn = Some(true),
            |m| m.content[1] = crate::Content::thinking("changed"),
            |m| m.content.push(crate::Content::text("extra")),
            |m| {
                if let crate::Content::Text(t) = &mut m.content[0] {
                    t.text_signature = Some("signature".into());
                }
            },
        ];
        for mutate in mutations {
            let mut changed = after.clone();
            mutate(&mut changed);
            assert!(!reconstructs_text(&before, &changed, 0, "🐈"));
        }
        let first = AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "α".into(),
            partial: Arc::new(before),
        };
        let next = AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "🐈".into(),
            partial: Arc::new(after),
        };
        let mut previous = None;
        assert!(encode_event(&first, &mut previous).get("partial").is_some());
        assert!(encode_event(&next, &mut previous).get("partial").is_none());
        encode_event(
            &AssistantMessageEvent::Start {
                partial: Arc::new(first.partial().clone()),
            },
            &mut previous,
        );
        assert!(encode_event(&next, &mut previous).get("partial").is_some());
    }
}
