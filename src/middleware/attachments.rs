//! User uploads exposed to the owning workspace.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::manifest::MiddlewareManifest;
use super::session_files::{SessionFileStore, session_storage_key};
use super::tools::{Catalog, ExecutionMode, Tool, ToolContext, render_tool_event};
use super::{
    Middleware, ModelContext, ModelRequestContext, PromptSection, RuntimeContext,
    SessionStartContext, SessionStartSource,
};
use crate::backend::model::{ToolDefinition, internal_user_message};
use crate::protocol::{
    ATTACHMENT_CONTEXT_MARKER, ATTACHMENTS_FIELD, EventMsg, FrontendBlock, FrontendContribution,
    INTERNAL_MESSAGE_FIELD, MESSAGE_METADATA_FIELD, MessageEvent, SessionFileReference,
    internal_message_kind,
};
use crate::{BoxFuture, Error, Result};

mod text {
    include!(concat!(
        env!("OUT_DIR"),
        "/src_middleware_attachments_text.rs"
    ));
}

const MAX_DIRECT_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MATERIALIZED_ATTACHMENTS_FIELD: &str = "_mobius_attachment_blobs";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MaterializedAttachment {
    reference: SessionFileReference,
    content_hash: Option<String>,
    image_media_type: Option<String>,
    #[serde(default)]
    path: Option<String>,
    unavailable_reason: Option<String>,
}
/// Configuration metadata for protected user uploads.
pub const MANIFEST: MiddlewareManifest = MiddlewareManifest {
    id: "attachments",
    label: text::MANIFEST_LABEL,
    description: text::MANIFEST_DESCRIPTION,
    required: false,
    default_enabled: true,
    settings: &[],
};

/// Optional middleware exposing user uploads to the owning workspace.
#[derive(Clone)]
pub struct Attachments {
    store: SessionFileStore,
    workspace: Option<Arc<Dir>>,
    workspace_path: Option<PathBuf>,
}

impl Attachments {
    #[must_use]
    pub fn new(store: SessionFileStore) -> Self {
        Self {
            store,
            workspace: None,
            workspace_path: None,
        }
    }

    /// Exposes uploads as workspace-local copies below the workspace's `.mobius` directory.
    ///
    /// Every session using that workspace can read those project-local files.
    pub fn with_workspace(mut self, workspace: impl AsRef<Path>) -> Result<Self> {
        let workspace = std::fs::canonicalize(workspace)?;
        if !workspace.is_dir() {
            return Err(Error::Config(
                "attachment workspace is not a directory".into(),
            ));
        }
        self.workspace = Some(Arc::new(Dir::open_ambient_dir(
            &workspace,
            ambient_authority(),
        )?));
        self.workspace_path = Some(workspace);
        Ok(self)
    }
}

