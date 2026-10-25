use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

use super::{
    ApprovalRequirement, HookIdentity, MAX_COMMAND_BYTES, Tool, ToolContext, ToolExposure,
};
use crate::backend::model::ToolDefinition;
use crate::backend::sandbox::BackgroundCommandPoll;
use crate::protocol::{EventMsg, FrontendBlock, ToolContent};
use crate::{BoxFuture, Error, Result};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    bash: super::ToolSpec,
    manage_command: super::ToolSpec,
    default_timeout_ms: u64,
    max_timeout_ms: u64,
}
crate::embedded_config! { static DEFINITION: Definition = include_str!("commands.toml"); }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
    timeout_ms: Option<u64>,
    #[serde(default)]
    run_in_background: bool,
}

impl BashArgs {
    fn wait(&self) -> Result<std::time::Duration> {
        if self.timeout_ms == Some(0) {
            return Err(Error::Tool("timeout_ms must be at least 1".into()));
        }
        if self.run_in_background {
            return Ok(std::time::Duration::ZERO);
        }
        Ok(std::time::Duration::from_millis(
            self.timeout_ms
                .map_or(DEFINITION.default_timeout_ms, |timeout| {
                    timeout.min(DEFINITION.max_timeout_ms)
                }),
        ))
    }
}

pub(super) struct Bash;

impl Tool for Bash {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &DEFINITION.bash.tool
    }

    fn render(&self, event: &EventMsg) -> Option<FrontendBlock> {
        let mut block = DEFINITION.bash.render(event)?;
        match event {
            EventMsg::ToolCallBegin(call) => {
                if let Some(command) = call
                    .arguments
                    .get("command")
                    .and_then(Value::as_str)
                    .filter(|command| !command.is_empty())
                {
                    block.title = format!("{} · {}", block.title, command_preview(command));
                }
            }
            EventMsg::ToolCallEnd(result) => end_command_block(&mut block, &result.output),
            _ => {}
        }
        Some(block)
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
        rewrite_command_input(input)
    }

    fn call(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let arguments: BashArgs = serde_json::from_value(arguments)?;
            validate_command(&arguments.command)?;
            let wait = arguments.wait()?;
            let output = context
                .sandbox
                .run_command(arguments.command, context.permissions.mutation()?, wait)
                .await?;
            Ok(background_output(&output, context.sandbox.output_limit()).into())
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
    fn definition(&self) -> &Arc<ToolDefinition> {
        &DEFINITION.manage_command.tool
    }

    fn render(&self, event: &EventMsg) -> Option<FrontendBlock> {
        let mut block = DEFINITION.manage_command.render(event)?;
        match event {
            EventMsg::ToolCallBegin(call) => {
                let command_id = call
                    .arguments
                    .get("command_id")
                    .and_then(Value::as_str)
                    .and_then(|id| uuid::Uuid::parse_str(id).ok());
                if let Some(action) = call.arguments.get("action").and_then(Value::as_str) {
                    block.title = match command_id {
                        Some(id) => {
                            format!("{} · {action} · #{:08x}", block.title, id.as_fields().0)
                        }
                        None => format!("{} · {action}", block.title),
                    };
                }
                block.group = command_id.map(|id| id.to_string());
            }
            EventMsg::ToolCallEnd(result) => end_command_block(&mut block, &result.output),
            _ => {}
        }
        Some(block)
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn call(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
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
            Ok(background_output(&output, context.sandbox.output_limit()).into())
        })
    }
}

/// One line of at most 80 characters; the full command stays in the block text.
fn command_preview(command: &str) -> String {
    let mut characters = command.chars();
    let mut preview: String = characters
        .by_ref()
        .take(80)
        .map(|character| match character {
            '\r' | '\n' | '\t' => ' ',
            other => other,
        })
        .collect();
    if characters.next().is_some() {
        preview.push('…');
    }
    preview
}

