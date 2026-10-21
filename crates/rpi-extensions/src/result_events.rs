//! Ordered, awaited native event callbacks, sharing the existing observer order.
use crate::{
    private_callback::Callback,
    registry::{RegisteredHandler, RegistrySnapshot},
    ActionBridge, PluginKeepalive,
};
use rpi_plugin_sdk::{EventTag, StablePluginEvent, StbString};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(crate) struct Handler {
    pub plugin: String,
    pub priority: i32,
    pub platforms: Vec<String>,
    pub callback: Callback,
    pub bridge: Option<Arc<ActionBridge>>,
}
#[derive(Clone)]
pub(crate) enum Listener {
    Observer(RegisteredHandler),
    Result(Handler),
}
impl Listener {
    pub fn priority(&self) -> i32 {
        match self {
            Self::Observer(h) => h.priority,
            Self::Result(h) => h.priority,
        }
    }
    pub fn set_priority(&mut self, priority: i32) {
        match self {
            Self::Observer(h) => h.priority = priority,
            Self::Result(h) => h.priority = priority,
        }
    }
    pub fn set_platforms(&mut self, platforms: &[String]) {
        match self {
            Self::Observer(h) => h.platforms = platforms.to_vec(),
            Self::Result(h) => h.platforms = platforms.to_vec(),
        }
    }
    fn platforms(&self) -> &[String] {
        match self {
            Self::Observer(h) => &h.platforms,
            Self::Result(h) => &h.platforms,
        }
    }
    fn plugin(&self) -> &str {
        match self {
            Self::Observer(h) => &h.plugin,
            Self::Result(h) => &h.plugin,
        }
    }
}

#[derive(Debug)]
pub struct Outcome {
    pub event: Value,
    /// Missing means undefined; Some(Null) is an explicit null result.
    pub result: Option<Value>,
    pub messages: Vec<Value>,
}

/// Invoke listeners in declaration/registration order. Authentication material
/// belongs to private provider ports and must never be included in `event`.
pub async fn dispatch(
    snapshot: Arc<RegistrySnapshot>,
    keepalive: Arc<PluginKeepalive>,
    tag: EventTag,
    event: Value,
    identity: Option<rpi_ai::identity::RequestIdentity>,
    signal: CancellationToken,
) -> Result<Outcome, String> {
    dispatch_borrowed(&snapshot, keepalive, tag, event, identity, signal).await
}

pub(crate) async fn dispatch_borrowed(
    snapshot: &RegistrySnapshot,
    keepalive: Arc<PluginKeepalive>,
    tag: EventTag,
    event: Value,
    identity: Option<rpi_ai::identity::RequestIdentity>,
    signal: CancellationToken,
) -> Result<Outcome, String> {
    let mut outcome = Outcome {
        event,
        result: None,
        messages: Vec::new(),
    };
    for listener in snapshot.listeners(tag) {
        if signal.is_cancelled() {
            return Err("event cancelled".into());
        }
        if !snapshot.is_active() {
            return Err("inactive event generation".into());
        }
        if !crate::registry::platform_allows(listener.platforms()) {
            continue;
        }
        let result = match listener {
            Listener::Result(h) => {
                let bridge = h.bridge.as_ref().map(|bridge| {
                    bridge.for_invocation(
                        rpi_agent::tool_execution_scope::is_child_execution(),
                        signal.clone(),
                    )
                });
                crate::private_callback::invoke_scoped(
                    h.callback.clone(),
                    keepalive.clone(),
                    json!({"version":1,"tag":tag as u32,"event":outcome.event,"identity":identity})
                        .to_string(),
                    signal.clone(),
                    false,
                    bridge,
                )
                .await
                .map_err(|error| {
                    if matches!(error, rpi_ai::auth::AuthError::Cancelled) {
                        "event cancelled".into()
                    } else {
                        "event callback failed".to_string()
                    }
                })
                .and_then(|response| {
                    if response["version"] != 1 {
                        return Err("unsupported event response version".into());
                    }
                    if let Some(error) = response.get("error").and_then(Value::as_str) {
                        return Err(error.to_owned());
                    }
                    Ok(response.get("result").cloned())
                })
            }
            Listener::Observer(h) => {
                let h = h.clone();
                let event = outcome.event.clone();
                let keepalive = keepalive.clone();
                let background = rpi_agent::tool_execution_scope::track_background_tool();
                let pending = tokio::task::spawn_blocking(move || {
                    let _keepalive = keepalive;
                    let _background = background;
                    invoke_observer(h, tag, event)
                });
                tokio::select! {
                    biased;
                    _ = signal.cancelled() => return Err("event cancelled".into()),
                    result = pending => result.map_err(|_| "event callback failed".to_owned()).and_then(|r| r),
                }
            }
        };
        match result {
            Ok(Some(value)) => match reduce(tag, &mut outcome, value) {
                Ok(true) => break,
                Ok(false) => {}
                Err(error) if tag == EventTag::ToolCall => return Err(error),
                Err(_) => tracing::warn!(
                    plugin = listener.plugin(),
                    ?tag,
                    "invalid extension event result"
                ),
            },
            Ok(None) => {}
            Err(error) => {
                if signal.is_cancelled() || tag == EventTag::ToolCall {
                    return Err(error);
                }
                tracing::warn!(
                    plugin = listener.plugin(),
                    ?tag,
                    "extension event callback failed"
                );
            }
        }
    }
    if !snapshot.is_active() {
        return Err("inactive event generation".into());
    }
    Ok(outcome)
}

