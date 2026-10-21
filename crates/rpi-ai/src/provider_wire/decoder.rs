use crate::types::{ToolCall, ToolCallType};
use crate::{AssistantMessage, AssistantMessageEvent, Content};
use std::sync::Arc;

/// Live accumulator for the chunks a streaming provider emits.
///
/// The mapper owns the growing `AssistantMessage` the deltas are folded into, so
/// every `*_delta` event can carry a `partial` snapshot without the plugin having
/// to send one — the same shape the built-in providers produce (an
/// `Arc<AssistantMessage>` per event, cloned once per delta).
pub struct StreamMapper {
    pub output: AssistantMessage,
    /// Whether the stream has been opened by a `Start` event. The host opens it
    /// lazily on the first chunk so a plugin that emits nothing is never
    /// preceded by a stray `Start`.
    started: bool,
    /// Whether the terminal event has been produced; a post-terminal chunk is
    /// dropped (mirrors the provider-side "no events after terminal" rule).
    terminal: bool,
    /// Raw tool-call JSON accumulated per content index, for the same reason the
    /// Anthropic mapper keeps a `partial_json` buffer: the `arguments` object a
    /// `ToolCallEnd` carries is the *authoritative* parse, and intermediate
    /// parses must never produce a value the cumulative JSON cannot justify.
    tool_json: std::collections::BTreeMap<usize, String>,
    /// A chunk parked because the host's `Start` had to be delivered first. Set
    /// when a chunk arrives before the stream is open; re-queued by the driver on
    /// the next iteration so no delta is lost or reordered.
    deferred: Option<serde_json::Value>,
    /// The plugin's own final message, once it has returned. `Some` only after
    /// [`Self::chunks_then_message`].
    final_message: Option<AssistantMessage>,
}

impl StreamMapper {
    pub fn new(api: crate::types::Api, provider: String, model: String) -> Self {
        Self {
            output: AssistantMessage::empty(api, provider, model, 0),
            started: false,
            terminal: false,
            tool_json: std::collections::BTreeMap::new(),
            deferred: None,
            final_message: None,
        }
    }

    /// Take the chunk parked behind the host-emitted `Start`.
    pub fn take_deferred(&mut self) -> Option<serde_json::Value> {
        self.deferred.take()
    }

    fn ensure_started(&mut self) -> Option<AssistantMessageEvent> {
        if self.started {
            return None;
        }
        self.started = true;
        Some(AssistantMessageEvent::Start {
            partial: Arc::new(self.output.clone()),
        })
    }

    fn partial(&self) -> Arc<AssistantMessage> {
        Arc::new(self.output.clone())
    }

    pub fn aborted_message(&self) -> AssistantMessage {
        let mut message = self.output.clone();
        message.stop_reason = crate::types::StopReason::Aborted;
        message.error_message = Some("plugin provider request was cancelled".into());
        message
    }

    /// A supplied delta snapshot already includes that delta. Replace the
    /// accumulator without appending the same text/arguments a second time.
    fn apply_partial(&mut self, partial: Option<serde_json::Value>) -> bool {
        match partial.map(serde_json::from_value::<AssistantMessage>) {
            Some(Ok(message)) => {
                self.output = message;
                true
            }
            Some(Err(error)) => {
                tracing::warn!(%error, "ignoring malformed plugin provider partial snapshot");
                false
            }
            None => false,
        }
    }

