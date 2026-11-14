use serde::Deserialize;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{
    ApprovalRequirement, HookIdentity, MAX_COMMAND_BYTES, Tool, ToolContext, ToolExposure,
};
use crate::backend::model::ToolDefinition;
use crate::backend::sandbox::{BackgroundCommandPoll, CommandOutputSink};
use crate::middleware::FrontendEventSink;
use crate::protocol::{
    EventMsg, FrontendBlock, FrontendEvent, FrontendPriority, FrontendSlot, FrontendTone,
    FrontendWidget, ToolContent,
};
use crate::{BoxFuture, Error, Result};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    bash: super::ToolSpec,
    manage_command: super::ToolSpec,
    default_timeout_ms: u64,
    max_timeout_ms: u64,
}
crate::embedded_config! { static DEFINITION: Definition = include_str!("commands.toml") => fill_limits; }

fn fill_limits(definition: &mut Definition) {
    super::fill_max_bytes(
        &mut definition.bash.tool,
        "/properties/command/description",
        MAX_COMMAND_BYTES,
    );
}

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
                    let preview = command_preview(command);
                    // The title already shows a short command; the body repeats only a longer one.
                    if preview == command {
                        block.text.clear();
                    }
                    block.title = format!("{} · {preview}", block.title);
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
            let live = context
                .frontend()
                .filter(|_| !arguments.run_in_background)
                .map(|frontend| LiveOutput::new(frontend, &context.call_id));
            let observer = live
                .as_ref()
                .map_or_else(CommandOutputSink::default, LiveOutput::sink);
            let output = context
                .sandbox
                .run_command(
                    arguments.command,
                    context.permissions.mutation()?,
                    wait,
                    observer,
                )
                .await;
            if let Some(live) = live {
                live.finish();
            }
            Ok(background_output(&output?, context.sandbox.output_limit()).into())
        })
    }
}

/// Newest output lines a running command shows, and how often they refresh.
const LIVE_LINES: usize = 5;
const LIVE_BYTES: usize = 600;
const LIVE_INTERVAL: Duration = Duration::from_millis(250);

/// A running command's newest output lines, published as a transient transcript-tail widget and
/// removed when the call returns; the call's result row stays the durable record.
struct LiveOutput(Arc<LiveTail>);

struct LiveTail {
    frontend: FrontendEventSink,
    id: Arc<str>,
    state: Mutex<LiveState>,
}

#[derive(Default)]
struct LiveState {
    tail: String,
    published: Option<Instant>,
}

impl LiveOutput {
    fn new(frontend: &FrontendEventSink, call_id: &str) -> Self {
        Self(Arc::new(LiveTail {
            frontend: Arc::clone(frontend),
            id: format!("live-output:{call_id}").into(),
            state: Mutex::default(),
        }))
    }

    fn sink(&self) -> CommandOutputSink {
        let tail = Arc::clone(&self.0);
        CommandOutputSink::new(move |_, bytes| tail.push(bytes))
    }

    fn finish(self) {
        let published = self
            .0
            .state
            .lock()
            .is_ok_and(|state| state.published.is_some());
        if published {
            self.0.publish(FrontendEvent::RemoveWidget {
                capability: super::MANIFEST.id.into(),
                id: self.0.id.to_string(),
            });
        }
    }
}

impl LiveTail {
    fn push(&self, bytes: &[u8]) {
        let text = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            state.tail.push_str(&String::from_utf8_lossy(bytes));
            state.tail = live_tail(&state.tail);
            if state
                .published
                .is_some_and(|at| at.elapsed() < LIVE_INTERVAL)
                || state.tail.trim().is_empty()
            {
                return;
            }
            state.published = Some(Instant::now());
            state.tail.trim_end().to_owned()
        };
        self.publish(FrontendEvent::Widget {
            capability: super::MANIFEST.id.into(),
            item: Arc::new(FrontendWidget {
                id: Arc::clone(&self.id),
                slot: FrontendSlot::TranscriptTail,
                text,
                tone: FrontendTone::Neutral,
                symbol: None,
                icon_only: false,
                progress: None,
                content: None,
                action: None,
                count: None,
                priority: FrontendPriority::default(),
            }),
        });
    }

    fn publish(&self, event: FrontendEvent) {
        if let Err(error) = (self.frontend)(event) {
            tracing::debug!(%error, "live command output was not published");
        }
    }
}