fn reduce(tag: EventTag, outcome: &mut Outcome, result: Value) -> Result<bool, String> {
    use EventTag as T;
    match tag {
        T::BeforeProviderRequest => outcome.event["payload"] = result.clone(),
        T::Context => {
            if let Some(messages) = result.get("messages") {
                if !messages.is_array() {
                    return Err("context result messages must be an array".into());
                }
                outcome.event["messages"] = messages.clone();
            }
        }
        T::BeforeAgentStart => {
            if let Some(prompt) = result.get("systemPrompt") {
                if !prompt.is_string() {
                    return Err("system prompt must be a string".into());
                }
                outcome.event["systemPrompt"] = prompt.clone();
            }
            if let Some(message) = result.get("message") {
                outcome.messages.push(message.clone());
            }
        }
        T::BeforeProviderHeaders => {
            if let Some(headers) = result.get("headers") {
                if !headers.is_object() {
                    return Err("headers must be an object".into());
                }
                outcome.event["headers"] = headers.clone();
            }
        }
        T::MessageEnd => {
            if let Some(message) = result.get("message") {
                outcome.event["message"] = message.clone();
            }
        }
        T::ToolResult => {
            if result.get("content").is_some() && result.get("structuredContent").is_none() {
                outcome
                    .event
                    .as_object_mut()
                    .ok_or("invalid tool result event")?
                    .remove("structuredContent");
            }
            for key in [
                "content",
                "details",
                "structuredContent",
                "isError",
                "usage",
            ] {
                if let Some(value) = result.get(key) {
                    outcome.event[key] = value.clone();
                }
            }
        }
        _ => {}
    }
    let stop = match tag {
        T::ToolCall => result["block"] == true,
        T::SessionBeforeSwitch
        | T::SessionBeforeFork
        | T::SessionBeforeCompact
        | T::SessionBeforeTree
        | T::SessionStart
        | T::SessionShutdown
        | T::BeforeTuiStart => result["cancel"] == true,
        _ => false,
    };
    outcome.result = Some(result);
    Ok(stop)
}

fn invoke_observer(
    h: RegisteredHandler,
    tag: EventTag,
    event: Value,
) -> Result<Option<Value>, String> {
    struct Strings(Vec<StbString>);
    impl Strings {
        fn string(&mut self, value: String) -> StbString {
            let value = StbString::from_string(value);
            self.0.push(value);
            value
        }
    }
    impl Drop for Strings {
        fn drop(&mut self) {
            for value in self.0.drain(..) {
                unsafe {
                    crate::host_free_string(value);
                }
            }
        }
    }
    let mut strings = Strings(Vec::new());
    use EventTag as T;
    let stable = match tag {
        T::MessageStart | T::MessageUpdate | T::MessageEnd => {
            StablePluginEvent::message(tag, strings.string(event["message"].to_string()))
        }
        T::ToolCall | T::ToolExecutionStart | T::ToolExecutionUpdate => {
            StablePluginEvent::tool_call(
                tag,
                strings.string(event["toolCallId"].as_str().unwrap_or_default().into()),
                strings.string(event["toolName"].as_str().unwrap_or_default().into()),
                strings.string(event["input"].to_string()),
            )
        }
        T::ToolResult | T::ToolExecutionEnd => StablePluginEvent::tool_result(
            tag,
            strings.string(event["toolCallId"].as_str().unwrap_or_default().into()),
            strings.string(event["toolName"].as_str().unwrap_or_default().into()),
            strings.string(event.to_string()),
            event["isError"] == true,
        ),
        T::ProjectTrust
        | T::ResourcesDiscover
        | T::SessionStart
        | T::SessionShutdown
        | T::AgentStart
        | T::AgentEnd
        | T::AgentSettled
        | T::TurnStart
        | T::TurnEnd
        | T::BeforeTuiStart => StablePluginEvent::empty(tag),
        _ => StablePluginEvent::data(tag, strings.string(event.to_string())),
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (h.handler)(stable, h.user_data)
    }))
    .map_err(|_| "event callback panicked".to_string())?;
    if result == 0 || (result == rpi_plugin_sdk::EVENT_HANDLER_CLAIMED && tag != EventTag::ToolCall)
    {
        Ok(None)
    } else if result == rpi_plugin_sdk::EVENT_HANDLER_ABORT && tag == EventTag::ToolCall {
        Ok(Some(json!({"block":true})))
    } else if result == rpi_plugin_sdk::EVENT_HANDLER_ABORT {
        Ok(Some(
            json!({"cancel":true,"reason":"extension vetoed lifecycle event"}),
        ))
    } else {
        Err("event observer failed".into())
    }
}

#[cfg(test)]
mod tests;