impl Middleware for Attachments {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        catalog.register(Arc::new(ListAttachments {
            store: self.store.clone(),
            session_id: runtime.session_id.clone(),
            workspace: self.workspace.clone(),
        }))
    }

    fn session_start<'a>(
        &'a self,
        context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if context.source() == SessionStartSource::Compact {
                return Ok(());
            }
            let Some(workspace) = self.workspace_path.as_deref() else {
                return Ok(());
            };
            self.store
                .register_attachment_workspace(&context.runtime.session_id, workspace)
                .await
        })
    }

    fn prompt_section(&self, _runtime: &RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(Some(PromptSection::new(text::PROMPT_MAIN)))
    }

    fn frontend(&self) -> FrontendContribution {
        FrontendContribution {
            capability: MANIFEST.id.into(),
            accepts_file_attachments: true,
            ..FrontendContribution::default()
        }
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        render_tool_event(
            event,
            |name| name == "list_attachments",
            |name, _| {
                if matches!(event, EventMsg::ToolCallEnd(_)) {
                    name.into()
                } else {
                    text::RENDER_LIST_ATTACHMENTS.into()
                }
            },
        )
    }

    fn pre_model<'a>(&'a self, context: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some((message_index, references)) = referenced_attachments(context.input())?.pop()
            else {
                return Ok(());
            };
            if materialization_matches(context.input(), message_index, &references)? {
                for reference in &references {
                    let content_hash = self
                        .store
                        .upload_content_hash(context.session_id, reference)
                        .await?;
                    stage_attachment(
                        &self.store,
                        self.workspace.as_deref(),
                        context.session_id,
                        reference,
                        &content_hash,
                    )
                    .await?;
                }
                return Ok(());
            }
            if message_index + 1 != context.input().len() {
                return Err(Error::Checkpoint(
                    "attachment-bearing user message is missing adjacent materialization".into(),
                ));
            }
            let mut direct_image_bytes = 0_usize;
            let mut materialized = Vec::with_capacity(references.len());
            let mut first_error = None;
            for reference in references {
                let content_hash = match self
                    .store
                    .upload_content_hash(context.session_id, &reference)
                    .await
                {
                    Ok(content_hash) => content_hash,
                    Err(error) => {
                        let reason = error.to_string();
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                        materialized.push(MaterializedAttachment {
                            reference,
                            content_hash: None,
                            image_media_type: None,
                            path: None,
                            unavailable_reason: Some(reason),
                        });
                        continue;
                    }
                };
                let path = match stage_attachment(
                    &self.store,
                    self.workspace.as_deref(),
                    context.session_id,
                    &reference,
                    &content_hash,
                )
                .await
                {
                    Ok(path) => path,
                    Err(error) => {
                        let reason = error.to_string();
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                        materialized.push(MaterializedAttachment {
                            reference,
                            content_hash: Some(content_hash),
                            image_media_type: None,
                            path: None,
                            unavailable_reason: Some(reason),
                        });
                        continue;
                    }
                };
                let image_media_type = if reference.media_type.starts_with("image/") {
                    let result = usize::try_from(reference.size)
                        .ok()
                        .and_then(|size| direct_image_bytes.checked_add(size))
                        .filter(|size| *size <= MAX_DIRECT_IMAGE_BYTES)
                        .ok_or_else(|| {
                            Error::Provider(
                                "image attachments exceed the 8 MiB model-input limit".into(),
                            )
                        });
                    match result {
                        Ok(next_image_bytes) => {
                            let bytes = self
                                .store
                                .read_content_blob(&content_hash, reference.size)
                                .await;
                            match bytes
                                .and_then(|bytes| raster_media_type(&bytes).map(str::to_string))
                            {
                                Ok(media_type) => {
                                    direct_image_bytes = next_image_bytes;
                                    Some(media_type)
                                }
                                Err(error) => {
                                    let reason = error.to_string();
                                    if first_error.is_none() {
                                        first_error = Some(error);
                                    }
                                    materialized.push(MaterializedAttachment {
                                        reference,
                                        content_hash: Some(content_hash),
                                        image_media_type: None,
                                        path,
                                        unavailable_reason: Some(reason),
                                    });
                                    continue;
                                }
                            }
                        }
                        Err(error) => {
                            let reason = error.to_string();
                            if first_error.is_none() {
                                first_error = Some(error);
                            }
                            materialized.push(MaterializedAttachment {
                                reference,
                                content_hash: Some(content_hash),
                                image_media_type: None,
                                path,
                                unavailable_reason: Some(reason),
                            });
                            continue;
                        }
                    }
                } else {
                    None
                };
                materialized.push(MaterializedAttachment {
                    reference,
                    content_hash: Some(content_hash),
                    image_media_type,
                    path,
                    unavailable_reason: None,
                });
            }
            context.append_model_input(materialization_message(&materialized)?);
            first_error.map_or(Ok(()), Err)
        })
    }

    fn model_request<'a>(
        &'a self,
        context: &'a mut ModelRequestContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let latest_user = context.input().iter().rposition(is_real_user);
            let supports_image_input = context.model.supports_image_input(context.provider)?;
            let mut direct_image_bytes = 0_usize;
            let mut input = context.input().to_vec();
            let mut changed = false;
            for message_index in (0..input.len()).rev() {
                let Some(materialized) = materialized_attachments(&input[message_index])? else {
                    continue;
                };
                let current = source_user_index(&input, message_index) == latest_user;
                let mut images = Vec::new();
                for attachment in materialized {
                    if attachment.unavailable_reason.is_some() {
                        continue;
                    }
                    let Some(media_type) = attachment.image_media_type else {
                        continue;
                    };
                    let content_hash = attachment.content_hash.ok_or_else(|| {
                        Error::Checkpoint(
                            "available materialized attachment omitted content hash".into(),
                        )
                    })?;
                    if !supports_image_input {
                        if current {
                            return Err(Error::Provider(
                                "the selected model does not support image input".into(),
                            ));
                        }
                        continue;
                    }
                    let Some(next_image_bytes) = usize::try_from(attachment.reference.size)
                        .ok()
                        .and_then(|size| direct_image_bytes.checked_add(size))
                        .filter(|size| *size <= MAX_DIRECT_IMAGE_BYTES)
                    else {
                        if current {
                            return Err(Error::Provider(
                                "image attachments exceed the 8 MiB model-input limit".into(),
                            ));
                        }
                        continue;
                    };
                    let bytes = self
                        .store
                        .read_content_blob(&content_hash, attachment.reference.size)
                        .await?;
                    if raster_media_type(&bytes)? != media_type {
                        return Err(Error::Checkpoint(
                            "materialized attachment media type changed".into(),
                        ));
                    }
                    images.push(serde_json::json!({
                        "type": "input_image",
                        "media_type": media_type,
                        "data": base64::engine::general_purpose::STANDARD.encode(bytes)
                    }));
                    direct_image_bytes = next_image_bytes;
                }
                if images.is_empty() {
                    continue;
                }
                let content = input[message_index]
                    .get_mut("content")
                    .and_then(Value::as_array_mut)
                    .ok_or_else(|| {
                        Error::Checkpoint(
                            "materialized attachment context has invalid content".into(),
                        )
                    })?;
                content.extend(images);
                changed = true;
            }
            if changed {
                context.replace_input(input);
            }
            Ok(())
        })
    }
}

