//! Attachment thumbnails drawn on this Mac, as ChatSessionFiles.swift draws them: a draft's
//! from the file the person chose, a sent or received image's from a download, both
//! downsampled by ImageIO.

use gpui::{ImgResourceLoader, Resource};
use mobius::SessionId;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mobius::protocol::SessionFileReference;

use super::*;

/// ChatSessionFiles.swift's bound on a downloaded thumbnail source.
const MAX_SOURCE_BYTES: u64 = 48 * 1024 * 1024;
/// Tiles are at most 320 points wide: two pixels per point.
const MAX_PIXELS: u32 = 640;
/// Thumbnail downloads at once; SwiftUI runs one, the transfer pool bounds the rest.
const MAX_DOWNLOADS: usize = 2;

#[derive(Clone, Debug, PartialEq)]
pub(in crate::ui) struct Thumbnail {
    pub path: Arc<Path>,
    pub width: u32,
    pub height: u32,
    /// When it became available, for the reveal of a picture that just arrived.
    pub arrived: std::time::Instant,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::ui) enum ThumbnailKey {
    /// A composer attachment, by its draft id.
    Draft(Arc<str>),
    File {
        session: SessionId,
        file: Arc<str>,
    },
}

impl ThumbnailKey {
    pub(in crate::ui) fn file(session: &SessionId, file: &str) -> Self {
        Self::File {
            session: session.share(),
            file: file.into(),
        }
    }
}

#[derive(Default)]
pub(in crate::ui) struct Thumbnails {
    ready: BTreeMap<ThumbnailKey, Thumbnail>,
    /// Drawn, downloading or failed: never asked for again.
    requested: BTreeSet<ThumbnailKey>,
    failed: BTreeSet<ThumbnailKey>,
    /// Images the transcript drew without a thumbnail, noted during render.
    wanted: RefCell<Vec<(SessionId, SessionFileReference)>>,
    queued: VecDeque<(SessionId, SessionFileReference)>,
    downloads: usize,
    /// Invalidates file draws on gateway changes, while reconnects keep the same cache.
    file_generation: u64,
    /// Files no longer referenced by any draft or open transcript, removed off-thread.
    discarded: Vec<Arc<Path>>,
    /// Chats recently shown, newest first, whose thumbnails and parsed text stay cached
    /// after they close so switching back redraws nothing.
    recent: VecDeque<SessionId>,
}

/// How many recently shown chats keep their thumbnails and parsed text.
const RECENT_CHATS: usize = 4;

impl Thumbnails {
    pub(in crate::ui) fn cache_paths(&self) -> BTreeSet<Arc<Path>> {
        self.ready
            .iter()
            .filter(|(key, _)| matches!(key, ThumbnailKey::File { .. }))
            .map(|(_, thumbnail)| Arc::clone(&thumbnail.path))
            .collect()
    }

    pub(in crate::ui) fn clear_files(&mut self) {
        self.recent.clear();
        self.retain_sessions(&BTreeSet::new());
        self.downloads = 0;
        self.file_generation = self.file_generation.wrapping_add(1);
    }

    pub(in crate::ui) fn get(&self, key: &ThumbnailKey) -> Option<&Thumbnail> {
        self.ready.get(key)
    }

    /// A sent draft's thumbnail also shows on the message that carries its file.
    pub(super) fn promote(&mut self, draft: &str, session: &SessionId, file: &str) {
        let Some(thumbnail) = self.ready.get(&ThumbnailKey::Draft(draft.into())) else {
            return;
        };
        let thumbnail = Thumbnail::clone(thumbnail);
        let key = ThumbnailKey::file(session, file);
        self.requested.insert(ThumbnailKey::clone(&key));
        if let Some(old) = self.ready.insert(key, thumbnail) {
            self.discard(old);
        }
    }

    fn complete(&mut self, key: ThumbnailKey, generation: u64, thumbnail: Thumbnail) -> bool {
        if !self.requested.contains(&key)
            || (matches!(key, ThumbnailKey::File { .. }) && generation != self.file_generation)
        {
            self.discard(thumbnail);
            return false;
        }
        self.failed.remove(&key);
        if let Some(old) = self.ready.insert(key, thumbnail) {
            self.discard(old);
        }
        true
    }

