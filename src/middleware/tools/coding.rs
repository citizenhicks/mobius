use diffy::DiffOptions;
use serde::Deserialize;
use serde_json::Value;

use super::patch::{apply_patch_document, parse_patch_document};
use super::{
    ApprovalRequirement, ExecutionMode, HookIdentity, MAX_MUTATION_BYTES, MAX_TOOL_OUTPUT_BYTES,
    Tool, ToolContext, ToolExposure, text,
};
use crate::backend::model::ToolDefinition;
use crate::backend::session_files::SessionFileStore;
use crate::protocol::{ContentPart, ImageDetail, ToolContent, ToolResponse};
use crate::{BoxFuture, Error, Result};

#[derive(Deserialize)]
struct PathArgs {
    path: String,
}

pub(super) struct ReadFile;

impl Tool for ReadFile {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_file".into(),
            description: text::TOOL_READ_FILE_DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": text::TOOL_READ_FILE_PARAMETER_PATH_DESCRIPTION
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn call<'a>(
        &'a self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let arguments: PathArgs = serde_json::from_value(arguments)?;
            context.sandbox.read(&arguments.path).await.map(Into::into)
        })
    }
}

pub(super) struct ViewImage {
    pub(super) store: SessionFileStore,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewImageArgs {
    images: Vec<ImageSource>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageSource {
    path: Option<String>,
    file_id: Option<String>,
    #[serde(default)]
    detail: ImageDetail,
}

impl Tool for ViewImage {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "view_image".into(),
            description: text::TOOL_VIEW_IMAGE_DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"images": {
                    "type": "array", "minItems": 1, "maxItems": 16,
                    "items": {"type": "object", "properties": {
                        "path": {"type": "string", "description": text::TOOL_VIEW_IMAGE_PARAMETER_PATH_DESCRIPTION},
                        "file_id": {"type": "string", "description": "An image file ID authorized in this session; provide either path or file_id."},
                        "detail": {"type": "string", "enum": ["auto", "low", "high"]}
                    }, "additionalProperties": false,
                    "oneOf": [{"required": ["path"]}, {"required": ["file_id"]}]}
                }},
                "required": ["images"], "additionalProperties": false
            }),
        }
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn call<'a>(
        &'a self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<ToolResponse>> {
        Box::pin(async move {
            let arguments: ViewImageArgs = serde_json::from_value(arguments)?;
            if arguments.images.is_empty() || arguments.images.len() > 16 {
                return Err(Error::Tool("view_image requires 1–16 images".into()));
            }
            let mut content = Vec::new();
            for source in arguments.images {
                let image = match (source.path, source.file_id) {
                    (Some(path), None) => {
                        let bytes = context
                            .sandbox
                            .read_bytes(&path, crate::backend::sandbox::MAX_BINARY_FILE_BYTES)
                            .await?;
                        let name = std::path::Path::new(&path)
                            .file_name()
                            .and_then(|name| name.to_str())
                            .ok_or_else(|| Error::Tool("image path must name a file".into()))?;
                        self.store
                            .ingest_image(
                                context.permissions.session_id(),
                                name.into(),
                                bytes,
                                source.detail,
                            )
                            .await?
                    }
                    (None, Some(file_id)) => {
                        let file = self
                            .store
                            .file_reference(context.permissions.session_id(), &file_id)
                            .await?;
                        self.store
                            .inspect_image(context.permissions.session_id(), &file, source.detail)
                            .await?
                    }
                    _ => {
                        return Err(Error::Tool(
                            "each image requires exactly one of path or file_id".into(),
                        ));
                    }
                };
                content.push(ContentPart::Text {
                    text: format!(
                        "{}: {} × {} pixels; file_id={}",
                        image.file.name, image.width, image.height, image.file.id
                    ),
                });
                content.push(ContentPart::Image { image });
            }
            Ok(ToolResponse {
                content: ToolContent(content),
                is_error: false,
            })
        })
    }
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

pub(super) struct WriteFile;

impl Tool for WriteFile {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_file".into(),
            description: text::TOOL_WRITE_FILE_DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"],
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

    fn call<'a>(
        &'a self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let arguments: WriteArgs = serde_json::from_value(arguments)?;
            if arguments.content.len() > MAX_MUTATION_BYTES {
                return Err(Error::Tool(format!(
                    "content exceeds {MAX_MUTATION_BYTES} bytes"
                )));
            }
            context
                .sandbox
                .write(&arguments.path, &arguments.content, &context.permissions)
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
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "apply_patch".into(),
            description: text::TOOL_APPLY_PATCH_DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": text::TOOL_APPLY_PATCH_PARAMETER_PATCH_DESCRIPTION
                    }
                },
                "required": ["patch"],
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
            name: "apply_patch",
            subjects: &["apply_patch", "Edit", "Write"],
        })
    }

    fn hook_input(&self, arguments: &Value) -> Value {
        serde_json::json!({
            "command": arguments.get("patch").cloned().unwrap_or(Value::Null)
        })
    }

    fn rewrite_hook_input(&self, input: Value) -> Result<Value> {
        let command = input
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Config("hook tool rewrite requires `command`".into()))?;
        Ok(serde_json::json!({"patch": command}))
    }

    fn call<'a>(
        &'a self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let arguments: ApplyPatchArgs = serde_json::from_value(arguments)?;
            if arguments.patch.len() > MAX_MUTATION_BYTES {
                return Err(Error::Tool(format!(
                    "patch exceeds {MAX_MUTATION_BYTES} bytes"
                )));
            }
            let document = parse_patch_document(&arguments.patch)?;
            let content = context.sandbox.read(&document.path).await?;
            let updated = apply_patch_document(&content, &document)?;
            if updated == content {
                return Err(Error::Tool(
                    "Patch rejected: patch applies but makes no changes.".into(),
                ));
            }
            let mut options = DiffOptions::new();
            options
                .set_original_filename(document.path.clone())
                .set_modified_filename(document.path.clone());
            let diff = options.create_patch(&content, &updated).to_string();
            context
                .sandbox
                .write(&document.path, &updated, &context.permissions)
                .await?;
            Ok((if diff.len() <= MAX_TOOL_OUTPUT_BYTES {
                diff
            } else {
                format!("patched {} (diff too large to display)", document.path)
            })
            .into())
        })
    }
}
