use mobius::SessionId;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mobius::protocol::SessionFileReference;

use super::thumbnails::ThumbnailKey;

use super::*;

pub(in crate::ui) struct DraftAttachment {
    pub id: Arc<str>,
    pub name: String,
    pub file: Option<SessionFileReference>,
    /// One of the chat's own files, added by reference: removing it only forgets it.
    pub(super) linked: bool,
    pub(super) cleanup_file_id: Option<String>,
    pub error: Option<String>,
    pub(super) cancellation: Arc<AtomicBool>,
}

/// What removing a draft attachment asks of the gateway.
#[derive(Debug, PartialEq)]
pub(super) enum Removal {
    /// A finished upload, or one whose finish may have committed: delete it by id.
    Delete(String),
    /// An upload in flight: its own connection deletes it at the next chunk boundary.
    Cancel,
    /// Nothing reached the gateway, so the tile just goes.
    Forget,
    /// A removal is already under way.
    Pending,
}

impl DraftAttachment {
    /// Removal asked for and not yet refused: the tile reads "Removing…".
    pub(in crate::ui) fn removing(&self) -> bool {
        self.cancellation.load(Ordering::Relaxed) && self.error.is_none()
    }

    pub(super) fn removal(&self) -> Removal {
        if self.removing() {
            return Removal::Pending;
        }
        if self.linked {
            return Removal::Forget;
        }
        match self.file.as_ref().map(|file| &file.id) {
            Some(id) => Removal::Delete(id.clone()), // rules-allow: the delete request owns the id; the tile keeps it for a retry
            None => match &self.cleanup_file_id {
                Some(id) => Removal::Delete(id.clone()), // rules-allow: the delete request owns the id; the tile keeps it for a retry
                None if self.error.is_some() => Removal::Forget,
                None => Removal::Cancel,
            },
        }
    }
}

impl Workspace {
    pub(in crate::ui) fn can_attach_files(&self) -> bool {
        self.composer_bot()
            .is_some_and(|bot| bot.capabilities.attachments)
            && (self.attachment_target().is_some()
                || (self.client.link.is_ready()
                    && self.draft_selected()
                    && self.queued_draft.is_none()))
    }

    pub(in crate::ui) fn draft_attachments(&self) -> &[DraftAttachment] {
        self.runtime
            .draft_owner
            .as_ref()
            .and_then(|key| self.runtime.attachments.get(key))
            .map_or_default(Vec::as_slice)
    }

    pub(in crate::ui) fn attachments_ready(&self) -> bool {
        self.draft_attachments()
            .iter()
            .all(|item| item.file.is_some() && !item.cancellation.load(Ordering::Relaxed))
    }

    pub(in crate::ui) fn attachment_target(&self) -> Option<String> {
        let session_id = self.client.state.selected_session_id.as_ref()?;
        (self.client.link.is_ready() && self.active_session_ready())
            .then(|| self.client.draft_key(session_id))
    }

    pub(in crate::ui) fn attach_files(
        &mut self,
        target: String,
        paths: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if self.attachment_target().as_ref() != Some(&target) {
            self.report("The chat changed while choosing attachments. Choose the files again.");
            cx.notify();
            return;
        }
        let (Some(gateway), Some(session_id), Some(state)) = (
            self.client.gateway.as_ref(),
            self.client
                .state
                .selected_session_id
                .as_ref()
                .map(SessionId::share),
            self.client.state.gateway.as_ref(),
        ) else {
            return;
        };
        let maximum = state.session_file_limits.max_attachment_references;
        let attachments = self.runtime.attachments.entry(target).or_default();
        if attachments.len().saturating_add(paths.len()) > maximum {
            self.report(format!(
                "A message can include up to {maximum} attachments."
            ));
            cx.notify();
            return;
        }
        let mut drawable = Vec::new();
        for path in paths {
            let id = gateway::request_id();
            let media_type = mime_guess::from_path(&path).first_or_octet_stream();
            if super::drawable(media_type.essence_str()) {
                drawable.push((Arc::clone(&id), path.clone())); // rules-allow: the thumbnail and the upload each read the file from their own path
            }
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let (cancellation, error) =
                match gateway.upload(Arc::clone(&id), session_id.share(), path) {
                    Ok(cancellation) => (cancellation, None),
                    Err(error) => (Default::default(), Some(gateway::describe(&error))),
                };
            attachments.push(DraftAttachment {
                id,
                name,
                file: None,
                linked: false,
                cleanup_file_id: None,
                error,
                cancellation,
            });
        }
        // SwiftUI draws the tile from the chosen file itself, before the upload finishes.
        for (id, path) in drawable {
            self.draw_thumbnail(ThumbnailKey::Draft(id), path, false, cx);
        }
        cx.notify();
    }