    pub(super) fn forget_draft(&mut self, draft: &str) {
        let key = ThumbnailKey::Draft(draft.into());
        self.requested.remove(&key);
        self.failed.remove(&key);
        if let Some(thumbnail) = self.ready.remove(&key) {
            self.discard(thumbnail);
        }
    }

    fn discard(&mut self, thumbnail: Thumbnail) {
        if !self.ready.values().any(|kept| kept.path == thumbnail.path) {
            self.discarded.push(thumbnail.path);
            crate::host::platform::wake();
        }
    }

    pub(in crate::ui) fn forget_chats(&mut self, ids: &[SessionId]) {
        self.recent.retain(|id| !ids.contains(id));
    }

    /// One retention policy for files and parsed text: open/recent chats plus documents.
    fn retain<'a>(
        &mut self,
        open: impl Iterator<Item = &'a SessionId>,
        documents: impl Iterator<Item = &'a SessionId>,
    ) -> BTreeSet<SessionId> {
        for session in open {
            self.recent.retain(|recent| recent != session);
            self.recent.push_front(session.share());
        }
        self.recent.truncate(RECENT_CHATS);
        let sessions: BTreeSet<_> = self
            .recent
            .iter()
            .map(SessionId::share)
            .chain(documents.map(SessionId::share))
            .collect();
        self.retain_sessions(&sessions);
        sessions
    }

    fn retain_sessions(&mut self, sessions: &BTreeSet<SessionId>) {
        let keep = |key: &ThumbnailKey| match key {
            ThumbnailKey::Draft(_) => true,
            ThumbnailKey::File { session, .. } => sessions.contains(session),
        };
        self.requested.retain(&keep);
        self.failed.retain(&keep);
        self.queued
            .retain(|(session, _)| sessions.contains(session));
        self.wanted
            .get_mut()
            .retain(|(session, _)| sessions.contains(session));
        let removed: Vec<_> = self
            .ready
            .extract_if(.., |key, _| !keep(key))
            .map(|(_, thumbnail)| thumbnail)
            .collect();
        for thumbnail in removed {
            self.discard(thumbnail);
        }
    }
}

/// SwiftUI draws images and videos it has on hand, and downloads only images.
/// A fresh path in `directory` for a copy of the file `name`, keeping a plain extension
/// so native apps recognize it.
pub(in crate::ui) fn private_path(directory: &Path, name: &str) -> PathBuf {
    let extension = Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| ext.len() <= 12 && ext.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("bin");
    directory.join(format!("{}.{extension}", uuid::Uuid::new_v4()))
}

pub(in crate::ui) fn drawable(media_type: &str) -> bool {
    let media_type = media_type.to_ascii_lowercase();
    media_type.starts_with("image/") || media_type.starts_with("video/")
}

fn downloadable(file: &SessionFileReference) -> bool {
    file.media_type.to_ascii_lowercase().starts_with("image/") && file.size <= MAX_SOURCE_BYTES
}

impl Workspace {
    pub(super) fn thumbnail_result(
        &mut self,
        endpoint: &str,
        session_id: SessionId,
        file_id: String,
        result: Result<PathBuf, String>,
        cx: &mut Context<Self>,
    ) {
        if endpoint == self.client.endpoint {
            self.thumbnail_source(session_id, file_id, result, cx);
        } else if let Ok(path) = result {
            cx.background_executor()
                .spawn(async move { crate::gateway::local::discard(&path) })
                .detach();
        }
    }