const FORKED_ATTACHMENT_PLACEHOLDER: &str = "[Attachment unavailable in this fork]";

pub(crate) fn strip_attachment_references(items: &mut Vec<Value>) {
    for item in items.iter_mut() {
        let needs_placeholder = item.get("role").and_then(Value::as_str) == Some("user")
            && !attachment_references(item).is_empty()
            && message_text(item, "user").is_none_or(|text| text.trim().is_empty());
        if let Some(object) = item.as_object_mut() {
            object.remove(ATTACHMENTS_FIELD);
            if needs_placeholder {
                object.insert(
                    "content".into(),
                    serde_json::json!([{
                        "type": "input_text",
                        "text": FORKED_ATTACHMENT_PLACEHOLDER
                    }]),
                );
            }
            if let Some(mut message) = object
                .get(MESSAGE_METADATA_FIELD)
                .cloned()
                .and_then(|value| serde_json::from_value::<MessageEvent>(value).ok())
            {
                message.attachments.clear();
                if needs_placeholder {
                    message.text = FORKED_ATTACHMENT_PLACEHOLDER.into();
                }
                if let Ok(metadata) = serde_json::to_value(message) {
                    object.insert(MESSAGE_METADATA_FIELD.into(), metadata);
                }
            }
        }
    }
    items.retain(|item| internal_message_kind(item) != Some(ATTACHMENT_CONTEXT_MARKER));
}

fn attachment_references(value: &Value) -> Vec<SessionFileReference> {
    value
        .get(ATTACHMENTS_FIELD)
        .cloned()
        .map(serde_json::from_value)
        .and_then(std::result::Result::ok)
        .unwrap_or_default()
}

