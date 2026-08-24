//! Tool registry, dispatch, and minimal filesystem tools.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use diffy::Patch;
use futures_util::FutureExt;
use futures_util::future::join_all;
use serde_json::Value;

use super::manifest::MiddlewareManifest;
use super::{Middleware, PromptSection};
use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::backend::model::ToolCall;
use crate::backend::model::ToolDefinition;
#[cfg(test)]
use crate::backend::sandbox::BackgroundCommandPoll;
use crate::backend::sandbox::Sandbox;
use crate::backend::sandbox::SandboxPermissions;
use crate::backend::sandbox::ToolPermissions;
use crate::preview_json;
use crate::protocol::EventMsg;
use crate::protocol::FrontendBlock;
use crate::protocol::FrontendBlockFormat;
use crate::protocol::FrontendBlockRole;
use crate::protocol::FrontendBlockState;
use crate::protocol::FrontendBlockUpdate;
use crate::protocol::FrontendContribution;
use crate::protocol::FrontendTone;

mod text {
    include!(concat!(env!("OUT_DIR"), "/src_middleware_tools_text.rs"));
}

mod coding;
mod commands;
mod patch;

#[cfg(test)]
use coding::ApplyPatchArgs;
use coding::{ApplyPatch, ReadFile, WriteFile};
#[cfg(test)]
use commands::background_output;
use commands::{Bash, PollCommand, StartCommand, StopCommand};
#[cfg(test)]
use patch::{apply_patch_document, parse_patch_document, validate_patch_complexity};

const MAX_TOOL_OUTPUT_BYTES: usize = 40_000;
const MAX_TOOL_UI_BYTES: usize = 512;
const MAX_TOOL_UI_LINES: usize = 5;
const MAX_TOOL_NAME_BYTES: usize = 256;
const MAX_MUTATION_BYTES: usize = 40_000;
const MAX_COMMAND_BYTES: usize = 8_000;
const MAX_PATCH_MATCH_WORK: usize = 32 * 1024 * 1024;

/// Configuration and presentation metadata for workspace tools.
pub const MANIFEST: MiddlewareManifest = MiddlewareManifest {
    id: "tools",
    label: text::MANIFEST_LABEL,
    description: text::MANIFEST_DESCRIPTION,
    required: true,
    default_enabled: true,
    settings: &[],
};

/// Whether a tool can overlap other calls in its model-produced batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Parallel,
    Exclusive,
}

/// Whether a tool requires sandbox mutation approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalRequirement {
    Never,
    Always,
}

/// Dependencies available only to terminal tool handlers.
pub struct ToolContext {
    pub sandbox: Arc<Sandbox>,
    pub permissions: ToolPermissions,
    pub turn_id: String,
}

/// External identity used when a tool participates in extension hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookIdentity {
    /// Tool name exposed to hook matchers and payloads.
    pub name: &'static str,
    /// Matcher subjects checked in declaration order.
    pub subjects: &'static [&'static str],
}

/// A named tool Adapter registered by middleware.
pub trait Tool: Send + Sync {
    /// Returns the provider-facing tool schema.
    fn definition(&self) -> ToolDefinition;

    /// Declares whether calls may overlap.
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Exclusive
    }

    /// Declares whether this tool requires sandbox mutation approval.
    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Never
    }

    /// Allows accepted active input to end a blocking wait at a model boundary.
    fn interrupt_on_active_input(&self) -> bool {
        false
    }

    /// Declares an external hook alias and matcher subjects.
    fn hook_identity(&self) -> Option<HookIdentity> {
        None
    }

    /// Maps provider-facing arguments into the extension hook payload.
    fn hook_input(&self, arguments: &Value) -> Value {
        arguments.clone()
    }

    /// Maps a hook rewrite back into provider-facing arguments.
    fn rewrite_hook_input(&self, input: Value) -> Result<Value> {
        object_hook_input(input)
    }

    /// Executes one validated provider call.
    fn call<'a>(&'a self, context: ToolContext, arguments: Value) -> BoxFuture<'a, Result<String>>;
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HookTool {
    pub(crate) name: String,
    pub(crate) input: Value,
    pub(crate) subjects: Vec<String>,
}