    /// Keep cached files and parsed text for open chats and documents, including hidden tabs
    /// and unsent drafts, and for the last few chats shown.
    pub(in crate::ui) fn prune_thumbnails(&mut self) {
        let sessions = self.runtime.thumbnails.retain(
            self.page_tabs.iter().filter_map(|page| match page {
                PageTab::Chat(id) => Some(id),
                _ => None,
            }),
            self.content
                .navigation
                .sessions()
                .chain(
                    self.content
                        .tabs
                        .values()
                        .filter_map(|tab| match &tab.content {
                            TabContent::Preview { session_id, .. } => Some(session_id),
                            TabContent::Routine(preview) => preview.source.run.session_id.as_ref(),
                            _ => None,
                        }),
                ),
        );
        self.chat_ui.markdown.get_mut().retain(|_, view| {
            view.session
                .as_ref()
                .is_none_or(|session| sessions.contains(session))
        });
    }

    pub(super) fn reap_thumbnails(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let discarded = std::mem::take(&mut self.runtime.thumbnails.discarded);
        // GPUI keeps every image it decodes, by path, until told otherwise; a discarded
        // thumbnail's path never returns, so its pixels and atlas tile go with it.
        for path in &discarded {
            let resource = Resource::Path(Arc::clone(path));
            if let Some(Ok(image)) = cx.take_asset::<ImgResourceLoader>(&resource) {
                cx.drop_image(image, Some(window));
            }
        }
        if !discarded.is_empty() {
            cx.background_executor()
                .spawn(async move {
                    for path in discarded {
                        crate::gateway::local::discard(&path);
                    }
                })
                .detach();
        }
    }

    /// The thumbnail for a file in the transcript; asks for one the first time it shows.
    pub(in crate::ui) fn file_thumbnail(
        &self,
        session: &SessionId,
        file: &SessionFileReference,
    ) -> Option<Thumbnail> {
        let thumbnails = &self.runtime.thumbnails;
        let key = ThumbnailKey::file(session, &file.id);
        if let Some(thumbnail) = thumbnails.get(&key) {
            return Some(Thumbnail::clone(thumbnail));
        }
        if downloadable(file) && !thumbnails.requested.contains(&key) {
            let mut wanted = thumbnails.wanted.borrow_mut();
            if !wanted.iter().any(|(s, f)| s == session && f.id == file.id) {
                wanted.push((session.share(), file.clone())); // rules-allow: the download queue owns the reference the transcript draws
                crate::host::platform::wake();
            }
        }
        None
    }

    pub(in crate::ui) fn draft_thumbnail(&self, id: &str) -> Option<Thumbnail> {
        let thumbnail = self
            .runtime
            .thumbnails
            .get(&ThumbnailKey::Draft(id.into()))?;
        Some(Thumbnail::clone(thumbnail))
    }

    pub(in crate::ui) fn thumbnail_pending(
        &self,
        session: &SessionId,
        file: &SessionFileReference,
    ) -> bool {
        downloadable(file)
            && self.downloads.is_some()
            && !self
                .runtime
                .thumbnails
                .failed
                .contains(&ThumbnailKey::file(session, &file.id))
    }

    /// Draws the thumbnail for `key` from `source` off the main thread; a downloaded source
    /// is removed once drawn.
    pub(in crate::ui) fn draw_thumbnail(
        &mut self,
        key: ThumbnailKey,
        source: PathBuf,
        downloaded: bool,
        cx: &Context<Self>,
    ) {
        let Some(directory) = self.downloads.as_ref().map(|dir| dir.path().to_owned()) else {
            return;
        };
        let thumbnails = &mut self.runtime.thumbnails;
        thumbnails.requested.insert(ThumbnailKey::clone(&key));
        let generation = thumbnails.file_generation;
        let destination = directory.join(format!("{}.png", uuid::Uuid::new_v4()));
        let draw = cx.background_executor().spawn(async move {
            let (width, height) = crate::host::platform::draw_thumbnail(
                &source,
                &destination,
                MAX_PIXELS,
                downloaded,
            )?;
            Some(Thumbnail {
                arrived: std::time::Instant::now(),
                path: destination.into(),
                width,
                height,
            })
        });
        cx.spawn(async move |this, cx| {
            let Some(thumbnail) = draw.await else {
                if let Err(error) = this.update(cx, |this, cx| {
                    let thumbnails = &mut this.runtime.thumbnails;
                    if generation == thumbnails.file_generation
                        && thumbnails.requested.contains(&key)
                    {
                        thumbnails.failed.insert(key);
                        cx.notify();
                    }
                }) {
                    tracing::debug!(%error, "the workspace closed before a failed thumbnail was noted");
                }
                return;
            };
            let mut thumbnail = Some(thumbnail);
            if let Err(error) = this.update(cx, |this, cx| {
                let Some(thumbnail) = thumbnail.take() else {
                    return;
                };
                if this.runtime.thumbnails.complete(key, generation, thumbnail) {
                    cx.notify();
                    crate::host::platform::wake();
                }
            }) {
                tracing::debug!(%error, "the workspace closed before a thumbnail arrived");
            }
            if let Some(thumbnail) = thumbnail {
                cx.background_executor()
                    .spawn(async move { crate::gateway::local::discard(&thumbnail.path) })
                    .detach();
            }
        })
        .detach();
    }