fn message_text(value: &Value, role: &str) -> Option<String> {
    if value.get("role").and_then(Value::as_str) != Some(role) {
        return None;
    }
    let content = value.get("content")?;
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text: String = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect();
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

pub(crate) fn is_attachment_materialization(item: &Value) -> bool {
    internal_message_kind(item) == Some(ATTACHMENT_CONTEXT_MARKER)
}

fn is_real_user(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("user")
        && item.get(INTERNAL_MESSAGE_FIELD).is_none()
}

fn source_user_index(input: &[Value], materialization_index: usize) -> Option<usize> {
    input[..materialization_index]
        .iter()
        .rposition(is_real_user)
}

fn materialization_message(attachments: &[MaterializedAttachment]) -> Result<Value> {
    let available = attachments
        .iter()
        .filter(|attachment| attachment.unavailable_reason.is_none())
        .collect::<Vec<_>>();
    let unavailable = attachments
        .iter()
        .filter(|attachment| attachment.unavailable_reason.is_some())
        .collect::<Vec<_>>();
    let mut message = internal_user_message(
        ATTACHMENT_CONTEXT_MARKER,
        &render_attachment_context(&available, &unavailable),
    );
    message[MATERIALIZED_ATTACHMENTS_FIELD] = serde_json::to_value(attachments)?;
    Ok(message)
}

fn materialized_attachments(item: &Value) -> Result<Option<Vec<MaterializedAttachment>>> {
    if !is_attachment_materialization(item) {
        return Ok(None);
    }
    let value = item.get(MATERIALIZED_ATTACHMENTS_FIELD).ok_or_else(|| {
        Error::Checkpoint("materialized attachment context omitted blob metadata".into())
    })?;
    let attachments = serde_json::from_value(value.clone()).map_err(|error| {
        Error::Checkpoint(format!("invalid materialized attachment context: {error}"))
    })?;
    Ok(Some(attachments))
}

fn materialization_matches(
    input: &[Value],
    user_index: usize,
    references: &[SessionFileReference],
) -> Result<bool> {
    let Some(item) = input.get(user_index + 1) else {
        return Ok(false);
    };
    let Some(materialized) = materialized_attachments(item)? else {
        return Ok(false);
    };
    Ok(materialized
        .iter()
        .map(|attachment| &attachment.reference)
        .eq(references))
}

async fn stage_attachment(
    store: &SessionFileStore,
    workspace: Option<&Dir>,
    session_id: &str,
    reference: &SessionFileReference,
    content_hash: &str,
) -> Result<Option<String>> {
    let Some(workspace) = workspace else {
        return Ok(None);
    };
    let relative = staged_attachment_path(session_id, reference);
    let destination = ensure_staging_directories(workspace, session_id, &reference.id)?;
    let source = store
        .content_blob_path(content_hash, reference.size)
        .await?;
    replace_with_copy(&source, &destination, &reference.name)?;
    let path = relative
        .to_str()
        .ok_or_else(|| Error::Tool("attachment workspace path is not UTF-8".into()))?;
    Ok(Some(path.into()))
}

fn staged_attachment_path(session_id: &str, reference: &SessionFileReference) -> PathBuf {
    PathBuf::from(".mobius")
        .join("attachments")
        .join(session_storage_key(session_id))
        .join(&reference.id)
        .join(&reference.name)
}

fn ensure_staging_directories(
    workspace: &Dir,
    session_id: &str,
    attachment_id: &str,
) -> Result<Dir> {
    let mobius = open_or_create_dir(workspace, ".mobius")?;
    let attachments = open_or_create_dir(&mobius, "attachments")?;
    let session = open_or_create_dir(&attachments, &session_storage_key(session_id))?;
    open_or_create_dir(&session, attachment_id)
}

fn open_or_create_dir(parent: &Dir, name: &str) -> Result<Dir> {
    match parent.create_dir(name) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let before = parent.symlink_metadata(name)?;
    if before.is_symlink() || !before.is_dir() {
        return Err(Error::Tool(format!(
            "attachment workspace path is not a directory: {name}"
        )));
    }
    let directory = parent.open_dir(name)?;
    if !same_file(&before, &directory.dir_metadata()?) {
        return Err(Error::Tool(
            "attachment workspace directory changed while opening it".into(),
        ));
    }
    Ok(directory)
}

#[cfg(unix)]
fn same_file(left: &cap_std::fs::Metadata, right: &cap_std::fs::Metadata) -> bool {
    use cap_std::fs::MetadataExt as _;

    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(_left: &cap_std::fs::Metadata, _right: &cap_std::fs::Metadata) -> bool {
    true
}

fn replace_with_copy(source: &Path, destination: &Dir, name: &str) -> Result<()> {
    let source_name = source
        .file_name()
        .ok_or_else(|| Error::Tool("attachment blob path has no filename".into()))?;
    let source_dir = Dir::open_ambient_dir(
        source
            .parent()
            .ok_or_else(|| Error::Tool("attachment blob path has no parent".into()))?,
        ambient_authority(),
    )?;
    if let Ok(existing) = destination.symlink_metadata(name)
        && existing.is_file()
        && !existing.is_symlink()
    {
        #[cfg(unix)]
        if let Ok(source) = source_dir.metadata(source_name)
            && !same_file(&source, &existing)
        {
            return Ok(());
        }
    }
    let temporary = format!(".{}.copy", uuid::Uuid::new_v4());
    source_dir
        .copy(source_name, destination, &temporary)
        .map_err(|error| {
            Error::Tool(format!(
                "attachment cannot be copied into the workspace: {error}"
            ))
        })?;
    if let Err(error) = destination.rename(&temporary, destination, name) {
        let _ = destination.remove_file(&temporary);
        return Err(error.into());
    }
    let _ = destination.remove_file(&temporary);
    Ok(())
}

fn raster_media_type(bytes: &[u8]) -> Result<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Ok("image/jpeg");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        if bytes
            .windows(4)
            .any(|window| matches!(window, b"ANIM" | b"ANMF"))
        {
            return Err(Error::Provider(
                "animated WebP attachments are not supported".into(),
            ));
        }
        return Ok("image/webp");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        if gif_image_count(bytes)? == 1 {
            return Ok("image/gif");
        }
        return Err(Error::Provider(
            "animated GIF attachments are not supported".into(),
        ));
    }
    Err(Error::Provider(
        "image attachment is not a supported PNG, JPEG, WebP, or GIF".into(),
    ))
}

