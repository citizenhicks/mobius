use serde::Deserialize;
use serde_json::Value;

use super::{
    ApprovalRequirement, HookIdentity, MAX_COMMAND_BYTES, MAX_TOOL_OUTPUT_BYTES, Tool, ToolContext,
    ToolExposure,
};
use crate::backend::model::ToolDefinition;
use crate::backend::sandbox::BackgroundCommandPoll;
use crate::{BoxFuture, Error, Result};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    bash: super::ToolSpec,
    manage_command: super::ToolSpec,
    initial_wait_ms: u64,
}
static DEFINITION: std::sync::LazyLock<Definition> = std::sync::LazyLock::new(|| {
    toml::from_str(include_str!("commands.toml")).expect("bundled commands tools must be valid")
});

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
}

pub(super) struct Bash;

impl Tool for Bash {
    fn definition(&self) -> ToolDefinition {
        DEFINITION.bash.tool.clone()
    }

    fn render(&self, event: &crate::protocol::EventMsg) -> Option<crate::protocol::FrontendBlock> {
        DEFINITION.bash.render(event)
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

    fn call<'a>(
        &'a self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let arguments: BashArgs = serde_json::from_value(arguments)?;
            validate_command(&arguments.command)?;
            let output = context
                .sandbox
                .run_command(
                    arguments.command,
                    &context.permissions,
                    std::time::Duration::from_millis(DEFINITION.initial_wait_ms),
                )
                .await?;
            Ok(background_output(output).into())
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManageCommandArgs {
    command_id: String,
    action: CommandAction,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum CommandAction {
    Poll,
    Stop,
}

pub(super) struct ManageCommand;

impl Tool for ManageCommand {
    fn definition(&self) -> ToolDefinition {
        DEFINITION.manage_command.tool.clone()
    }

    fn render(&self, event: &crate::protocol::EventMsg) -> Option<crate::protocol::FrontendBlock> {
        DEFINITION.manage_command.render(event)
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn call<'a>(
        &'a self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let arguments: ManageCommandArgs = serde_json::from_value(arguments)?;
            validate_command_id(&arguments.command_id)?;
            let output = match arguments.action {
                CommandAction::Poll => {
                    context
                        .sandbox
                        .poll_background(&arguments.command_id, &context.permissions)
                        .await?
                }
                CommandAction::Stop => {
                    context
                        .sandbox
                        .stop_background(&arguments.command_id, &context.permissions)
                        .await?
                }
            };
            Ok(background_output(output).into())
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

pub(super) fn background_output(output: BackgroundCommandPoll) -> String {
    let status = output.status.as_str();
    let exit_code = output.exit_code;
    let rendered = serde_json::json!({
        "command_id": output.command_id,
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
        "command_id": output.command_id,
        "status": status,
        "exit_code": exit_code,
        "stdout": "",
        "stderr": "",
        "truncated": true,
        "error": "background output exceeded its serialized limit"
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_arguments_reject_unknown_fields() {
        assert!(
            serde_json::from_value::<BashArgs>(
                serde_json::json!({"command": "true", "unexpected": true})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<ManageCommandArgs>(serde_json::json!({
                "command_id": uuid::Uuid::nil().to_string(),
                "action": "poll",
                "unexpected": true,
            }))
            .is_err()
        );
    }
}
