//! Declarative native tools execute through the ordinary host invocation path.
use rpi_agent::{AgentError, AgentTool, AgentToolResult, ToolExecutionMode, ToolResultPartial};
use rpi_ai::{Schema, Tool};
use rpi_harness::types::HarnessTool;
use rpi_tools::ExecutionToolContext;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Builtin {
    Read,
    Bash,
    Edit,
    Write,
    Grep,
    Find,
    Ls,
}
impl Builtin {
    fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Bash => "bash",
            Self::Edit => "edit",
            Self::Write => "write",
            Self::Grep => "grep",
            Self::Find => "find",
            Self::Ls => "ls",
        }
    }
}

pub(crate) fn builtin(ctx: &ExecutionToolContext, name: Builtin) -> Arc<dyn AgentTool> {
    use rpi_tools::tools::*;
    match name {
        Builtin::Read => create_read_tool(
            ctx,
            Some(read::ReadToolOptions {
                image_processor: Some(Arc::new(rpi_tools::image_processing::InlineImageProcessor)),
                ..Default::default()
            }),
        ),
        Builtin::Bash => create_bash_tool(ctx, Some(crate::session::bash_options())),
        Builtin::Edit => create_edit_tool(ctx),
        Builtin::Write => create_write_tool(ctx),
        Builtin::Grep => create_grep_tool(ctx, None),
        Builtin::Find => create_find_tool(ctx, None),
        Builtin::Ls => create_ls_tool(ctx, None),
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum Binding {
    Builtin {
        name: Builtin,
        #[serde(default, rename = "prepareArguments")]
        prepare_arguments: bool,
    },
    Recorded {
        error: String,
    },
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Declaration {
    name: String,
    description: String,
    parameters: Schema,
    label: String,
    #[serde(default)]
    execution_mode: ToolExecutionMode,
    prompt_snippet: Option<String>,
    #[serde(default)]
    prompt_guidelines: Vec<String>,
    recorded_call_prefix: Option<String>,
    recorded_error: Option<String>,
    binding: Binding,
}

pub(crate) fn owner_key(owner: &rpi_extensions::native::NativeOwner) -> String {
    json!([owner.generation, owner.id, owner.plugin]).to_string()
}

pub(crate) fn declarations(
    value: Value,
    owner: &rpi_extensions::native::NativeOwner,
    ctx: &ExecutionToolContext,
) -> Result<Vec<HarnessTool>, String> {
    if value.as_array().map_or(true, |items| items.len() > 128)
        || serde_json::to_vec(&value)
            .map_err(|_| "invalid native tools")?
            .len()
            > 1024 * 1024
    {
        return Err("native tool declarations exceed capacity".into());
    }
    let items: Vec<Declaration> =
        serde_json::from_value(value).map_err(|_| "invalid native tool declarations")?;
    let mut names = std::collections::HashSet::new();
    let message = |value: &str| !value.trim().is_empty() && value.len() <= 4096;
    items
        .into_iter()
        .map(|item| {
            if item.name.is_empty()
                || item.name.len() > 128
                || !item
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                || !names.insert(item.name.clone())
                || !item.parameters.as_value().is_object()
                || !message(&item.label)
                || match (&item.recorded_call_prefix, &item.recorded_error) {
                    (None, None) => false,
                    (Some(prefix), Some(error)) => {
                        prefix.is_empty() || prefix.len() > 256 || !message(error)
                    }
                    _ => true,
                }
                || matches!(&item.binding, Binding::Recorded { error } if !message(error))
                || matches!(&item.binding, Binding::Builtin { name, .. } if name.name() != item.name)
            {
                return Err("invalid native tool declaration".into());
            }
            let delegate = match item.binding {
                Binding::Builtin { name, .. } => Some(builtin(ctx, name)),
                Binding::Recorded { .. } => None,
            };
            let schema = Tool {
                name: item.name.clone(),
                description: item.description.clone(),
                parameters: item.parameters.clone(),
                constrained_sampling: None,
            };
            Ok(HarnessTool::new(Arc::new(BoundTool {
                schema,
                item,
                delegate,
            }))
            .with_source_info(json!({"source":"extension","path":owner.plugin,
                "scope":"temporary","origin":"top-level"})))
        })
        .collect()
}

struct BoundTool {
    schema: Tool,
    item: Declaration,
    delegate: Option<Arc<dyn AgentTool>>,
}
#[async_trait::async_trait]
impl AgentTool for BoundTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        &self.item.label
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        self.item.execution_mode
    }
    fn prompt_snippet(&self) -> Option<&str> {
        self.item.prompt_snippet.as_deref()
    }
    fn prompt_guidelines(&self) -> &[String] {
        &self.item.prompt_guidelines
    }
    fn prepare_arguments(&self, args: Value) -> Result<Value, AgentError> {
        if matches!(
            self.item.binding,
            Binding::Builtin {
                prepare_arguments: true,
                ..
            }
        ) {
            return self
                .delegate
                .as_ref()
                .expect("validated builtin binding")
                .prepare_arguments(args);
        }
        Ok(args)
    }
    async fn execute(
        &self,
        id: &str,
        params: Value,
        signal: CancellationToken,
        update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        if self
            .item
            .recorded_call_prefix
            .as_ref()
            .is_some_and(|prefix| id.starts_with(prefix))
        {
            return Err(AgentError::Tool(
                self.item
                    .recorded_error
                    .clone()
                    .expect("validated recording guard"),
            ));
        }
        if let Binding::Recorded { error } = &self.item.binding {
            return Err(AgentError::Tool(error.clone()));
        }
        let tool = self.delegate.as_ref().expect("validated builtin binding");
        // Older extension schemas validate legacy fields before builtin adaptation.
        let params = if matches!(
            self.item.binding,
            Binding::Builtin {
                prepare_arguments: true,
                ..
            }
        ) {
            params
        } else {
            tool.prepare_arguments(params)?
        };
        tool.execute(id, params, signal, update).await
    }
}

#[cfg(test)]
mod tests;