    /// Fold one plugin chunk into the accumulator and return the event it maps
    /// to. `None` means "nothing to emit" (an unknown `type`, or a malformed
    /// chunk the plugin is allowed to get wrong without killing the run).
    pub fn apply(&mut self, chunk: serde_json::Value) -> Option<AssistantMessageEvent> {
        if self.terminal {
            tracing::debug!("dropping a plugin provider chunk that follows a terminal event");
            return None;
        }
        let mut chunk = chunk;
        chunk.get("type").and_then(serde_json::Value::as_str)?;

        // A chunk that opens or continues a block implies the stream is open.
        // `Start` is emitted first so the consumer's `MessageStart` never lands
        // after a delta (the agent loop and the frame encoder both require the
        // ordering). The chunk is parked and re-applied by `drive_...`'s next
        // iteration, so no delta is lost or reordered.
        if !self.started {
            self.started = true;
            if chunk["type"] == "start" {
                self.apply_partial(chunk.get("partial").cloned());
            }
            self.deferred = Some(std::mem::replace(&mut chunk, serde_json::Value::Null));
            return Some(AssistantMessageEvent::Start {
                partial: self.partial(),
            });
        }

        // Extract only after deferral: the parked frame must keep its snapshot.
        let partial = chunk.as_object_mut()?.remove("partial");
        let kind = chunk.get("type").and_then(serde_json::Value::as_str)?;
        match kind {
            // `start` is not an event of its own: the host emits `Start` above
            // the first time it sees *any* chunk, so a duplicate is dropped.
            "start" => None,
            "usage" => {
                // Usage is not an event of its own: it patches the accumulating
                // message so the terminal one reports it. A plugin that sends no
                // usage keeps the zeroed default.
                if let Some(usage) = chunk.get("usage") {
                    if let Ok(parsed) = serde_json::from_value::<crate::types::Usage>(usage.clone())
                    {
                        self.output.usage = parsed;
                    }
                }
                None
            }
            "text_start" => {
                let index = self.content_index(&chunk);
                if !self.apply_partial(partial) {
                    self.open_block(index, Content::text(String::new()));
                }
                Some(AssistantMessageEvent::TextStart {
                    content_index: index,
                    partial: self.partial(),
                })
            }
            "text_delta" => {
                let index = self.content_index(&chunk);
                let delta = chunk_string(&chunk, "delta");
                if !self.apply_partial(partial) {
                    let block = self.ensure_text_block(index);
                    block.text.push_str(&delta);
                }
                Some(AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta,
                    partial: self.partial(),
                })
            }
            "text_end" => {
                let index = self.content_index(&chunk);
                self.apply_partial(partial);
                let block = self.ensure_text_block(index);
                let content = if chunk.get("content").is_some() {
                    chunk_string(&chunk, "content")
                } else {
                    block.text.clone()
                };
                block.text = content.clone();
                Some(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content,
                    partial: self.partial(),
                })
            }
            "thinking_start" => {
                let index = self.content_index(&chunk);
                if !self.apply_partial(partial) {
                    self.open_block(index, Content::thinking(String::new()));
                }
                Some(AssistantMessageEvent::ThinkingStart {
                    content_index: index,
                    partial: self.partial(),
                })
            }
            "thinking_delta" => {
                let index = self.content_index(&chunk);
                let delta = chunk_string(&chunk, "delta");
                if !self.apply_partial(partial) {
                    let block = self.ensure_thinking_block(index);
                    block.thinking.push_str(&delta);
                }
                Some(AssistantMessageEvent::ThinkingDelta {
                    content_index: index,
                    delta,
                    partial: self.partial(),
                })
            }
            "thinking_end" => {
                let index = self.content_index(&chunk);
                self.apply_partial(partial);
                let block = self.ensure_thinking_block(index);
                let content = if chunk.get("content").is_some() {
                    chunk_string(&chunk, "content")
                } else {
                    block.thinking.clone()
                };
                block.thinking = content.clone();
                let redacted = chunk
                    .get("redacted")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(block.redacted);
                block.redacted = redacted;
                Some(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content,
                    partial: self.partial(),
                })
            }
            "toolcall_start" => {
                let index = self.content_index(&chunk);
                let id = chunk_string(&chunk, "id");
                let name = chunk_string(&chunk, "name");
                // The `arguments` snapshot at start is authoritative for the
                // arguments the block already has; the delta stream then extends
                // it. A plugin that sends only deltas starts from an empty
                // object, which is what the frame encoder expects.
                let arguments = chunk
                    .get("arguments")
                    .cloned()
                    .filter(serde_json::Value::is_object)
                    .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
                self.tool_json.insert(index, String::new());
                if !self.apply_partial(partial) {
                    self.open_block(index, Content::tool_call(id, name, arguments));
                }
                Some(AssistantMessageEvent::ToolCallStart {
                    content_index: index,
                    partial: self.partial(),
                })
            }
            "toolcall_delta" => {
                let index = self.content_index(&chunk);
                let delta = chunk_string(&chunk, "delta");
                let accumulated = self.tool_json.entry(index).or_default();
                accumulated.push_str(&delta);
                // Re-parse on every delta so the partial message renders live
                // arguments (the same invariant the Anthropic mapper holds).
                let parsed = crate::json_parse::parse_streaming_json(Some(accumulated));
                if !self.apply_partial(partial) {
                    match self.output.content.get_mut(index) {
                        Some(Content::ToolCall(call)) => call.arguments = parsed,
                        _ => {
                            let id = chunk_string(&chunk, "id");
                            let name = chunk_string(&chunk, "name");
                            self.ensure_tool_call_block(index, id, name);
                            if let Some(Content::ToolCall(call)) =
                                self.output.content.get_mut(index)
                            {
                                call.arguments = parsed;
                            }
                        }
                    }
                }
                Some(AssistantMessageEvent::ToolCallDelta {
                    content_index: index,
                    delta,
                    partial: self.partial(),
                })
            }
            "toolcall_end" => {
                let index = self.content_index(&chunk);
                self.apply_partial(partial);
                let id = chunk_string(&chunk, "id");
                let name = chunk_string(&chunk, "name");
                let arguments = chunk
                    .get("arguments")
                    .cloned()
                    .filter(serde_json::Value::is_object)
                    .or_else(|| match self.output.content.get(index) {
                        Some(Content::ToolCall(call)) => Some(call.arguments.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
                self.ensure_tool_call_block(index, id.clone(), name.clone());
                let call = match self.output.content.get_mut(index) {
                    Some(Content::ToolCall(call)) => {
                        if !id.is_empty() {
                            call.id = id.clone();
                        }
                        if !name.is_empty() {
                            call.name = name.clone();
                        }
                        call.arguments = arguments.clone();
                        call.clone()
                    }
                    _ => ToolCall {
                        kind: ToolCallType,
                        id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                        thought_signature: None,
                        namespace: None,
                    },
                };
                self.tool_json.remove(&index);
                Some(AssistantMessageEvent::ToolCallEnd {
                    content_index: index,
                    tool_call: call,
                    partial: self.partial(),
                })
            }
            other => {
                tracing::debug!(
                    chunk_type = other,
                    "ignoring an unknown plugin provider chunk"
                );
                None
            }
        }
    }

    fn content_index(&self, chunk: &serde_json::Value) -> usize {
        chunk
            .get("contentIndex")
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(0)
    }

    /// Append a freshly-opened block at `index`, padding any gap with empty text
    /// blocks so the index the plugin named is the index the message has. A
    /// plugin that names indices out of order is a plugin bug, but it must not
    /// corrupt the message the host persists.
    fn open_block(&mut self, index: usize, content: Content) {
        while self.output.content.len() <= index {
            self.output.content.push(Content::text(String::new()));
        }
        self.output.content[index] = content;
    }

    fn ensure_text_block(&mut self, index: usize) -> &mut crate::types::TextContent {
        self.ensure_block(index, || Content::text(String::new()));
        match self.output.content.get_mut(index) {
            Some(Content::Text(block)) => block,
            other => unreachable!("ensure_text_block left a non-text block: {other:?}"),
        }
    }

    fn ensure_thinking_block(&mut self, index: usize) -> &mut crate::types::ThinkingContent {
        self.ensure_block(index, || Content::thinking(String::new()));
        match self.output.content.get_mut(index) {
            Some(Content::Thinking(block)) => block,
            other => unreachable!("ensure_thinking_block left a non-thinking block: {other:?}"),
        }
    }

    fn ensure_tool_call_block(&mut self, index: usize, id: String, name: String) {
        self.ensure_block(index, || {
            Content::tool_call(
                id.clone(),
                name.clone(),
                serde_json::Value::Object(serde_json::Map::new()),
            )
        });
    }

    fn ensure_block(&mut self, index: usize, make: impl FnOnce() -> Content) {
        if self.output.content.len() <= index {
            // No `*_start` opened this block: the plugin skipped it. Repair the
            // message (the deltas still land) but say so — a missing start is a
            // contract violation the plugin author should see in the logs rather
            // than a silently different message shape.
            tracing::warn!(
                content_index = index,
                "plugin provider sent a block delta without its start chunk — synthesizing the block"
            );
            while self.output.content.len() < index {
                self.output.content.push(Content::text(String::new()));
            }
            self.output.content.push(make());
        }
    }

    /// Events that must precede the terminal event for `message`: whatever the
    /// mapper has not yet delivered (at minimum a `Start`, so a plugin that
    /// emitted nothing still produces a well-formed stream).
    /// `message` is the plugin's final payload on completion, or the accumulated
    /// partial with an Aborted stop reason when the caller cancels.
    pub fn chunks_then_message(
        &mut self,
        message: &AssistantMessage,
    ) -> Vec<AssistantMessageEvent> {
        let mut events = Vec::new();
        if let Some(start) = self.ensure_started() {
            events.push(start);
        }
        self.terminal = true;
        self.final_message = Some(message.clone());
        events
    }

    /// The accumulated message, for tests and diagnostics.
    pub fn accumulated(&self) -> &AssistantMessage {
        &self.output
    }
}

/// Read a string field from a chunk, defaulting to empty.
fn chunk_string(chunk: &serde_json::Value, field: &str) -> String {
    chunk
        .get(field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}