    /// Starts downloads for images the transcript asked about since the last poll.
    pub(super) fn request_thumbnails(&mut self) {
        let thumbnails = &mut self.runtime.thumbnails;
        for (session, file) in thumbnails.wanted.get_mut().drain(..) {
            if thumbnails
                .requested
                .insert(ThumbnailKey::file(&session, &file.id))
            {
                thumbnails.queued.push_back((session, file));
            }
        }
        let (Some(gateway), Some(directory)) =
            (self.client.gateway.as_ref(), self.downloads.as_ref())
        else {
            return;
        };
        if !self.client.link.is_ready() {
            return;
        }
        while thumbnails.downloads < MAX_DOWNLOADS
            && let Some((session, file)) = thumbnails.queued.pop_front()
        {
            let destination = private_path(directory.path(), &file.name);
            let key = ThumbnailKey::file(&session, &file.id);
            if gateway
                .download_thumbnail(session, file, destination)
                .is_ok()
            {
                thumbnails.downloads += 1;
            } else {
                thumbnails.requested.remove(&key);
            }
        }
    }

    pub(super) fn thumbnail_source(
        &mut self,
        session: SessionId,
        file: String,
        result: Result<PathBuf, String>,
        cx: &mut Context<Self>,
    ) {
        let thumbnails = &mut self.runtime.thumbnails;
        thumbnails.downloads = thumbnails.downloads.saturating_sub(1);
        let key = ThumbnailKey::File {
            session,
            file: file.into(),
        };
        if !thumbnails.requested.contains(&key) {
            if let Ok(path) = result {
                thumbnails.discarded.push(path.into());
            }
            return;
        }
        match result {
            Ok(path) => self.draw_thumbnail(key, path, true, cx),
            // A dropped connection asks again when the file next shows; a refusal does not.
            Err(_) if !self.client.link.is_ready() => {
                thumbnails.requested.remove(&key);
            }
            Err(_) => {
                thumbnails.failed.insert(key);
                cx.notify();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Thumbnail, ThumbnailKey, Thumbnails};
    use crate::ui::runtime::RuntimeState;
    use std::collections::BTreeSet;
    use std::path::Path;

    fn thumbnail(path: &str) -> Thumbnail {
        Thumbnail {
            arrived: std::time::Instant::now(),
            path: Path::new(path).into(),
            width: 4,
            height: 3,
        }
    }

    #[test]
    fn reconnects_keep_file_draws_but_gateway_changes_reject_stale_completions() {
        let mut runtime = RuntimeState::default();
        let session = "chat".parse().unwrap();
        let key = ThumbnailKey::file(&session, "file");
        runtime.thumbnails.requested.insert(key.clone());
        assert_eq!(
            runtime
                .thumbnails
                .retain(std::iter::once(&session), std::iter::empty()),
            BTreeSet::from([session.share()])
        );
        let generation = runtime.thumbnails.file_generation;

        // An explicit reconnect leaves the cache as it is.
        assert!(runtime.thumbnails.complete(
            key.clone(),
            generation,
            thumbnail("/tmp/reconnected.png")
        ));
        assert!(runtime.thumbnails.get(&key).is_some());

        runtime.thumbnails.clear_files();
        assert!(
            runtime.thumbnails.recent.is_empty(),
            "a gateway change must forget recent cache owners"
        );
        // A new gateway may request the same key before the previous draw finishes.
        runtime.thumbnails.requested.insert(key.clone());
        assert!(
            !runtime
                .thumbnails
                .complete(key.clone(), generation, thumbnail("/tmp/stale.png"))
        );
        assert!(runtime.thumbnails.get(&key).is_none());
        assert!(runtime.thumbnails.requested.contains(&key));
        assert!(
            runtime
                .thumbnails
                .discarded
                .contains(&Path::new("/tmp/stale.png").into())
        );
        assert!(runtime.thumbnails.complete(
            key.clone(),
            runtime.thumbnails.file_generation,
            thumbnail("/tmp/current.png")
        ));
        assert_eq!(
            runtime.thumbnails.get(&key).unwrap().path.as_ref(),
            Path::new("/tmp/current.png")
        );
    }

    /// Switching between chats keeps both chats' pictures: nothing is discarded, so
    /// nothing is downloaded, drawn or decoded again. A chat older than the recent few
    /// lets its pictures go.
    #[test]
    fn switching_chats_keeps_recent_pictures_and_lets_old_ones_go() {
        let mut thumbnails = Thumbnails::default();
        let chats: Vec<mobius::SessionId> = (0..6)
            .map(|index| format!("chat-{index}").parse().unwrap())
            .collect();
        let show = |thumbnails: &mut Thumbnails, chat: &mobius::SessionId| {
            thumbnails.retain(std::iter::once(chat), std::iter::empty());
        };
        // Each chat's picture arrives while it shows.
        for (index, chat) in chats.iter().take(2).enumerate() {
            show(&mut thumbnails, chat);
            let key = ThumbnailKey::file(chat, "picture");
            thumbnails.requested.insert(key.clone());
            let generation = thumbnails.file_generation;
            assert!(
                thumbnails.complete(key, generation, thumbnail(&format!("/tmp/{index}.png"))),
                "fixture thumbnail must complete for its current generation"
            );
        }
        show(&mut thumbnails, &chats[0]);
        show(&mut thumbnails, &chats[1]);
        assert!(
            thumbnails.discarded.is_empty(),
            "{:?}",
            thumbnails.discarded
        );
        assert!(
            thumbnails
                .get(&ThumbnailKey::file(&chats[0], "picture"))
                .is_some(),
            "recent chat thumbnail must remain cached"
        );
        thumbnails.forget_chats(&chats[..1]);
        show(&mut thumbnails, &chats[1]);
        assert!(
            thumbnails
                .get(&ThumbnailKey::file(&chats[0], "picture"))
                .is_none(),
            "deleted chat must release its recent picture"
        );
        assert!(
            thumbnails
                .get(&ThumbnailKey::file(&chats[1], "picture"))
                .is_some()
        );
        for chat in &chats[2..] {
            show(&mut thumbnails, chat);
        }
        assert_eq!(
            thumbnails.discarded,
            [
                Path::new("/tmp/0.png").into(),
                Path::new("/tmp/1.png").into()
            ]
        );
    }

    #[test]
    fn promotion_discards_a_replaced_file_only_when_its_path_is_unshared() {
        let mut thumbnails = Thumbnails::default();
        let session = "chat".parse().unwrap();
        let draft = ThumbnailKey::Draft("draft".into());
        let file = ThumbnailKey::file(&session, "file");
        thumbnails.ready.insert(draft, thumbnail("/tmp/draft.png"));
        thumbnails
            .ready
            .insert(file.clone(), thumbnail("/tmp/old.png"));
        thumbnails.promote("draft", &session, "file");
        assert_eq!(thumbnails.discarded, vec![Path::new("/tmp/old.png").into()]);

        // Promoting the same shared draft twice must not schedule its live path for deletion.
        thumbnails.promote("draft", &session, "file");
        assert_eq!(thumbnails.discarded.len(), 1);
        thumbnails.forget_draft("draft");
        assert_eq!(thumbnails.discarded.len(), 1);
        assert!(thumbnails.get(&file).is_some());
    }
}