fn gif_image_count(bytes: &[u8]) -> Result<usize> {
    if bytes.len() < 13 {
        return Err(Error::Provider("GIF attachment is truncated".into()));
    }
    let packed = bytes[10];
    let global_table = if packed & 0x80 == 0 {
        0
    } else {
        3_usize << (usize::from(packed & 0x07) + 1)
    };
    let mut offset = 13_usize
        .checked_add(global_table)
        .ok_or_else(|| Error::Provider("GIF attachment size overflow".into()))?;
    let mut images = 0_usize;
    while offset < bytes.len() {
        match bytes[offset] {
            0x2c => {
                images += 1;
                if images > 1 {
                    return Ok(images);
                }
                let descriptor_end = offset
                    .checked_add(10)
                    .filter(|end| *end <= bytes.len())
                    .ok_or_else(|| Error::Provider("GIF image descriptor is truncated".into()))?;
                let packed = bytes[descriptor_end - 1];
                let local_table = if packed & 0x80 == 0 {
                    0
                } else {
                    3_usize << (usize::from(packed & 0x07) + 1)
                };
                offset = descriptor_end
                    .checked_add(local_table)
                    .and_then(|value| value.checked_add(1))
                    .filter(|value| *value <= bytes.len())
                    .ok_or_else(|| Error::Provider("GIF image data is truncated".into()))?;
                offset = skip_gif_sub_blocks(bytes, offset)?;
            }
            0x21 => {
                offset = offset
                    .checked_add(2)
                    .filter(|value| *value <= bytes.len())
                    .ok_or_else(|| Error::Provider("GIF extension is truncated".into()))?;
                offset = skip_gif_sub_blocks(bytes, offset)?;
            }
            0x3b => return Ok(images),
            _ => return Err(Error::Provider("GIF attachment is malformed".into())),
        }
    }
    Err(Error::Provider("GIF attachment has no trailer".into()))
}

fn skip_gif_sub_blocks(bytes: &[u8], mut offset: usize) -> Result<usize> {
    loop {
        let length = usize::from(
            *bytes
                .get(offset)
                .ok_or_else(|| Error::Provider("GIF data block is truncated".into()))?,
        );
        offset = offset
            .checked_add(1)
            .and_then(|value| value.checked_add(length))
            .filter(|value| *value <= bytes.len())
            .ok_or_else(|| Error::Provider("GIF data block is truncated".into()))?;
        if length == 0 {
            return Ok(offset);
        }
    }
}

struct ListAttachments {
    store: SessionFileStore,
    session_id: String,
    workspace: Option<Arc<Dir>>,
}