#[derive(Clone)]
struct RegisteredTool {
    definition: ToolDefinition,
    execution_mode: ExecutionMode,
    approval: ApprovalRequirement,
    interrupt_on_active_input: bool,
    handler: Arc<dyn Tool>,
}

/// The validated tool registry built during agent creation.
#[derive(Clone, Default)]
pub struct Catalog {
    tools: BTreeMap<String, RegisteredTool>,
    definitions: Arc<[ToolDefinition]>,
}

impl Catalog {
    /// Registers one tool and rejects invalid definitions or duplicate names.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<()> {
        let definition = tool.definition();
        validate_definition(&definition)?;
        let name = definition.name.clone();
        let entry = RegisteredTool {
            definition,
            execution_mode: tool.execution_mode(),
            approval: tool.approval(),
            interrupt_on_active_input: tool.interrupt_on_active_input(),
            handler: tool,
        };
        if self.tools.contains_key(&name) {
            return Err(Error::Duplicate(format!("tool `{name}`")));
        }
        self.tools.insert(name, entry);
        self.definitions = self
            .tools
            .values()
            .map(|tool| tool.definition.clone())
            .collect::<Vec<_>>()
            .into();
        Ok(())
    }

    /// Returns model-facing definitions in stable name order.
    #[must_use]
    pub fn definitions(&self) -> Arc<[ToolDefinition]> {
        Arc::clone(&self.definitions)
    }

    /// Returns whether the named tool requires approval.
    #[must_use]
    pub fn requires_approval(&self, name: &str) -> bool {
        self.tools
            .get(name)
            .is_some_and(|tool| tool.approval == ApprovalRequirement::Always)
    }

    pub(crate) fn interrupts_on_active_input(&self, calls: &[ToolCall]) -> bool {
        !calls.is_empty()
            && calls.iter().all(|call| {
                self.tools
                    .get(&call.name)
                    .is_some_and(|tool| tool.interrupt_on_active_input)
            })
    }

    pub(crate) fn hook_tool(&self, call: &ToolCall, description: Option<&str>) -> HookTool {
        let registered = self.get(&call.name);
        let identity = registered.and_then(|tool| tool.handler.hook_identity());
        let name = identity.map_or_else(|| call.name.clone(), |identity| identity.name.into());
        let subjects = identity.map_or_else(
            || vec![call.name.clone()],
            |identity| {
                identity
                    .subjects
                    .iter()
                    .map(|subject| (*subject).into())
                    .collect()
            },
        );
        let mut input = registered.map_or_else(
            || call.arguments.clone(),
            |tool| tool.handler.hook_input(&call.arguments),
        );
        if let Some(description) = description
            && let Some(input) = input.as_object_mut()
        {
            input
                .entry("description")
                .or_insert_with(|| Value::String(description.into()));
        }
        HookTool {
            name,
            input,
            subjects,
        }
    }

    pub(crate) fn rewrite_hook_input(&self, name: &str, input: Value) -> Result<Value> {
        match self.get(name) {
            Some(tool) => tool.handler.rewrite_hook_input(input),
            None => object_hook_input(input),
        }
    }

    fn get(&self, name: &str) -> Option<&RegisteredTool> {
        self.tools.get(name)
    }
}

fn validate_definition(definition: &ToolDefinition) -> Result<()> {
    if definition.name.trim().is_empty() {
        return Err(Error::Config("tool name cannot be empty".into()));
    }
    if definition.name.len() > MAX_TOOL_NAME_BYTES {
        return Err(Error::Config(format!(
            "tool name exceeds {MAX_TOOL_NAME_BYTES} bytes"
        )));
    }
    if !definition.parameters.is_object() {
        return Err(Error::Config(format!(
            "tool `{}` parameters must be a JSON object",
            definition.name
        )));
    }
    Ok(())
}