    /// Attaches files dropped or pasted onto the chat, starting the draft's chat first when
    /// it has none yet, as the attach button does.
    pub(in crate::ui) fn attach_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if let Some(target) = self.attachment_target() {
            self.attach_files(target, paths, cx);
        } else if self.can_attach_files() {
            self.create_draft(DraftIntent::Attachments(paths), cx);
        } else {
            self.report("Connect to a gateway and select a Bot to attach files.");
            cx.notify();
        }
    }

    /// Adds one of the chat's own files, such as one the Bot sent, without uploading it
    /// again; removing it leaves the chat's copy in place.
    pub(in crate::ui) fn attach_chat_file(
        &mut self,
        file: SessionFileReference,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.attachment_target() else {
            self.report("Open this chat to attach its files.");
            return;
        };
        let attachments = self.runtime.attachments.entry(target).or_default();
        if attachments.iter().any(|item| {
            item.file
                .as_ref()
                .is_some_and(|attached| attached.id == file.id)
        }) {
            return;
        }
        attachments.push(DraftAttachment {
            id: gateway::request_id(),
            name: file.name.clone(), // rules-allow: the tile names itself apart from the file reference it sends
            file: Some(file),
            linked: true,
            cleanup_file_id: None,
            error: None,
            cancellation: Arc::default(),
        });
        cx.notify();
    }

    pub(in crate::ui) fn remove_attachment(&mut self, id: &str, cx: &mut Context<Self>) {
        let (Some(key), Some(session_id)) = (
            self.runtime.draft_owner.clone(), // rules-allow: discarding borrows the runtime mutably
            self.client
                .state
                .selected_session_id
                .as_ref()
                .map(SessionId::share),
        ) else {
            return;
        };
        if self.client.attachment_pending(&key, id) {
            self.report("This attachment is part of a message awaiting confirmation.");
        } else {
            self.discard_attachment(&key, &session_id, id);
        }
        cx.notify();
    }

    /// Removes a draft attachment from the gateway as well as the composer, as SwiftUI's
    /// discardComposerAttachment does; the tile stays, marked, until the gateway confirms.
    fn discard_attachment(&mut self, key: &str, session_id: &SessionId, id: &str) {
        let Some(items) = self.runtime.attachments.get_mut(key) else {
            return;
        };
        let Some(item) = items.iter_mut().find(|item| *item.id == *id) else {
            return;
        };
        match item.removal() {
            Removal::Pending => {}
            Removal::Cancel => item.cancellation.store(true, Ordering::Relaxed),
            Removal::Forget => {
                items.retain(|item| *item.id != *id);
                self.runtime.thumbnails.forget_draft(id);
            }
            Removal::Delete(file_id) => {
                let result = self
                    .client
                    .gateway
                    .as_ref()
                    .filter(|_| self.client.link.is_ready())
                    .ok_or_else(|| "Reconnect to this gateway to remove the attachment.".to_owned())
                    .and_then(|gateway| {
                        gateway
                            .delete_attachment(Arc::clone(&item.id), session_id.share(), file_id)
                            .map_err(|error| gateway::describe(&error))
                    });
                item.cancellation.store(result.is_ok(), Ordering::Relaxed);
                item.error = result.err();
            }
        }
    }

    pub(super) fn uploaded(
        &mut self,
        endpoint: &str,
        session_id: &SessionId,
        id: &str,
        result: Result<Option<SessionFileReference>, gateway::UploadFailure>,
    ) {
        let key = format!("{}:{}{}", endpoint.len(), endpoint, session_id);
        let Some(items) = self.runtime.attachments.get_mut(&key) else {
            return;
        };
        let Some(item) = items.iter_mut().find(|item| *item.id == *id) else {
            return;
        };
        // Removed while uploading: whatever reached the gateway is deleted now.
        let removed = item.cancellation.swap(false, Ordering::Relaxed);
        match result {
            Ok(Some(file)) => {
                self.runtime.thumbnails.promote(id, session_id, &file.id);
                item.file = Some(file);
                item.cleanup_file_id = None;
                item.error = None;
            }
            Ok(None) => {
                items.retain(|item| *item.id != *id);
                self.runtime.thumbnails.forget_draft(id);
                return;
            }
            Err(error) => {
                item.cleanup_file_id = error.cleanup_file_id;
                item.error = Some(error.message);
            }
        }
        if !removed {
            return;
        }
        if item.file.is_none() && item.cleanup_file_id.is_none() {
            items.retain(|item| *item.id != *id);
            self.runtime.thumbnails.forget_draft(id);
        } else if endpoint == self.client.endpoint && self.client.link.is_ready() {
            item.error = None;
            self.discard_attachment(&key, session_id, id);
        } else {
            item.error = Some("Reconnect to this gateway to remove the attachment.".into());
        }
    }

    pub(super) fn attachment_deleted(
        &mut self,
        endpoint: &str,
        session_id: &SessionId,
        id: &str,
        result: Result<(), String>,
    ) {
        let key = format!("{}:{}{}", endpoint.len(), endpoint, session_id);
        let Some(items) = self.runtime.attachments.get_mut(&key) else {
            return;
        };
        match result {
            Ok(()) => {
                items.retain(|item| *item.id != *id);
                self.runtime.thumbnails.forget_draft(id);
            }
            Err(error) => {
                if let Some(item) = items.iter_mut().find(|item| *item.id == *id) {
                    item.cancellation.store(false, Ordering::Relaxed);
                    item.error = Some(error);
                }
            }
        }
    }

    pub(super) fn interrupt_uploads(&mut self) {
        for attachments in self.runtime.attachments.values_mut() {
            for item in attachments {
                if item.file.is_none() && item.error.is_none() {
                    item.cancellation.store(true, Ordering::Relaxed);
                }
            }
        }
    }
}

