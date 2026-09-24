//! User uploads exposed to the owning workspace.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cap_std::ambient_authority;
use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::manifest::MiddlewareManifest;
use super::tools::{Catalog, ExecutionMode, Tool, ToolContext, render_tool_event};
use super::{
    Middleware, ModelContext, PromptSection, RuntimeContext, SessionStartContext,
    SessionStartSource,
};
use crate::backend::model::{ToolDefinition, internal_user_message};
use crate::backend::session_files::{SessionFileStore, session_storage_key};
use crate::protocol::{
    ATTACHMENT_CONTEXT_MARKER, ATTACHMENTS_FIELD, EventMsg, FrontendBlock, FrontendContribution,
    INTERNAL_MESSAGE_FIELD, MESSAGE_METADATA_FIELD, SessionFileReference, internal_message_kind,
};
use crate::{BoxFuture, Error, Result};

mod text {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Definition {
        pub(super) default_enabled: bool,
        pub(super) manifest_description: String,
        pub(super) manifest_label: String,
        pub(super) prompt_main: String,
        pub(super) render_list_attachments: String,
        pub(super) tool_list_attachments_description: String,
    }
    pub(super) static DEFINITION: std::sync::LazyLock<Definition> =
        std::sync::LazyLock::new(|| {
            toml::from_str(include_str!("attachments.toml"))
                .expect("bundled attachments definition must be valid")
        });
}
const MATERIALIZED_ATTACHMENTS_FIELD: &str = "_mobius_attachment_blobs";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MaterializedAttachment {
    reference: SessionFileReference,
    content_hash: Option<String>,
    image: Option<crate::protocol::ImageReference>,
    #[serde(default)]
    path: Option<String>,
    unavailable_reason: Option<String>,
}
/// Configuration metadata for protected user uploads.
pub static MANIFEST: std::sync::LazyLock<MiddlewareManifest> =
    std::sync::LazyLock::new(|| MiddlewareManifest {
        id: "attachments",
        label: text::DEFINITION.manifest_label.as_str(),
        description: text::DEFINITION.manifest_description.as_str(),
        required: false,
        default_enabled: text::DEFINITION.default_enabled,
        required_model_capability: None,
        settings: &[],
    });

/// Optional middleware exposing user uploads to the owning workspace.
#[derive(Clone)]
pub struct Attachments {
    store: SessionFileStore,
    workspace: Option<Arc<Dir>>,
    workspace_path: Option<PathBuf>,
}

impl Attachments {
    #[must_use]
    /// Creates a new instance.
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
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
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
    fn prepare_compacted_input(&self, original: &[Value], compacted: &mut Vec<Value>) {
        // ponytail: scan the retained context; index message identities if large histories make this costly.
        for index in (0..compacted.len()).rev() {
            let item = &compacted[index];
            if item.get("role").and_then(Value::as_str) != Some("user") {
                continue;
            }
            let materialization = original
                .windows(2)
                .rfind(|pair| {
                    pair[0].get("role") == item.get("role")
                        && pair[0].get("content") == item.get("content")
                        && pair[0].get(MESSAGE_METADATA_FIELD) == item.get(MESSAGE_METADATA_FIELD)
                        && is_attachment_materialization(&pair[1])
                })
                .map(|pair| &pair[1]);
            restore_attachment_materialization(compacted, index, materialization);
        }
    }

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
            let (Some(workspace), Some(workspace_path)) =
                (self.workspace.as_deref(), self.workspace_path.as_deref())
            else {
                return Ok(());
            };
            self.store
                .register_attachment_workspace(
                    &context.runtime.session_id,
                    workspace,
                    workspace_path,
                )
                .await
        })
    }

    fn prompt_section(&self, _runtime: &RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(Some(PromptSection::new(
            text::DEFINITION.prompt_main.as_str(),
        )))
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
                    text::DEFINITION.render_list_attachments.as_str().into()
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
                return Ok(());
            }
            if message_index + 1 != context.input().len() {
                return Err(Error::Checkpoint(
                    "attachment-bearing user message is missing adjacent materialization".into(),
                ));
            }
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
                            image: None,
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
                            image: None,
                            path: None,
                            unavailable_reason: Some(reason),
                        });
                        continue;
                    }
                };
                let image = if reference.media_type.starts_with("image/") {
                    match self
                        .store
                        .inspect_image(
                            context.session_id,
                            &reference,
                            crate::protocol::ImageDetail::Auto,
                        )
                        .await
                    {
                        Ok(image) => Some(image),
                        Err(error) => {
                            let reason = error.to_string();
                            if first_error.is_none() {
                                first_error = Some(error);
                            }
                            materialized.push(MaterializedAttachment {
                                reference,
                                content_hash: Some(content_hash),
                                image: None,
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
                    image,
                    path,
                    unavailable_reason: None,
                });
            }
            context.append_model_input(materialization_message(&materialized)?);
            first_error.map_or(Ok(()), Err)
        })
    }
}