fn object_hook_input(input: Value) -> Result<Value> {
    if input.is_object() {
        Ok(input)
    } else {
        Err(Error::Config(
            "hook tool rewrite must be a JSON object".into(),
        ))
    }
}

/// The result returned to the model for one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    pub output: String,
    pub is_error: bool,
    pub(crate) handler_executed: bool,
    pub(crate) additional_input: Vec<Value>,
}

impl ToolResult {
    pub(crate) fn error(call: &ToolCall, output: impl AsRef<str>) -> Self {
        Self {
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            output: capped(output.as_ref(), MAX_TOOL_OUTPUT_BYTES),
            is_error: true,
            handler_executed: false,
            additional_input: Vec::new(),
        }
    }

    pub(crate) fn replace(&mut self, output: impl AsRef<str>) {
        self.output = capped(output.as_ref(), MAX_TOOL_OUTPUT_BYTES);
    }
}

/// Executes maximal runs of parallel-safe calls concurrently.
/// Exclusive and unknown calls form barriers and execute alone.
pub(crate) async fn execute_batch(
    catalog: &Catalog,
    calls: &[ToolCall],
    sandbox: Arc<Sandbox>,
    permissions: &SandboxPermissions,
    turn_id: &str,
) -> Vec<ToolResult> {
    let mut results = Vec::with_capacity(calls.len());
    let mut index = 0;
    while index < calls.len() {
        if is_parallel(catalog, &calls[index]) {
            let end = calls[index..]
                .iter()
                .position(|call| !is_parallel(catalog, call))
                .map_or(calls.len(), |offset| index + offset);
            // ModelOutput validation bounds every batch to 128 calls.
            results.extend(
                join_all(
                    calls[index..end]
                        .iter()
                        .cloned()
                        .map(|call| execute_call(catalog, call, &sandbox, permissions, turn_id)),
                )
                .await,
            );
            index = end;
        } else {
            results.push(
                execute_call(
                    catalog,
                    calls[index].clone(),
                    &sandbox,
                    permissions,
                    turn_id,
                )
                .await,
            );
            index += 1;
        }
    }
    results
}

fn is_parallel(catalog: &Catalog, call: &ToolCall) -> bool {
    catalog
        .get(&call.name)
        .is_some_and(|tool| tool.execution_mode == ExecutionMode::Parallel)
}

async fn execute_call(
    catalog: &Catalog,
    call: ToolCall,
    sandbox: &Arc<Sandbox>,
    permissions: &SandboxPermissions,
    turn_id: &str,
) -> ToolResult {
    let context = ToolContext {
        sandbox: Arc::clone(sandbox),
        permissions: permissions.for_call(&call.call_id),
        turn_id: turn_id.into(),
    };
    let Some(tool) = catalog.get(&call.name).cloned() else {
        return ToolResult::error(&call, format!("unknown tool `{}`", call.name));
    };
    if tool.approval == ApprovalRequirement::Always && !context.permissions.allows_mutation() {
        return ToolResult::error(&call, "tool call is not authorized to mutate state");
    }
    let ToolCall {
        call_id,
        name,
        arguments,
    } = call;
    let result = AssertUnwindSafe(async move { tool.handler.call(context, arguments).await })
        .catch_unwind()
        .await;
    match result {
        Ok(Ok(output)) => ToolResult {
            call_id,
            name,
            output: capped(&output, MAX_TOOL_OUTPUT_BYTES),
            is_error: false,
            handler_executed: true,
            additional_input: Vec::new(),
        },
        Ok(Err(error)) => ToolResult {
            call_id,
            name,
            output: capped(&error.to_string(), MAX_TOOL_OUTPUT_BYTES),
            is_error: true,
            handler_executed: true,
            additional_input: Vec::new(),
        },
        Err(_) => ToolResult {
            call_id,
            name,
            output: "tool panicked".into(),
            is_error: true,
            handler_executed: true,
            additional_input: Vec::new(),
        },
    }
}

