use serde::Deserialize;
use serde_json::Value;

use super::{
    ApprovalRequirement, HookIdentity, MAX_COMMAND_BYTES, MAX_TOOL_OUTPUT_BYTES, Tool, ToolContext,
    ToolExposure, text,
};
use crate::backend::model::ToolDefinition;
use crate::backend::sandbox::BackgroundCommandPoll;
use crate::{BoxFuture, Error, Result};

#[derive(Deserialize)]
struct BashArgs {
    command: String,
}

pub(super) struct Bash;

impl Tool for Bash {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".into(),
            description: text::TOOL_BASH_DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Always
    }

    fn hook_identity(&self) -> Option<HookIdentity> {
        Some(HookIdentity {
            name: "Bash",
            subjects: &["Bash"],
        })
    }

    fn rewrite_hook_input(&self, input: Value) -> Result<Value> {
        rewrite_command_input(&input)
    }

    fn call<'a>(&'a self, context: ToolContext, arguments: Value) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let arguments: BashArgs = serde_json::from_value(arguments)?;
            validate_command(&arguments.command)?;
            let output = context
                .sandbox
                .execute(&arguments.command, &context.permissions)
                .await?;
            Ok(format!(
                "exit code: {}\nstdout:\n{}\nstderr:\n{}",
                output.exit_code, output.stdout, output.stderr
            ))
        })
    }
}

pub(super) struct StartCommand;

impl Tool for StartCommand {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "start_command".into(),
            description: text::TOOL_START_COMMAND_DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Always
    }

    fn hook_identity(&self) -> Option<HookIdentity> {
        Some(HookIdentity {
            name: "Bash",
            subjects: &["Bash"],
        })
    }

    fn rewrite_hook_input(&self, input: Value) -> Result<Value> {
        rewrite_command_input(&input)
    }

    fn call<'a>(&'a self, context: ToolContext, arguments: Value) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let arguments: BashArgs = serde_json::from_value(arguments)?;
            validate_command(&arguments.command)?;
            let id = context
                .sandbox
                .start_background(arguments.command, &context.permissions)?;
            Ok(serde_json::json!({"command_id": id, "status": "running"}).to_string())
        })
    }
}

#[derive(Deserialize)]
struct CommandIdArgs {
    command_id: String,
}

pub(super) struct PollCommand;

impl Tool for PollCommand {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "poll_command".into(),
            description: text::TOOL_POLL_COMMAND_DESCRIPTION.into(),
            parameters: command_id_schema(),
        }
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn call<'a>(&'a self, context: ToolContext, arguments: Value) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let arguments: CommandIdArgs = serde_json::from_value(arguments)?;
            validate_command_id(&arguments.command_id)?;
            let output = context
                .sandbox
                .poll_background(&arguments.command_id, &context.permissions)
                .await?;
            Ok(background_output(output))
        })
    }
}

pub(super) struct StopCommand;

impl Tool for StopCommand {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "stop_command".into(),
            description: text::TOOL_STOP_COMMAND_DESCRIPTION.into(),
            parameters: command_id_schema(),
        }
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn call<'a>(&'a self, context: ToolContext, arguments: Value) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let arguments: CommandIdArgs = serde_json::from_value(arguments)?;
            validate_command_id(&arguments.command_id)?;
            let output = context
                .sandbox
                .stop_background(&arguments.command_id, &context.permissions)
                .await?;
            Ok(background_output(output))
        })
    }
}

fn validate_command(command: &str) -> Result<()> {
    if command.trim().is_empty() {
        return Err(Error::Tool("command cannot be empty".into()));
    }
    if command.len() > MAX_COMMAND_BYTES {
        return Err(Error::Tool(format!(
            "command exceeds {MAX_COMMAND_BYTES} bytes"
        )));
    }
    Ok(())
}

fn rewrite_command_input(input: &Value) -> Result<Value> {
    let command = input
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Config("hook tool rewrite requires `command`".into()))?;
    Ok(serde_json::json!({"command": command}))
}

fn validate_command_id(id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|_| Error::Tool("command_id must be a UUID".into()))
}

fn command_id_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {"command_id": {"type": "string", "format": "uuid"}},
        "required": ["command_id"],
        "additionalProperties": false
    })
}

pub(super) fn background_output(output: BackgroundCommandPoll) -> String {
    let status = output.status.as_str();
    let exit_code = output.exit_code;
    let rendered = serde_json::json!({
        "status": status,
        "exit_code": exit_code,
        "stdout": output.stdout,
        "stderr": output.stderr,
        "truncated": output.truncated,
        "error": output.error
    })
    .to_string();
    if rendered.len() <= MAX_TOOL_OUTPUT_BYTES {
        return rendered;
    }
    serde_json::json!({
        "status": status,
        "exit_code": exit_code,
        "stdout": "",
        "stderr": "",
        "truncated": true,
        "error": "background output exceeded its serialized limit"
    })
    .to_string()
}
