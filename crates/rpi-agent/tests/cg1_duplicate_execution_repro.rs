//! Reproduction for the duplicate execution of tool calls that a provider's
//! remote agent already ran (issue #110, gap CG1).
//!
//! Call sequence: a provider package whose remote agent runs tools itself (the
//! Cursor package) runs `shell`, then streams the call back with its recorded
//! result so the transcript is complete. The host has no way to tell that call
//! apart from a fresh one, so the local loop runs the command a second time.

#[path = "common/mod.rs"]
mod common;

use std::sync::{Arc, Mutex};

use common::{base_config, mock_stream_fn, run_and_collect, user_message};
use rpi_agent::{AgentContext, AgentError, AgentTool, AgentToolResult, ToolResultPartial};
use rpi_ai::types::{Api, AssistantMessage, Content, StopReason, Tool, Usage};
use tokio_util::sync::CancellationToken;

/// A `shell`-like tool whose `execute` is observable: every real run bumps
/// `executions`.
struct ShellTool {
    schema: Tool,
    executions: Arc<Mutex<usize>>,
}

impl ShellTool {
    fn new() -> (Self, Arc<Mutex<usize>>) {
        let executions = Arc::new(Mutex::new(0));
        let schema = Tool {
            name: "shell".to_string(),
            description: "Run a shell command".to_string(),
            parameters: rpi_ai::types::Schema::new(serde_json::json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"],
                "additionalProperties": false
            })),
            constrained_sampling: None,
        };
        (
            ShellTool {
                schema,
                executions: Arc::clone(&executions),
            },
            executions,
        )
    }
}

#[async_trait::async_trait]
impl AgentTool for ShellTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "Shell"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        *self.executions.lock().expect("executions lock") += 1;
        Ok(AgentToolResult::text("executed locally"))
    }
}

/// A `toolCall` content block as a provider stream delivers it. It is built
/// from JSON so the test compiles against the wire shape, not a Rust literal.
fn streamed_call(id: &str, command: &str, recorded: Option<serde_json::Value>) -> Content {
    let mut block = serde_json::json!({
        "type": "toolCall",
        "id": id,
        "name": "shell",
        "arguments": { "command": command },
    });
    if let Some(recorded) = recorded {
        block["recorded"] = recorded;
    }
    serde_json::from_value(block).expect("streamed tool call decodes")
}

/// A recording in the shape the Cursor replay worker emits (`replay.ts`):
/// `{"version":1,"isError":bool,"result":{"content":[...],"details":{},"terminate"?:bool}}`.
fn worker_recording(text: &str, is_error: bool, terminate: Option<bool>) -> serde_json::Value {
    let mut result = serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "details": {}
    });
    if let Some(terminate) = terminate {
        result["terminate"] = serde_json::json!(terminate);
    }
    serde_json::json!({ "version": 1, "isError": is_error, "result": result })
}

fn assistant_with(content: Vec<Content>) -> AssistantMessage {
    AssistantMessage {
        role: rpi_ai::types::AssistantRole,
        content,
        api: Api::Other("openai-responses".into()),
        provider: "mock".to_string(),
        model: "mock".to_string(),
        response_model: None,
        response_id: None,
        usage: Usage::zero(),
        stop_reason: StopReason::ToolUse,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    }
}

#[tokio::test]
async fn a_call_the_remote_agent_already_ran_is_executed_again_locally() {
    let (tool, executions) = ShellTool::new();
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![Arc::new(tool)],
    };
    let replayed = streamed_call(
        "c1",
        "git push --force",
        Some(worker_recording("pushed", false, None)),
    );
    let script = vec![
        assistant_with(vec![replayed]),
        common::assistant_text("done", StopReason::Stop),
    ];
    let _ = run_and_collect(
        vec![user_message("push it")],
        context,
        base_config(),
        mock_stream_fn(script),
    )
    .await;
    assert_eq!(
        *executions.lock().expect("executions lock"),
        0,
        "the remote agent already ran this call; running it locally runs it twice"
    );
}