/// `line` without control characters or terminal escape sequences such as colours.
fn printable(line: &str) -> String {
    let mut text = String::with_capacity(line.len());
    let mut escape = false;
    for character in line.chars() {
        if character == '\x1b' {
            escape = true;
        } else if escape {
            // A sequence ends at its final letter; `[` and parameters come before it.
            escape = !character.is_ascii_alphabetic();
        } else if !character.is_control() || character == '\t' {
            text.push(character);
        }
    }
    text
}

/// The last lines of `output` as a terminal would show them: a carriage return redraws its
/// line, control characters drop out, and the tail stays within its line and byte bounds.
fn live_tail(output: &str) -> String {
    let lines: Vec<String> = output
        .split('\n')
        .map(|line| printable(line.rsplit('\r').next().unwrap_or_default()))
        .collect();
    // One more than the kept lines: the text after the last newline is the line still being written.
    let skipped = lines.len().saturating_sub(LIVE_LINES + 1);
    let mut tail = lines
        .into_iter()
        .skip(skipped)
        .collect::<Vec<_>>()
        .join("\n");
    if tail.len() > LIVE_BYTES {
        let start = tail.ceil_char_boundary(tail.len() - LIVE_BYTES);
        tail.drain(..start);
    }
    tail
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

    fn polls(&self) -> bool {
        true
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
/// result envelope's background command ID groups this row with every row about it. The body
/// shows the command's own output rather than the envelope the model reads.
fn end_command_block(block: &mut FrontendBlock, output: &ToolContent) {
    #[derive(Deserialize)]
    struct Envelope {
        command_id: Option<String>,
        status: String,
        exit_code: Option<i32>,
        #[serde(default)]
        stdout: String,
        #[serde(default)]
        stderr: String,
        error: Option<String>,
    }
    block.title.clear();
    let Ok(envelope) = serde_json::from_str::<Envelope>(&output.text()) else {
        return;
    };
    let mut lines = Vec::new();
    for stream in [&envelope.stdout, &envelope.stderr] {
        let stream = stream.trim_end();
        if !stream.is_empty() {
            lines.push(stream);
        }
    }
    let status = match (envelope.status.as_str(), envelope.exit_code) {
        ("running", _) => Some("still running in the background".to_owned()),
        (_, Some(0)) => None,
        (_, Some(code)) => Some(format!("exit {code}")),
        (status, None) => Some(status.to_owned()),
    };
    if lines.is_empty() && status.is_none() && envelope.error.is_none() {
        lines.push("(no output)");
    }
    let mut text = lines.join("\n");
    for note in status.iter().chain(envelope.error.as_ref()) {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(note);
    }
    block.text = super::compact_output(&text);
    block.group = envelope.command_id;
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
    fn live_tail_keeps_the_newest_lines_as_a_terminal_shows_them() {
        let output = "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\n\
                      progress 10%\rprogress 90%\x1b[0m";
        assert_eq!(
            live_tail(output),
            "line 5\nline 6\nline 7\nline 8\nline 9\nprogress 90%"
        );
        assert!(live_tail(&"x".repeat(5_000)).len() <= LIVE_BYTES);
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
        let short = Bash
            .render(&begin("bash", serde_json::json!({ "command": "ls" })))
            .expect("short bash begin");
        assert_eq!(
            (short.title.as_str(), short.text.as_str()),
            ("Bash · ls", "")
        );
        let bash = Bash.render(&end("bash")).expect("bash end");
        assert_eq!((bash.title.as_str(), bash.group.as_deref()), ("", Some(id)));
        assert_eq!(bash.text, "still running in the background");

        let finished = |output: serde_json::Value| {
            Bash.render(&EventMsg::ToolCallEnd(ToolCallEndEvent {
                turn_id: "turn".into(),
                call_id: "call".into(),
                name: "bash".into(),
                output: output.to_string().into(),
                is_error: false,
            }))
            .expect("bash end")
            .text
        };
        assert_eq!(
            finished(
                serde_json::json!({"command_id": null, "status": "exited", "exit_code": 0,
                "stdout": "tick 1\ntick 2\n", "stderr": "", "truncated": false, "error": null})
            ),
            "tick 1\ntick 2"
        );
        assert_eq!(
            finished(
                serde_json::json!({"command_id": null, "status": "exited", "exit_code": 2,
                "stdout": "", "stderr": "boom\n", "truncated": false, "error": null})
            ),
            "boom\nexit 2"
        );
        assert_eq!(
            finished(
                serde_json::json!({"command_id": null, "status": "exited", "exit_code": 0,
                "stdout": "", "stderr": "", "truncated": false, "error": null})
            ),
            "(no output)"
        );

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
