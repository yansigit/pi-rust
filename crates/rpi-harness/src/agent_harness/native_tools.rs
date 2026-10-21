//! Provider/extension tool calls use the same validation and hooks as loop calls.
use super::*;
use rpi_agent::{ToolCallOptions, ToolInvocationResult};
use rpi_ai::{AssistantMessage, ToolCall};
use serde_json::{json, Value};

impl AgentHarness {
    /// Commit presentation state and executable selection together; state never grants tool access.
    pub fn select_native_tools(&self, names: Vec<String>, state: Value) -> HarnessResult<Value> {
        if serde_json::to_vec(&state)
            .map_err(|e| HarnessError::io(e.to_string()))?
            .len()
            > 16 * 1024
        {
            return Err(HarnessError::io("tool selection state exceeds 16 KiB"));
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        let mut selected = Vec::new();
        for name in names {
            if !selected.contains(&name)
                && inner
                    .tools
                    .iter()
                    .any(|tool| tool.tool.schema().name == name)
            {
                selected.push(name);
            }
        }
        inner.select_tools(selected, state)?;
        Ok(json!({"tools":inner.active_tool_names,
            "selection":{"revision":inner.tool_selection_revision.to_string(),"state":inner.tool_selection_state}}))
    }

    /// Freeze the same projection rules used by this execution's provider requests.
    pub fn session_context_options(&self) -> HarnessResult<SessionContextBuildOptions> {
        let snapshot = self.snapshot_config()?;
        Ok(Self::build_opts(
            &snapshot.entry_projectors,
            &snapshot.entry_transforms,
        ))
    }

    pub fn inspect_tools(&self) -> HarnessResult<Vec<Value>> {
        let snapshot = self.snapshot_config()?;
        Ok(snapshot
            .tools
            .iter()
            .map(|tool| {
                let schema = tool.tool.schema();
                let mut info = serde_json::to_value(schema).expect("tool schema serializes");
                info["label"] = json!(tool.tool.label());
                info["exposure"] = json!("direct");
                if let Some(snippet) = tool.tool.prompt_snippet() {
                    info["promptSnippet"] = json!(snippet);
                }
                info["promptGuidelines"] = json!(tool.tool.prompt_guidelines());
                info["sourceInfo"] = tool.source_info.clone().unwrap_or_else(|| {
                    json!({"source":"inline","path":schema.name,"scope":"temporary","origin":"top-level"})
                });
                info["active"] = json!(
                    snapshot.active_tool_names.contains(&schema.name)
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
                &snapshot.tools,
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

#[cfg(test)]
mod tests {
    use super::*;
    struct GuidedTool {
        schema: rpi_ai::Tool,
        guidelines: Vec<String>,
    }
    #[async_trait::async_trait]
    impl rpi_agent::AgentTool for GuidedTool {
        fn schema(&self) -> &rpi_ai::Tool {
            &self.schema
        }
        fn label(&self) -> &str {
            "Guided"
        }
        fn prompt_snippet(&self) -> Option<&str> {
            Some("Ask once")
        }
        fn prompt_guidelines(&self) -> &[String] {
            &self.guidelines
        }
        async fn execute(
            &self,
            _: &str,
            _: Value,
            _: CancellationToken,
            _: Arc<dyn Fn(rpi_agent::ToolResultPartial) + Send + Sync>,
        ) -> Result<rpi_agent::AgentToolResult, rpi_agent::AgentError> {
            panic!("prompt inspection must not execute tools")
        }
    }
    #[test]
    fn tool_prompt_guidance_respects_activation_custom_prompts_and_wrapper_metadata() {
        let tool: Arc<dyn rpi_agent::AgentTool> = Arc::new(GuidedTool {
            schema: rpi_ai::Tool {
                name: "question".into(),
                description: "Ask".into(),
                parameters: rpi_ai::Schema::empty_object(),
                constrained_sampling: None,
            },
            guidelines: vec![
                " Wait for the answer ".into(),
                "Wait for the answer".into(),
                " ".into(),
            ],
        });
        let wrapped = crate::image_tool::wrap(tool);
        assert_eq!(wrapped.prompt_snippet(), Some("Ask once"));
        let tools = vec![HarnessTool::new(wrapped)];
        let resources = AgentHarnessResources {
            include_tool_guidance: true,
            ..Default::default()
        };
        let prompt =
            AgentHarness::compose_prompt(Some("Base"), &resources, &["question".into()], &tools);
        assert!(prompt.contains("- question: Ask once"));
        assert_eq!(prompt.matches("- Wait for the answer").count(), 1);
        assert_eq!(
            AgentHarness::compose_prompt(Some("Base"), &resources, &["other".into()], &tools),
            "Base"
        );
        assert_eq!(
            AgentHarness::compose_prompt(
                Some("Custom"),
                &Default::default(),
                &["question".into()],
                &tools
            ),
            "Custom"
        );
    }
}