fn capped(output: &str, limit: usize) -> String {
    if output.len() <= limit {
        return output.to_string();
    }

    let left_budget = limit / 2;
    let right_budget = limit - left_budget;
    let left = crate::truncate_utf8(output, left_budget);
    let mut right_start = output.len() - right_budget;
    while !output.is_char_boundary(right_start) {
        right_start += 1;
    }
    let removed = output[left.len()..right_start].chars().count();
    format!(
        "{}…{removed} chars truncated…{}",
        left,
        &output[right_start..]
    )
}

fn compact_output(output: &str) -> String {
    let total_lines = output.lines().count();
    if output.len() <= MAX_TOOL_UI_BYTES && total_lines <= MAX_TOOL_UI_LINES {
        return output.to_string();
    }

    let kept_lines = if total_lines > MAX_TOOL_UI_LINES {
        MAX_TOOL_UI_LINES - 1
    } else {
        total_lines
    };
    let line_budget = MAX_TOOL_UI_BYTES / kept_lines.max(1);
    let mut preview = String::new();
    let mut first = true;
    let mut append = |line: &str| {
        if !first {
            preview.push('\n');
        }
        first = false;
        preview.push_str(&capped(line, line_budget));
    };

    if total_lines <= MAX_TOOL_UI_LINES {
        output.lines().for_each(&mut append);
        return preview;
    }

    let head_lines = (MAX_TOOL_UI_LINES - 1) / 2;
    output.lines().take(head_lines).for_each(&mut append);
    append(&format!(
        "… +{} lines",
        total_lines - (MAX_TOOL_UI_LINES - 1)
    ));
    let mut tail = output
        .lines()
        .rev()
        .take(MAX_TOOL_UI_LINES - 1 - head_lines)
        .collect::<Vec<_>>();
    tail.reverse();
    tail.into_iter().for_each(append);
    preview
}

/// Middleware that contributes an explicit list of tools.
pub struct Tools {
    tools: Vec<Arc<dyn Tool>>,
    names: BTreeSet<String>,
}

impl Tools {
    /// Creates a tool middleware from explicit handlers.
    #[must_use]
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        let names = tools.iter().map(|tool| tool.definition().name).collect();
        Self { tools, names }
    }

    /// Creates the default file, foreground command, and background command tools.
    #[must_use]
    pub fn coding() -> Self {
        Self::new(vec![
            Arc::new(ReadFile),
            Arc::new(WriteFile),
            Arc::new(ApplyPatch),
            Arc::new(Bash),
            Arc::new(StartCommand),
            Arc::new(PollCommand),
            Arc::new(StopCommand),
        ])
    }

    fn section(&self) -> PromptSection {
        PromptSection::new(text::PROMPT_MAIN)
    }
}

impl Middleware for Tools {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn register(&self, catalog: &mut Catalog, _runtime: &super::RuntimeContext) -> Result<()> {
        for tool in &self.tools {
            catalog.register(Arc::clone(tool))?;
        }
        Ok(())
    }

    fn prompt_section(&self, _runtime: &super::RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(Some(self.section()))
    }