fn restore_attachment_materialization(
    compacted: &mut Vec<Value>,
    user_index: usize,
    materialization: Option<&Value>,
) {
    let Some(materialization) = materialization else {
        return;
    };
    match compacted.get(user_index + 1) {
        Some(retained) if retained == materialization => {}
        Some(retained) if is_attachment_materialization(retained) => {
            compacted[user_index + 1] = materialization.clone();
        }
        Some(_) | None => compacted.insert(user_index + 1, materialization.clone()),
    }
}

fn is_attachment_materialization(item: &Value) -> bool {
    internal_message_kind(item) == Some(ATTACHMENT_CONTEXT_MARKER)
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
    for attachment in attachments {
        if let Some(image) = &attachment.image {
            message["content"]
                .as_array_mut()
                .ok_or_else(|| Error::Checkpoint("attachment content is not an array".into()))?
                .push(serde_json::to_value(crate::protocol::ContentPart::Image {
                    image: image.clone(),
                })?);
        }
    }
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

struct ListAttachments {
    store: SessionFileStore,
    session_id: String,
    workspace: Option<Arc<Dir>>,
}

impl Tool for ListAttachments {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "list_attachments".into(),
            description: text::DEFINITION.tool_list_attachments_description.clone(),
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
    ) -> BoxFuture<'a, Result<crate::protocol::ToolResponse>> {
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
            Ok((Value::Array(listed).to_string()).into())
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
    fn compaction_restores_each_retained_messages_media_without_duplicates() {
        let user = |id| {
            serde_json::json!({
                "role": "user", "content": "inspect",
                (MESSAGE_METADATA_FIELD): {"id": id}
            })
        };
        let first = user("first");
        let second = user("second");
        let first_media = internal_user_message(ATTACHMENT_CONTEXT_MARKER, "first image");
        let second_media = internal_user_message(ATTACHMENT_CONTEXT_MARKER, "second image");
        let original = vec![
            first.clone(),
            first_media.clone(),
            second.clone(),
            second_media.clone(),
        ];
        let marker = serde_json::json!({"type": "compaction", "encrypted_content": "opaque"});
        let mut compacted = vec![
            first.clone(),
            second_media.clone(),
            marker.clone(),
            second.clone(),
        ];
        let directory = tempfile::tempdir().expect("files");
        let attachments = Attachments::new(SessionFileStore::new(directory.path()));
        attachments.prepare_compacted_input(&original, &mut compacted);
        attachments.prepare_compacted_input(&original, &mut compacted);
        assert_eq!(
            compacted,
            vec![first, first_media, marker, second, second_media]
        );
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
    fn unavailable_attachment_can_omit_its_workspace_path() {
        let value = serde_json::json!({
            "reference": {
                "id": "attachment",
                "name": "note.txt",
                "size": 4,
                "media_type": "text/plain"
            },
            "content_hash": "hash",
            "image": null,
            "unavailable_reason": null
        });

        let materialized: MaterializedAttachment =
            serde_json::from_value(value).expect("decode attachment context");

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
