use diffy::{DiffOptions, Patch};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

use super::patch::{apply_patch_document, parse_patch_document};
use super::{
    ApprovalRequirement, ExecutionMode, HookIdentity, MAX_MUTATION_BYTES, Tool, ToolContext,
    ToolExposure,
};
use crate::backend::model::ToolDefinition;
use crate::backend::session_files::SessionFileStore;
use crate::protocol::{
    ContentPart, EventMsg, FrontendBlock, FrontendBlockFormat, FrontendBlockUpdate, ImageDetail,
    ToolContent, ToolResponse,
};
use crate::{BoxFuture, Error, Result};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    read_file: super::ToolSpec,
    write_file: super::ToolSpec,
    apply_patch: super::ToolSpec,
}
crate::embedded_config! { static DEFINITION: Definition = include_str!("coding.toml"); }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: Option<String>,
    file_id: Option<String>,
}

/// Reads text files and views images, by workspace path or authorized session file.
pub(super) struct ReadFile {
    pub(super) store: Arc<SessionFileStore>,
}

impl Tool for ReadFile {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &DEFINITION.read_file.tool
    }

    fn render(&self, event: &crate::protocol::EventMsg) -> Option<crate::protocol::FrontendBlock> {
        let mut block = DEFINITION.read_file.render(event)?;
        if let EventMsg::ToolCallBegin(call) = event
            && call.arguments.get("path").and_then(Value::as_str).is_none()
            && let Some(file_id) = call.arguments.get("file_id").and_then(Value::as_str)
        {
            block.text = super::formatted_tool_text(file_id);
        }
        Some(block)
    }

    fn read_only(&self) -> bool {
        true
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn call(&self, context: ToolContext, arguments: Value) -> BoxFuture<'_, Result<ToolResponse>> {
        Box::pin(async move {
            let arguments: ReadArgs = serde_json::from_value(arguments)?;
            let session = context.permissions.session_id();
            // Models without image input see a text placeholder; the request layer swaps it in.
            let image = match (arguments.path, arguments.file_id) {
                (Some(path), None) => {
                    let bytes = context
                        .sandbox
                        .read_bytes(
                            &path,
                            crate::backend::sandbox::MAX_BINARY_FILE_BYTES,
                            &context.permissions,
                        )
                        .await?;
                    if image::guess_format(&bytes).is_err() {
                        if bytes.len() > crate::backend::sandbox::MAX_FILE_BYTES {
                            return Err(Error::Sandbox("file exceeds read limit".into()));
                        }
                        return String::from_utf8(bytes).map(Into::into).map_err(|error| {
                            Error::Sandbox(format!("{path} is not valid UTF-8: {error}"))
                        });
                    }
                    let name = std::path::Path::new(&path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .ok_or_else(|| Error::Tool("image path must name a file".into()))?;
                    self.store
                        .ingest_image(session, name.into(), bytes, ImageDetail::Auto)
                        .await?
                }
                (None, Some(file_id)) => {
                    self.store
                        .inspect_image_id(session, &file_id, ImageDetail::Auto)
                        .await?
                }
                _ => return Err(Error::Tool("provide exactly one of path or file_id".into())),
            };
            Ok(ToolResponse {
                content: ToolContent(vec![
                    ContentPart::Text {
                        text: format!(
                            "{}: {} × {} pixels; file_id={}",
                            image.file.name, image.width, image.height, image.file.id
                        ),
                    },
                    ContentPart::Image { image },
                ]),
                is_error: false,
            })
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}

pub(super) struct WriteFile;

impl Tool for WriteFile {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &DEFINITION.write_file.tool
    }

    fn render(&self, event: &crate::protocol::EventMsg) -> Option<crate::protocol::FrontendBlock> {
        DEFINITION.write_file.render(event)
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Always
    }

    fn call(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let arguments: WriteArgs = serde_json::from_value(arguments)?;
            if arguments.content.len() > MAX_MUTATION_BYTES {
                return Err(Error::Tool(format!(
                    "content exceeds {MAX_MUTATION_BYTES} bytes"
                )));
            }
            context
                .sandbox
                .write(
                    &arguments.path,
                    &arguments.content,
                    context.permissions.mutation()?,
                )
                .await?;
            Ok((format!(
                "wrote {} bytes to {}",
                arguments.content.len(),
                arguments.path
            ))
            .into())
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ApplyPatchArgs {
    patch: String,
}

pub(super) struct ApplyPatch;

impl Tool for ApplyPatch {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &DEFINITION.apply_patch.tool
    }

    fn prompt_section(&self) -> Option<&str> {
        DEFINITION.apply_patch.prompt.as_deref()
    }

    fn render(&self, event: &EventMsg) -> Option<FrontendBlock> {
        let mut block = super::render_tool_event(
            event,
            |name| name == DEFINITION.apply_patch.tool.name,
            |_, arguments| {
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
                super::ToolHeading {
                    title: DEFINITION.apply_patch.title.clone(), // rules-allow: the heading owns its title from static text
                    detail,
                }
            },
        )?;
        if let EventMsg::ToolCallEnd(result) = event
            && !result.is_error
            && Patch::from_str(&result.output.text()).is_ok()
        {
            block.update = FrontendBlockUpdate::Replace;
            block.title.clone_from(&DEFINITION.apply_patch.title);
            block.text = result.output.text();
            block.format = FrontendBlockFormat::UnifiedDiff;
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
            name: "apply_patch",
            subjects: &["apply_patch", "Edit", "Write"],
        })
    }

    fn hook_input(&self, arguments: &Value) -> Value {
        serde_json::json!({
            "command": arguments.get("patch")
        })
    }

    fn rewrite_hook_input(&self, input: Value) -> Result<Value> {
        let command = input
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Config("hook tool rewrite requires `command`".into()))?;
        Ok(serde_json::json!({"patch": command}))
    }

    fn call(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let arguments: ApplyPatchArgs = serde_json::from_value(arguments)?;
            if arguments.patch.len() > MAX_MUTATION_BYTES {
                return Err(Error::Tool(format!(
                    "patch exceeds {MAX_MUTATION_BYTES} bytes"
                )));
            }
            let document = parse_patch_document(&arguments.patch)?;
            let content = context
                .sandbox
                .read(&document.path, &context.permissions)
                .await?;
            let updated = apply_patch_document(&content, &document)?;
            if updated == content {
                return Err(Error::Tool(
                    "patch rejected: patch applies but makes no changes".into(),
                ));
            }
            context
                .sandbox
                .write(&document.path, &updated, context.permissions.mutation()?)
                .await?;
            let mut options = DiffOptions::new();
            options
                .set_original_filename(document.path.clone()) // rules-allow: diffy requires a second 'static filename
                .set_modified_filename(document.path);
            let patch = options.create_patch(&content, &updated);
            let diff = patch.to_string();
            Ok((if diff.len() <= context.sandbox.output_limit() {
                diff
            } else {
                format!(
                    "patched {} (diff too large to display)",
                    patch.modified().unwrap_or_default()
                )
            })
            .into())
        })
    }
}