/// An empty Append title keeps the heading the begin block gave its command, and the
/// result envelope's background command ID groups this row with every row about it.
fn end_command_block(block: &mut FrontendBlock, output: &ToolContent) {
    #[derive(Deserialize)]
    struct Envelope {
        command_id: Option<String>,
    }
    block.title.clear();
    block.group = serde_json::from_str::<Envelope>(&output.text())
        .ok()
        .and_then(|envelope| envelope.command_id);
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

fn rewrite_command_input(mut input: Value) -> Result<Value> {
    hook_command(&input)?;
    if let Some(arguments) = input.as_object_mut() {
        arguments.retain(|key, _| {
            matches!(key.as_str(), "command" | "timeout_ms" | "run_in_background")
        });
    }
    Ok(input)
}

/// The rewritten `command` a Bash-shaped hook returns for any tool it governs.
pub(super) fn hook_command(input: &Value) -> Result<&str> {
    input
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Config("hook tool rewrite requires `command`".into()))
}

fn validate_command_id(id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|error| Error::Tool(format!("command_id must be a UUID: {error}")))
}

pub(super) fn background_output(output: &BackgroundCommandPoll, output_limit: usize) -> String {
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
    if rendered.len() <= output_limit {
        return rendered;
    }
    // Keep each stream's head and tail; JSON escaping can still inflate, so shrink until it fits.
    let mut budget = output_limit / 4;
    while budget > 0 {
        let rendered = serde_json::json!({
            "command_id": output.command_id,
            "status": status,
            "exit_code": exit_code,
            "stdout": super::capped(&output.stdout, budget),
            "stderr": super::capped(&output.stderr, budget),
            "truncated": true,
            "error": output.error
        })
        .to_string();
        if rendered.len() <= output_limit {
            return rendered;
        }
        budget /= 2;
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
    fn bash_wait_bounds_and_hook_options_are_preserved() {
        for (input, expected) in [
            (serde_json::json!({"command": "true"}), 120_000),
            (serde_json::json!({"command": "true", "timeout_ms": 1}), 1),
            (
                serde_json::json!({"command": "true", "timeout_ms": 900_000}),
                600_000,
            ),
            (
                serde_json::json!({"command": "true", "timeout_ms": 1, "run_in_background": true}),
                0,
            ),
        ] {
            let rewritten = Bash.rewrite_hook_input(input.clone()).expect("hook input");
            assert_eq!(rewritten, input);
            let arguments: BashArgs = serde_json::from_value(rewritten).expect("arguments");
            assert_eq!(arguments.wait().expect("wait").as_millis(), expected);
        }
        for run_in_background in [false, true] {
            let arguments: BashArgs = serde_json::from_value(serde_json::json!({
                "command": "true", "timeout_ms": 0, "run_in_background": run_in_background,
            }))
            .expect("arguments");
            assert!(arguments.wait().is_err());
        }
    }

    #[test]
    fn command_rows_carry_their_preview_and_join_by_command_id() {
        use crate::protocol::{ToolCallBeginEvent, ToolCallEndEvent};
        let id = "1a2b3c4d-0000-4000-8000-000000000000";
        let begin = |name: &str, arguments| {
            EventMsg::ToolCallBegin(ToolCallBeginEvent {
                turn_id: "turn".into(),
                call_id: "call".into(),
                name: name.into(),
                arguments,
            })
        };
        let end = |name: &str| {
            EventMsg::ToolCallEnd(ToolCallEndEvent {
                turn_id: "turn".into(),
                call_id: "call".into(),
                name: name.into(),
                output: format!(r#"{{"command_id":"{id}","status":"running"}}"#).into(),
                is_error: false,
            })
        };

        let command = format!("cargo test\n{}", "x".repeat(80));
        let bash = Bash
            .render(&begin("bash", serde_json::json!({ "command": command })))
            .expect("bash begin");
        assert_eq!(bash.title, format!("Bash · cargo test {}…", "x".repeat(69)));
        assert_eq!(bash.text, command);
        let bash = Bash.render(&end("bash")).expect("bash end");
        assert_eq!((bash.title.as_str(), bash.group.as_deref()), ("", Some(id)));

        let manage = ManageCommand
            .render(&begin(
                "manage_command",
                serde_json::json!({ "command_id": id, "action": "poll" }),
            ))
            .expect("manage begin");
        assert_eq!(manage.title, "Command · poll · #1a2b3c4d");
        assert_eq!(manage.group.as_deref(), Some(id));
        let manage = ManageCommand
            .render(&end("manage_command"))
            .expect("manage end");
        assert_eq!(manage.group.as_deref(), Some(id));
    }
}