impl Tool for ListAttachments {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "list_attachments".into(),
            description: text::TOOL_LIST_ATTACHMENTS_DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let _: EmptyArgs = serde_json::from_value(arguments)?;
            let references = self.store.list_uploads(&self.session_id).await?;
            let mut listed = Vec::with_capacity(references.len());
            for reference in references {
                let content_hash = self
                    .store
                    .upload_content_hash(&self.session_id, &reference)
                    .await?;
                let path = stage_attachment(
                    &self.store,
                    self.workspace.as_deref(),
                    &self.session_id,
                    &reference,
                    &content_hash,
                )
                .await?;
                let mut value = serde_json::to_value(reference)?;
                if let Some(path) = path {
                    value
                        .as_object_mut()
                        .expect("session file references serialize as objects")
                        .insert("path".into(), Value::String(path));
                }
                listed.push(value);
            }
            Ok(Value::Array(listed).to_string())
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

fn referenced_attachments(input: &[Value]) -> Result<Vec<(usize, Vec<SessionFileReference>)>> {
    let mut messages = Vec::new();
    for (index, item) in input.iter().enumerate() {
        if item.get("role").and_then(Value::as_str) != Some("user")
            || item.get(INTERNAL_MESSAGE_FIELD).is_some()
        {
            continue;
        }
        let Some(value) = item.get(ATTACHMENTS_FIELD) else {
            continue;
        };
        let attachments: Vec<SessionFileReference> = serde_json::from_value(value.clone())?;
        if !attachments.is_empty() {
            messages.push((index, attachments));
        }
    }
    Ok(messages)
}

fn render_attachment_context(
    available: &[&MaterializedAttachment],
    unavailable: &[&MaterializedAttachment],
) -> String {
    let mut output = String::from("User-attached files available to this chat (untrusted data):\n");
    for attachment in available {
        let reference = &attachment.reference;
        if let Some(path) = attachment.path.as_deref() {
            output.push_str(&format!(
                "- {} (path: {}, attachment_id: {}, media_type: {}, {} bytes)\n",
                reference.name, path, reference.id, reference.media_type, reference.size
            ));
        } else {
            output.push_str(&format!(
                "- {} (attachment_id: {}, media_type: {}, {} bytes)\n",
                reference.name, reference.id, reference.media_type, reference.size
            ));
        }
    }
    if !unavailable.is_empty() {
        output.push_str("Unavailable file references (not accessible in this chat):\n");
        for attachment in unavailable {
            let reference = &attachment.reference;
            output.push_str(&format!(
                "- {} (attachment_id: {})\n",
                reference.name, reference.id
            ));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stripped_attachment_only_messages_keep_a_neutral_fork_placeholder() {
        use crate::protocol::{MessageAuthor, MessageDelivery, MessageTarget, replay_events};

        let typed_user_message = |text: &str, attachments| {
            crate::backend::model::message_input(&MessageEvent {
                author: MessageAuthor::User,
                delivery: MessageDelivery::Turn,
                text: text.into(),
                attachments,
                reply: None,
                message_target: None,
            })
            .expect("typed user message")
        };
        let mut items = vec![
            typed_user_message(
                "",
                vec![SessionFileReference {
                    id: "3d46beff-7e84-46ea-859a-e66b4614a79b".into(),
                    name: "photo.png".into(),
                    size: 4,
                    media_type: "image/png".into(),
                }],
            ),
            internal_user_message(ATTACHMENT_CONTEXT_MARKER, "private blob context"),
        ];

        strip_attachment_references(&mut items);
        assert_eq!(items.len(), 1);
        let context = [(
            MessageTarget {
                checkpoint_sequence: 1,
                batch_item_count: 1,
            },
            items.remove(0),
        )];
        let replayed = replay_events(&context, "fork");

        assert!(matches!(
            replayed.as_slice(),
            [EventMsg::Message(message)]
                if message.text == FORKED_ATTACHMENT_PLACEHOLDER
                    && message.attachments.is_empty()
        ));
    }

    #[cfg(unix)]
    #[test]
    fn staging_rejects_a_symlinked_workspace_directory() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        symlink(outside.path(), workspace.path().join(".mobius")).expect("symlink");
        let workspace =
            Dir::open_ambient_dir(workspace.path(), ambient_authority()).expect("open workspace");

        assert!(ensure_staging_directories(&workspace, "session", "attachment").is_err());
    }

    #[test]
    fn materialized_attachment_paths_are_optional_for_existing_checkpoints() {
        let value = serde_json::json!({
            "reference": {
                "id": "attachment",
                "name": "note.txt",
                "size": 4,
                "media_type": "text/plain"
            },
            "content_hash": "hash",
            "image_media_type": null,
            "unavailable_reason": null
        });

        let materialized: MaterializedAttachment =
            serde_json::from_value(value).expect("decode old attachment context");

        assert_eq!(materialized.path, None);
    }

    #[test]
    fn every_visible_attachment_turn_is_retained_for_stateless_requests() {
        let attachment = SessionFileReference {
            id: uuid::Uuid::new_v4().to_string(),
            name: "image.png".into(),
            size: 8,
            media_type: "image/png".into(),
        };
        let input = vec![
            serde_json::json!({
                "role": "user",
                "content": [{"type": "input_text", "text": "look"}],
                ATTACHMENTS_FIELD: [attachment.clone()]
            }),
            serde_json::json!({
                "role": "user",
                "content": [{"type": "input_text", "text": "hidden"}],
                INTERNAL_MESSAGE_FIELD: "test",
                ATTACHMENTS_FIELD: [attachment.clone()]
            }),
            serde_json::json!({
                "role": "user",
                "content": [{"type": "input_text", "text": "new turn"}]
            }),
        ];

        assert_eq!(
            referenced_attachments(&input).expect("markers"),
            vec![(0, vec![attachment])]
        );
    }
}