#[cfg(feature = "snapshot")]
type FixtureAttachment<'a> = (
    &'a str,
    Option<SessionFileReference>,
    Option<&'a str>,
    Option<PathBuf>,
);

#[cfg(feature = "snapshot")]
impl Workspace {
    /// Lays draft attachments over the open chat's composer without uploading: each is
    /// `(name, uploaded file, error, source image)`, the image drawn by the real pipeline.
    pub(in crate::ui) fn fixture_attachments(
        &mut self,
        items: Vec<FixtureAttachment<'_>>,
        cx: &mut Context<Self>,
    ) {
        if self.runtime.draft_owner.is_none() {
            return;
        }
        let mut drafted = Vec::with_capacity(items.len());
        for (index, (name, file, error, source)) in items.into_iter().enumerate() {
            let id: Arc<str> = format!("fixture-draft-{index}").into();
            if let Some(source) = source {
                self.draw_thumbnail(ThumbnailKey::Draft(Arc::clone(&id)), source, false, cx);
            }
            drafted.push(DraftAttachment {
                id,
                name: name.into(),
                file,
                linked: false,
                cleanup_file_id: None,
                error: error.map(str::to_owned),
                cancellation: Default::default(),
            });
        }
        if let Some(key) = &self.runtime.draft_owner {
            self.runtime
                .attachments
                .entry(key.clone()) // rules-allow: the attachments map and the draft owner each hold the chat's key
                .or_default()
                .extend(drafted);
        }
        cx.notify();
    }

    pub(in crate::ui) fn clear_fixture_attachments(&mut self) {
        for items in self.runtime.attachments.values_mut() {
            items.retain(|item| !item.id.starts_with("fixture-draft-"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DraftAttachment, Removal};
    use mobius::protocol::SessionFileReference;
    use std::sync::atomic::Ordering;

    fn draft(file: Option<&str>, cleanup: Option<&str>, error: Option<&str>) -> DraftAttachment {
        DraftAttachment {
            id: "draft".into(),
            name: "photo.png".into(),
            linked: false,
            file: file.map(|id| SessionFileReference {
                id: id.into(),
                name: "photo.png".into(),
                media_type: "image/png".into(),
                size: 4,
            }),
            cleanup_file_id: cleanup.map(str::to_owned),
            error: error.map(str::to_owned),
            cancellation: Default::default(),
        }
    }

    #[test]
    fn removing_an_attachment_deletes_whatever_reached_the_gateway() {
        // Uploaded: delete the stored file by its id.
        assert_eq!(
            draft(Some("file"), None, None).removal(),
            Removal::Delete("file".into())
        );
        // A finish whose acknowledgement was lost may have committed: delete that id too.
        assert_eq!(
            draft(None, Some("upload"), Some("timed out")).removal(),
            Removal::Delete("upload".into())
        );
        // A refused delete can be retried.
        assert_eq!(
            draft(Some("file"), None, Some("offline")).removal(),
            Removal::Delete("file".into())
        );
        // In flight: the upload's own connection sends the delete.
        assert_eq!(draft(None, None, None).removal(), Removal::Cancel);
        // Failed before the gateway kept anything.
        assert_eq!(
            draft(None, None, Some("too large")).removal(),
            Removal::Forget
        );
        let removing = draft(Some("file"), None, None);
        removing.cancellation.store(true, Ordering::Relaxed);
        assert!(removing.removing());
        assert_eq!(removing.removal(), Removal::Pending, "no second delete");
    }
}