    fn frontend(&self) -> FrontendContribution {
        FrontendContribution {
            capability: self.name().into(),
            ..FrontendContribution::default()
        }
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        let mut block = render_tool_event(event, |name| self.names.contains(name), tool_heading)?;
        match event {
            EventMsg::ToolCallBegin(call) if call.name == "read_file" => {
                block.group = Some(format!("read:{}", call.turn_id));
            }
            EventMsg::ToolCallEnd(result) if result.name == "read_file" => {
                block.group = Some(format!("read:{}", result.turn_id));
            }
            EventMsg::ToolCallEnd(result)
                if !result.is_error
                    && result.name == "apply_patch"
                    && Patch::from_str(&result.output).is_ok() =>
            {
                block.update = FrontendBlockUpdate::Replace;
                block.title = tool_heading(&result.name, &Value::Null).title;
                block.text = result.output.clone();
                block.format = FrontendBlockFormat::UnifiedDiff;
            }
            _ => {}
        }
        Some(block)
    }
}

pub(crate) fn render_tool_event(
    event: &EventMsg,
    owns: impl Fn(&str) -> bool,
    heading: impl Fn(&str, &Value) -> ToolHeading,
) -> Option<FrontendBlock> {
    match event {
        EventMsg::ToolCallBegin(call) if owns(&call.name) => {
            let heading = heading(&call.name, &call.arguments);
            Some(FrontendBlock {
                id: Some(format!("{}/{}", call.turn_id, call.call_id)),
                group: None,
                update: FrontendBlockUpdate::Replace,
                state: FrontendBlockState::Pending,
                role: FrontendBlockRole::Tool,
                title: heading.title,
                text: heading.detail,
                symbol: None,
                files: Vec::new(),
                format: FrontendBlockFormat::PlainText,
                tone: FrontendTone::Neutral,
            })
        }
        EventMsg::ToolCallEnd(result) if owns(&result.name) => {
            let output = compact_output(&result.output);
            Some(FrontendBlock {
                id: Some(format!("{}/{}", result.turn_id, result.call_id)),
                group: None,
                update: FrontendBlockUpdate::Append,
                state: FrontendBlockState::Complete,
                role: FrontendBlockRole::Tool,
                title: tool_heading(&result.name, &Value::Null).title,
                text: output,
                symbol: None,
                files: Vec::new(),
                format: FrontendBlockFormat::PlainText,
                tone: if result.is_error {
                    FrontendTone::Error
                } else {
                    FrontendTone::Success
                },
            })
        }
        _ => None,
    }
}

pub(crate) struct ToolHeading {
    pub(crate) title: String,
    pub(crate) detail: String,
}

impl From<&str> for ToolHeading {
    fn from(title: &str) -> Self {
        Self {
            title: title.into(),
            detail: String::new(),
        }
    }
}

impl From<String> for ToolHeading {
    fn from(title: String) -> Self {
        Self {
            title,
            detail: String::new(),
        }
    }
}

fn tool_heading(name: &str, arguments: &Value) -> ToolHeading {
    if name == "apply_patch" {
        let detail = arguments
            .get("patch")
            .and_then(Value::as_str)
            .and_then(|patch| {
                patch
                    .lines()
                    .find_map(|line| line.strip_prefix("*** Update File: "))
            })
            .unwrap_or_default()
            .into();
        return ToolHeading {
            title: text::RENDER_APPLY_PATCH.into(),
            detail,
        };
    }
    let (label, detail) = match name {
        "read_file" => (text::RENDER_READ_FILE, "path"),
        "write_file" => (text::RENDER_WRITE_FILE, "path"),
        "bash" => (text::RENDER_BASH, "command"),
        "start_command" => (text::RENDER_START_COMMAND, "command"),
        "poll_command" => (text::RENDER_POLL_COMMAND, "command_id"),
        "stop_command" => (text::RENDER_STOP_COMMAND, "command_id"),
        _ => {
            return ToolHeading {
                title: name.into(),
                detail: preview_json(arguments),
            };
        }
    };
    labeled_tool_heading(label, detail, arguments)
}

pub(crate) fn labeled_tool_heading(label: &str, detail: &str, arguments: &Value) -> ToolHeading {
    ToolHeading {
        title: label.into(),
        detail: arguments
            .get(detail)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or_default()
            .into(),
    }
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
