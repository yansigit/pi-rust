//! Provider/extension tool calls use the same validation and hooks as loop calls.
use super::*;
use rpi_agent::{ToolCallOptions, ToolInvocationResult};
use rpi_ai::{AssistantMessage, ToolCall};
use serde_json::{json, Value};

impl AgentHarness {
    pub fn inspect_tools(&self) -> HarnessResult<Vec<Value>> {
        let snapshot = self.snapshot_config()?;
        Ok(snapshot
            .tools
            .iter()
            .map(|tool| {
                let schema = tool.tool.schema();
                let mut info = serde_json::to_value(schema).expect("tool schema serializes");
                info["label"] = json!(tool.tool.label());
                info["active"] = json!(
                    snapshot.active_tool_names.is_empty()
                        || snapshot.active_tool_names.contains(&schema.name)
                );
                info["executionMode"] = json!(match tool.tool.execution_mode() {
                    rpi_agent::ToolExecutionMode::Sequential => "sequential",
                    rpi_agent::ToolExecutionMode::Parallel => "parallel",
                });
                info
            })
            .collect())
    }

    pub async fn invoke_tool(
        &self,
        call: ToolCall,
        signal: CancellationToken,
    ) -> HarnessResult<ToolInvocationResult> {
        let snapshot = self.snapshot_config()?;
        if let Some(identity) = rpi_ai::identity::current() {
            if identity
                .session_id
                .as_deref()
                .is_some_and(|id| id != self.session.storage().metadata().id)
            {
                return Err(HarnessError::io(
                    "tool invocation belongs to a different session",
                ));
            }
        }
        let context = build_session_context(
            &self.branch_path_oldest_first().await?,
            &Self::build_opts(&snapshot.entry_projectors, &snapshot.entry_transforms),
        );
        let context = AgentContext {
            system_prompt: Self::compose_prompt(
                snapshot.system_prompt.as_deref(),
                &snapshot.resources,
                &snapshot.active_tool_names,
            ),
            messages: context.messages,
            tools: Self::active_tools(&snapshot.tools, &snapshot.active_tool_names),
        };
        let mut assistant = AssistantMessage::empty(
            snapshot.model.api,
            snapshot.model.provider,
            snapshot.model.id,
            0,
        );
        assistant
            .content
            .push(rpi_ai::Content::ToolCall(call.clone()));
        assistant.stop_reason = rpi_ai::StopReason::ToolUse;
        let signal = signal.child_token();
        let _cancel = signal.clone().drop_guard();
        let options = ToolCallOptions {
            before_tool_call: snapshot.before_tool_call,
            after_tool_call: snapshot.after_tool_call,
            signal: signal.clone(),
        };
        let emitter = snapshot
            .agent_emitter
            .unwrap_or_else(|| Arc::new(rpi_agent::CollectorEmitter::default()));
        let parent = snapshot.parent_signal.unwrap_or_default();
        let lifetime = snapshot.child_scope;
        tokio::select! {
            biased;
            _ = signal.cancelled() => Err(HarnessError::io("tool invocation cancelled")),
            _ = parent.cancelled() => Err(HarnessError::io("tool invocation cancelled with its operation")),
            _ = lifetime.cancelled() => Err(HarnessError::io("tool invocation target is no longer active")),
            result = rpi_agent::invoke_tool(&context, &assistant, &call, &options, &emitter) => result.map_err(|error| HarnessError::io(error.to_string())),
        }
    }
}
