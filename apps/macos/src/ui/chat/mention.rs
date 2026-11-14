//! Pointing a message at what's beside it: an open tab's file or page, a file in the chat's
//! folder, or one of the chat's files, from `@` in the composer or a right click; and
//! saving any of those files to Downloads.

use super::*;
use crate::gateway::{PreviewData, PreviewRequest};
use gpui::{ContentMask, CursorStyle, EntityInputHandler as _, Keystroke, TextAlign, TextRun};
use gpui_component::input::TextareaState;
use gpui_component::menu::PopupMenu;
use mobius::SessionId;
use mobius::protocol::{
    FrontendBlockRole, FrontendBlockState, SessionFileOrigin, SessionFileRecord,
    SessionFileReference,
};
use mobius_gateway::wire::TranscriptItemContent;
use std::ops::Range;
use std::path::PathBuf;

/// Suggestions the `@` menu shows at most.
const SUGGESTIONS: usize = 8;

/// Something a message can point at.
#[derive(Clone)]
pub(in crate::ui) enum Mention {
    /// A file in the chat's folder on the gateway's machine, named by its path.
    Path(String),
    /// A web page, by its address.
    Page(String),
    /// A file on this Mac: named by its path to a gateway on this Mac, otherwise attached.
    Local(PathBuf),
    /// A file already stored with the chat, attached without uploading it again.
    ChatFile(SessionFileReference),
}

/// The `@query` being typed: where its `@` sits and the highlighted suggestion.
pub(super) struct MentionMenu {
    at: usize,
    query: String,
    selected: usize,
}

/// What `@` suggests in one chat beyond its open tabs, refreshed as the menu opens.
#[derive(Default)]
pub(super) struct ChatSources {
    session: Option<SessionId>,
    pending: Vec<Arc<str>>,
    workspace: Vec<String>,
    files: Vec<SessionFileRecord>,
    files_loaded: bool,
    files_request: Option<Arc<str>>,
}

impl ChatSources {
    fn begin_files(&mut self, request: &Arc<str>) {
        // A previous listing cannot establish that a newly published file was purged.
        self.files_loaded = false;
        self.files_request = Some(Arc::clone(request));
    }

    fn finish_files(&mut self, request: &str, files: Vec<SessionFileRecord>) {
        if self.files_request.as_deref() != Some(request) {
            return;
        }
        self.files_request = None;
        self.files = files;
        self.files_loaded = true;
    }

    fn file_missing(&self, file_id: &str) -> bool {
        self.files_loaded && !self.files.iter().any(|record| record.file.id == file_id)
    }
}

pub(super) fn files_published<'a>(
    mut entries: impl Iterator<Item = &'a crate::gateway::TranscriptEntry>,
) -> bool {
    entries.any(|entry| {
        entry.block().is_some_and(|block| {
            block.role == FrontendBlockRole::Artifact
                && block.state == FrontendBlockState::Complete
                && (!block.files.is_empty() || block.content.files().next().is_some())
        })
    })
}

/// One suggestion: its title, a detail line, its glyph and what choosing it adds.
struct Suggestion {
    title: SharedString,
    detail: String,
    glyph: &'static str,
    mention: Mention,
}

/// Byte ranges of the `@path` words in `text` whose path is in `linked`.
fn linked_mentions<'a>(
    text: &'a str,
    linked: &'a std::collections::BTreeSet<String>,
) -> impl Iterator<Item = Range<usize>> + 'a {
    text.match_indices('@').filter_map(move |(at, _)| {
        let starts_word = text
            .get(..at)
            .and_then(|before| before.chars().next_back())
            .is_none_or(char::is_whitespace);
        let end = text
            .get(at..)
            .and_then(|rest| rest.find(char::is_whitespace))
            .map_or(text.len(), |length| at + length);
        (starts_word
            && text
                .get(at + 1..end)
                .is_some_and(|path| linked.contains(path)))
        .then_some(at..end)
    })
}

/// The `@` and the whole link on screen, while the link sits on one line in view and the
/// caret or selection is outside it.
fn link_bounds(
    composer: &Entity<TextareaState>,
    range: &Range<usize>,
    window: &mut Window,
    cx: &mut App,
) -> Option<(Bounds<Pixels>, Bounds<Pixels>)> {
    composer.update(cx, |state, cx| {
        let selection = state.selected_range();
        if state.focus_handle(cx).is_focused(window)
            && selection.start <= range.end
            && range.start <= selection.end
        {
            return None;
        }
        let area = state.text_bounds()?;
        let utf16 = {
            let text = state.value();
            let utf16 = |offset: usize| {
                text.get(..offset)
                    .map_or(0, |prefix| prefix.encode_utf16().count())
            };
            (utf16(range.start), utf16(range.end))
        };
        let (start, end) = utf16;
        let at = state.bounds_for_range(start..start + 1, area, window, cx)?;
        let last = state.bounds_for_range(end - 1..end, area, window, cx)?;
        // Ranges outside the drawn lines come back at the origin; the `@` must be under it.
        let probe = point(at.left() + px(1.), at.center().y);
        let shown = state.character_index_for_point(probe, window, cx) == Some(start);
        (shown && at.top() == last.top())
            .then(|| (at, Bounds::from_corners(at.origin, last.bottom_right())))
    })
}

/// The `@query` ending at `cursor`, when the word under the cursor starts with `@`.
fn typed_mention(text: &str, cursor: usize) -> Option<(usize, &str)> {
    let before = text.get(..cursor)?;
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(index, c)| index + c.len_utf8());
    Some((start, before.get(start..)?.strip_prefix('@')?))
}

impl Workspace {
    /// The right-click menu for anything a message can point at: Add to message, and
    /// Download for files.
    pub(in crate::ui) fn file_menu(
        mention: Mention,
        cx: &Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        type Act = fn(&mut Workspace, Mention, &mut Window, &mut Context<Workspace>);
        let view = cx.entity().downgrade();
        move |menu, _, cx| {
            let item = |label: &'static str, glyph: &'static str, act: Act| {
                let mention = mention.clone(); // rules-allow: each menu item's 'static listener owns its copy of the mention
                let run = move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                    act(this, mention.clone(), window, cx); // rules-allow: every click acts on its own copy of the mention
                };
                workspace_item(label, glyph, &view, run, &palette(cx))
            };
            let menu = menu.item(item("Add to message", "hi.link", Self::add_mention));
            if matches!(mention, Mention::Page(_)) {
                return menu;
            }
            menu.item(item("Download", "hi.download01", |this, mention, _, cx| {
                this.download(mention, cx);
            }))
            .when(
                match &mention {
                    Mention::ChatFile(file) => {
                        !(file.name.starts_with("generated-image.")
                            && file.media_type.starts_with("image/"))
                    }
                    Mention::Path(_) => true,
                    _ => false,
                },
                |menu| {
                    menu.item(item(
                        "Delete…",
                        "hi.trash",
                        |this, mention, window, cx| this.confirm_delete_file(mention, window, cx),
                    ))
                },
            )
        }
    }

    /// Saves what `mention` points at to Downloads, numbered rather than overwriting, as a
    /// download from the web would go. Pages download through the browser instead.
    pub(in crate::ui) fn download(&mut self, mention: Mention, cx: &Context<Self>) {
        match mention {
            Mention::ChatFile(file) => {
                let (Some(session), Some(gateway), Some(destination)) = (
                    self.client.state.selected_session_id.as_ref(),
                    self.client.gateway.as_ref(),
                    crate::host::browser::download_path(&file.name),
                ) else {
                    return self.report("Open this chat to download its files.");
                };
                if let Err(error) = gateway.download(session.share(), file, destination) {
                    self.report(crate::gateway::describe(&error));
                }
            }
            Mention::Local(source) => {
                let name = source.file_name().unwrap_or_default().to_string_lossy();
                let Some(destination) = crate::host::browser::download_path(&name) else {
                    return;
                };
                Self::save_in_background(cx, move || {
                    crate::gateway::local::copy(&source, destination)
                });
            }
            Mention::Path(path) => {
                let (Some(session), Some(gateway)) = (
                    self.client.state.selected_session_id.as_ref(),
                    self.client.gateway.as_ref(),
                ) else {
                    return self.report("Open this chat to download its files.");
                };
                let id = crate::gateway::request_id();
                match gateway.preview(Arc::clone(&id), session.share(), PreviewRequest::File(path))
                {
                    Ok(()) => self.chat_ui.saving.push(id),
                    Err(error) => self.report(crate::gateway::describe(&error)),
                }
            }
            Mention::Page(_) => {}
        }
    }

    /// Runs `save` off the main thread and reports where the file went.
    fn save_in_background(
        cx: &Context<Self>,
        save: impl FnOnce() -> std::io::Result<PathBuf> + Send + 'static,
    ) {
        cx.spawn(async move |this, cx| {
            let saved = cx.background_executor().spawn(async move { save() }).await;
            if let Err(error) = this.update(cx, |this, cx| {
                match saved {
                    Ok(path) => this.announce_saved(path),
                    Err(error) => this.report(format!("Couldn’t save the file: {error}")),
                }
                cx.notify();
            }) {
                tracing::debug!(%error, "the workspace closed before the file was saved");
            }
        })
        .detach();
    }

    /// Adds `mention` to the message being written and returns the caret to it.
    pub(in crate::ui) fn add_mention(
        &mut self,
        mention: Mention,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match mention {
            Mention::Path(path) => self.link_in_composer(path, window, cx),
            Mention::Page(url) => self.insert_in_composer(format!("{url} "), window, cx),
            Mention::Local(path) if self.client.local_gateway => {
                self.link_in_composer(path.display().to_string(), window, cx);
            }
            Mention::Local(path) => self.attach_paths(vec![path], cx),
            Mention::ChatFile(file) => self.attach_chat_file(file, cx),
        }
        self.focus_composer(window, cx);
        cx.notify();
    }

    fn insert_in_composer(&self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.composer
            .update(cx, |input, cx| input.insert(text, window, cx));
    }

    /// Writes `@path` for the Bot and remembers it, so the composer draws it as a link.
    fn link_in_composer(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let text = format!("@{path} ");
        self.chat_ui.linked.insert(path);
        self.insert_in_composer(text, window, cx);
    }

    /// Opens the file a composer link names, in a tab beside the chat.
    fn open_link(&mut self, path: Range<usize>, cx: &mut Context<Self>) {
        let Some(session) = self
            .client
            .state
            .selected_session_id
            .as_ref()
            .map(SessionId::share)
        else {
            return;
        };
        let Some(path) = self.composer.read(cx).value().get(path).map(str::to_owned) else {
            return;
        };
        self.open_workspace_file(session, path, cx);
    }

    /// Draws each linked `@path` over the composer's own text: the file's glyph over the
    /// `@` and the path in the accent colour, opening it on a click. The text underneath
    /// stays plain, so a link being edited, selected, wrapped or scrolled away shows as
    /// typed.
    pub(super) fn composer_links(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        let text = self.composer.read(cx).value();
        let links: Vec<_> = linked_mentions(&text, &self.chat_ui.linked).collect();
        if links.is_empty() {
            return None;
        }
        let (composer, view) = (self.composer.downgrade(), cx.entity().downgrade());
        let (accent, card) = (self.palette.accent, self.palette.card);
        let paint = move |bounds: Bounds<Pixels>, (), window: &mut Window, cx: &mut App| {
            let Some(composer) = composer.upgrade() else {
                return;
            };
            let Some(area) = composer.read(cx).text_bounds() else {
                return;
            };
            let mut hits = Vec::new();
            // Clipped to the field's scrolled lines, with the side insets for the glyph.
            let mask = Bounds::new(
                point(bounds.left(), area.top()),
                size(bounds.size.width, area.size.height),
            );
            window.with_content_mask(Some(ContentMask { bounds: mask }), |window| {
                for range in links {
                    let Some((at, link)) = link_bounds(&composer, &range, window, cx) else {
                        continue;
                    };
                    let value = composer.read(cx).value();
                    let Some(path) = value.get(range.start + 1..range.end) else {
                        continue;
                    };
                    let glyph = panel::FileKind::for_name(path).glyph;
                    let run = TextRun {
                        len: path.len(),
                        font: font(FontSet::current().regular()),
                        color: accent,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    let line = window.text_system().shape_line(
                        SharedString::from(path.to_owned()),
                        Type::Body.size(),
                        &[run],
                        None,
                    );
                    // The glyph takes the `@`'s place, a hair gap before the path.
                    let mark = Bounds::new(
                        point(
                            at.right() - space::XXS - glyph::MARK,
                            at.center().y - glyph::MARK / 2.,
                        ),
                        size(glyph::MARK, glyph::MARK),
                    );
                    window.paint_quad(fill(link, card));
                    let icon = SharedString::from(format!("icons/{glyph}.svg"));
                    if let Err(error) = window.paint_svg(
                        mark,
                        icon,
                        None,
                        Default::default(),
                        palette(cx).icon_ink(),
                        cx,
                    ) {
                        tracing::debug!(%error, "couldn't paint a composer link's glyph");
                    }
                    let origin = point(at.right(), link.top());
                    if let Err(error) =
                        line.paint(origin, link.size.height, TextAlign::Left, None, window, cx)
                    {
                        tracing::debug!(%error, "couldn't paint a composer link's path");
                    }
                    hits.push((link, range.start + 1..range.end));
                }
            });
            if hits
                .iter()
                .any(|(link, _)| link.contains(&window.mouse_position()))
            {
                window.set_window_cursor_style(CursorStyle::PointingHand);
            }
            window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
                let hit = hits.iter().find(|(link, _)| link.contains(&event.position));
                if let (DispatchPhase::Bubble, MouseButton::Left, Some((_, path))) =
                    (phase, event.button, hit)
                {
                    cx.stop_propagation();
                    let path = path.start..path.end;
                    if let Err(error) = view.update(cx, |this, cx| this.open_link(path, cx)) {
                        tracing::debug!(%error, "the workspace closed before the link opened");
                    }
                }
            });
        };
        Some(canvas(|_, _, _| {}, paint).absolute().size_full())
    }

    /// Files dropped onto the chat, or pasted into the composer: copied files, or an image
    /// on its own. `false` lets a paste with text fall through to the text.
    pub(in crate::ui) fn paste_into_composer(
        &mut self,
        item: &ClipboardItem,
        cx: &mut Context<Self>,
    ) -> bool {
        let entries = item.entries();
        let text = entries
            .iter()
            .any(|entry| matches!(entry, ClipboardEntry::String(_)));
        let mut paths = Vec::new();
        for entry in entries {
            match entry {
                ClipboardEntry::ExternalPaths(external) => {
                    paths.extend_from_slice(external.paths());
                }
                ClipboardEntry::Image(image) if !text => paths.extend(self.save_pasted(image)),
                _ => {}
            }
        }
        if paths.is_empty() {
            return false;
        }
        self.attach_paths(paths, cx);
        true
    }

    /// A pasted image as a file in the app's private folder, so it uploads like any other.
    fn save_pasted(&mut self, image: &gpui::Image) -> Option<PathBuf> {
        let short = uuid::Uuid::new_v4().simple().to_string();
        let name = format!(
            "Pasted image {}.{}",
            short.get(..6).unwrap_or(&short),
            image.format.extension()
        );
        let path = self.downloads.as_ref()?.path().join(name);
        match crate::gateway::local::save(path, &image.bytes) {
            Ok(path) => Some(path),
            Err(error) => {
                self.report(format!("Couldn’t attach the pasted image: {error}"));
                None
            }
        }
    }

    /// Opens, narrows or closes the `@` menu as the composer's text changes.
    pub(in crate::ui) fn track_mention(&mut self, cx: &Context<Self>) {
        let input = self.composer.read(cx);
        let text = input.value();
        let menu = typed_mention(&text, input.cursor()).map(|(at, query)| MentionMenu {
            at,
            query: query.to_lowercase(),
            selected: 0,
        });
        let opening = menu.is_some() && self.chat_ui.mention.is_none();
        self.chat_ui.mention = menu;
        if opening {
            self.load_chat_sources();
        }
    }

    /// Lists the chat's folder and its stored files, keeping what an earlier listing found
    /// until the new one arrives.
    pub(in crate::ui) fn load_chat_sources(&mut self) {
        self.load_chat_listings(true);
    }

    pub(in crate::ui) fn load_chat_files(&mut self) {
        self.load_chat_listings(false);
    }

    fn load_chat_listings(&mut self, workspace_files: bool) {
        let Some(session) = self.client.state.selected_session_id.as_ref() else {
            return;
        };
        if !self.client.link.is_ready() || !self.active_session_ready() {
            return;
        }
        let sources = &mut self.chat_ui.chat_sources;
        if sources.session.as_ref() != Some(session) {
            *sources = ChatSources {
                session: Some(session.share()),
                ..ChatSources::default()
            };
        }
        let Some(gateway) = &self.client.gateway else {
            return;
        };
        let workspace = self.client.state.workspace_session_id().is_some();
        for request in [PreviewRequest::Files, PreviewRequest::ChatFiles] {
            if matches!(request, PreviewRequest::Files) && (!workspace || !workspace_files) {
                continue;
            }
            let id = crate::gateway::request_id();
            if matches!(request, PreviewRequest::ChatFiles) {
                sources.begin_files(&id);
            }
            if gateway
                .preview(Arc::clone(&id), session.share(), request)
                .is_ok()
            {
                sources.pending.push(id);
            }
        }
    }

    /// Takes the listings `load_chat_sources` asked for and the files `download` read;
    /// anything else is handed back.
    pub(in crate::ui) fn chat_sources_loaded(
        &mut self,
        request: &str,
        session: &str,
        result: Result<PreviewData, String>,
        cx: &Context<Self>,
    ) -> Option<Result<PreviewData, String>> {
        if let Some(index) = self.chat_ui.saving.iter().position(|id| **id == *request) {
            self.chat_ui.saving.swap_remove(index);
            match result {
                Ok(PreviewData::File { path, bytes }) => {
                    if let Some(destination) = crate::host::browser::download_path(&path) {
                        Self::save_in_background(cx, move || {
                            crate::gateway::local::save(destination, &bytes)
                        });
                    }
                }
                Err(error) => self.report(error),
                Ok(_) => {}
            }
            return None;
        }
        let sources = &mut self.chat_ui.chat_sources;
        let Some(index) = sources.pending.iter().position(|id| **id == *request) else {
            return Some(result);
        };
        sources.pending.swap_remove(index);
        if sources.session.as_deref() == Some(session) {
            match result {
                Ok(PreviewData::Files { files, .. }) => {
                    sources.workspace = files.into_iter().map(|file| file.path).collect();
                }
                Ok(PreviewData::ChatFiles(files)) => {
                    sources.finish_files(request, files);
                }
                _ => {}
            }
        }
        None
    }

    /// The chat listings, while they belong to the open chat.
    fn chat_sources(&self) -> Option<&ChatSources> {
        let sources = &self.chat_ui.chat_sources;
        (sources.session.is_some() && self.client.state.selected_session_id == sources.session)
            .then_some(sources)
    }

    pub(in crate::ui) fn refresh_removable_files(&mut self) {
        let revision = self.client.state.transcript.revision();
        if self
            .chat_ui
            .removable_revision
            .as_ref()
            .is_some_and(|(session, previous)| {
                session.as_deref() == self.client.state.selected_session_id.as_deref()
                    && *previous == revision
            })
        {
            return;
        }
        self.chat_ui.removable_revision = Some((
            self.client
                .state
                .selected_session_id
                .as_ref()
                .map(SessionId::share),
            revision,
        ));
        self.chat_ui.removable_files.clear();
        for entry in self.client.state.transcript.entries() {
            match &entry.content {
                TranscriptItemContent::Message { message } => self
                    .chat_ui
                    .removable_files
                    .extend(message.attachments.iter().map(|file| file.id.clone())), // rules-allow: the cache keeps ids across transcript revisions
                TranscriptItemContent::Block { block, .. }
                    if block.role == mobius::protocol::FrontendBlockRole::Artifact
                        || block.format == mobius::protocol::FrontendBlockFormat::Image =>
                {
                    self.chat_ui.removable_files.extend(
                        block
                            .files
                            .iter()
                            .chain(block.content.files())
                            .map(|file| file.id.clone()), // rules-allow: the cache keeps ids across transcript revisions
                    );
                }
                _ => {}
            }
        }
    }

    pub(in crate::ui) fn file_removed(&self, session_id: &str, file_id: &str) -> bool {
        self.chat_sources().is_some_and(|sources| {
            sources.session.as_deref() == Some(session_id)
                && sources.file_missing(file_id)
                && self.chat_ui.removable_files.contains(file_id)
        })
    }

    /// User uploads and published artifacts from the gateway's current file listing.
    pub(in crate::ui) fn chat_files(&self) -> Vec<(&SessionFileReference, &'static str)> {
        self.chat_sources()
            .into_iter()
            .flat_map(|sources| sources.files.iter())
            .map(|record| {
                let origin = match record.origin {
                    SessionFileOrigin::Agent => "Sent by the Bot",
                    SessionFileOrigin::User => "Sent by you",
                };
                (&record.file, origin)
            })
            .collect()
    }

    /// The files the Bot sent back in the open chat, from the same listing.
    pub(in crate::ui) fn chat_outputs(&self) -> impl Iterator<Item = &SessionFileRecord> {
        self.chat_sources()
            .into_iter()
            .flat_map(|sources| sources.files.iter())
            .filter(|record| record.origin == SessionFileOrigin::Agent)
    }

    /// Lists two Bot files and one upload as the open chat's files.
    #[cfg(feature = "snapshot")]
    pub(in crate::ui) fn fixture_chat_files(&mut self) {
        let Some(session) = &self.client.state.selected_session_id else {
            return;
        };
        let record = |origin, name: &str, media_type: &str, size| SessionFileRecord {
            origin,
            file: SessionFileReference {
                id: format!("fixture-{name}"),
                name: name.into(),
                media_type: media_type.into(),
                size,
            },
            modified_at_ms: 1_763_100_000_000,
        };
        self.chat_ui.chat_sources = ChatSources {
            session: Some(session.share()),
            files: vec![
                record(
                    SessionFileOrigin::Agent,
                    "release-notes.md",
                    "text/markdown",
                    4_200,
                ),
                record(
                    SessionFileOrigin::User,
                    "brief.pdf",
                    "application/pdf",
                    182_000,
                ),
                record(
                    SessionFileOrigin::Agent,
                    "context-chart.png",
                    "image/png",
                    96_000,
                ),
            ],
            files_loaded: true,
            ..ChatSources::default()
        };
    }

    /// Open tabs first, then the chat's files, then its folder, each matching the query.
    fn mention_suggestions(&self, cx: &App) -> Vec<Suggestion> {
        let Some(menu) = &self.chat_ui.mention else {
            return Vec::new();
        };
        let matches = |text: &str| text.to_lowercase().contains(&menu.query);
        let tabs = self.visible_content().filter_map(|tab| {
            let mention = self.tab_mention(&tab.id, cx)?;
            matches(&tab.title).then(|| Suggestion {
                title: SharedString::clone(&tab.title),
                detail: "Open tab".into(),
                glyph: tab.glyph(),
                mention,
            })
        });
        let files = self
            .chat_files()
            .into_iter()
            .filter(|(file, _)| matches(&file.name))
            .map(|(file, origin)| Suggestion {
                title: file.name.clone().into(), // rules-allow: a suggestion owns its title for the menu row
                detail: origin.into(),
                glyph: file_glyph(&file.media_type),
                mention: Mention::ChatFile(file.to_owned()),
            });
        let workspace = self
            .chat_sources()
            .into_iter()
            .flat_map(|sources| sources.workspace.iter())
            .filter(|path| matches(path))
            .map(|path| {
                let (folder, name) = path.rsplit_once('/').unwrap_or(("Chat folder", path));
                Suggestion {
                    title: name.to_owned().into(),
                    detail: folder.to_owned(),
                    glyph: panel::FileKind::for_name(path).glyph,
                    mention: Mention::Path(path.to_owned()),
                }
            });
        tabs.chain(files)
            .chain(workspace)
            .take(SUGGESTIONS)
            .collect()
    }

    /// Replaces the typed `@query` with suggestion `index`.
    fn choose_mention(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(suggestion) = self.mention_suggestions(cx).into_iter().nth(index) else {
            return;
        };
        let Some(menu) = self.chat_ui.mention.take() else {
            return;
        };
        self.composer.update(cx, |input, cx| {
            let cursor = input.cursor();
            input.set_selected_range(menu.at..cursor, cx);
            input.replace("", window, cx);
        });
        self.add_mention(suggestion.mention, window, cx);
    }

    /// Arrows move through the `@` menu, Return or Tab choose, Escape closes it; `true`
    /// when the key was the menu's, so the composer doesn't also act on it.
    pub(in crate::ui) fn mention_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.chat_ui.mention.is_none()
            || keystroke.modifiers.modified()
            || !self.composer.read(cx).focus_handle(cx).is_focused(window)
        {
            return false;
        }
        let count = self.mention_suggestions(cx).len();
        let Some(menu) = self.chat_ui.mention.as_mut() else {
            return false;
        };
        match keystroke.key.as_str() {
            "up" if count > 0 => menu.selected = (menu.selected + count - 1) % count,
            "down" if count > 0 => menu.selected = (menu.selected + 1) % count,
            "enter" | "tab" if count > 0 => {
                let selected = menu.selected;
                self.choose_mention(selected, window, cx);
            }
            "escape" => self.chat_ui.mention = None,
            _ => return false,
        }
        cx.notify();
        true
    }

    /// The `@` menu over the composer, while a query has suggestions.
    pub(super) fn mention_card(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let selected = self.chat_ui.mention.as_ref()?.selected;
        let suggestions = self.mention_suggestions(cx);
        if suggestions.is_empty() {
            return None;
        }
        let p = self.palette;
        let rows = suggestions
            .into_iter()
            .enumerate()
            .map(|(index, suggestion)| {
                div()
                    .id(("mention", index))
                    .flex()
                    .items_center()
                    .gap(space::S)
                    .h(height::REGULAR)
                    .px(space::S)
                    .control_corners()
                    .cursor_pointer()
                    .when(index == selected, |row| row.bg(p.accent_soft))
                    .hover(|row| row.bg(p.hover))
                    .child(icon(suggestion.glyph, &palette(cx)).size(glyph::INLINE))
                    .child(
                        div()
                            .flex_shrink_0()
                            .typography(Type::Body)
                            .child(suggestion.title),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .typography(Type::Caption)
                            .text_color(p.muted)
                            .child(suggestion.detail),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.choose_mention(index, window, cx);
                    }))
            });
        Some(
            div()
                .p(space::XS)
                .flex()
                .flex_col()
                .relative()
                .map(|card| backdrop::floating(card, p.card, cx))
                .border_1()
                .border_color(p.separator)
                .shadow(composer::surface_shadow())
                .children(rows)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{ChatSources, files_published};
    use mobius::protocol::{
        FrontendBlock, FrontendBlockFormat, FrontendBlockRole, FrontendBlockState,
        SessionFileOrigin, SessionFileRecord, SessionFileReference,
    };

    fn generated_file() -> SessionFileRecord {
        SessionFileRecord {
            origin: SessionFileOrigin::Agent,
            file: SessionFileReference {
                id: "generated".into(),
                name: "generated-image.png".into(),
                media_type: "image/png".into(),
                size: 100,
            },
            modified_at_ms: 0,
        }
    }

    #[test]
    fn generated_image_waits_for_its_current_listing_before_inferring_purge() {
        let file = generated_file();
        let mut sources = ChatSources::default();
        sources.begin_files(&"opened".into());
        sources.finish_files("opened", Vec::new());
        sources.begin_files(&"before-generation".into());
        let mut script = crate::gateway::fixture::Script::default();
        script.block(
            "image_generation",
            FrontendBlock {
                role: FrontendBlockRole::Artifact,
                state: FrontendBlockState::Complete,
                format: FrontendBlockFormat::Image,
                files: vec![file.file.clone()],
                ..Default::default()
            },
        );
        assert!(files_published(script.store().entries()));
        sources.begin_files(&"after-generation".into());
        assert!(!sources.file_missing(&file.file.id));
        sources.finish_files("before-generation", Vec::new());
        assert!(!sources.file_missing(&file.file.id));
        sources.finish_files("after-generation", vec![file.clone()]);
        assert!(!sources.file_missing(&file.file.id));

        sources.begin_files(&"after-purge".into());
        sources.finish_files("after-purge", Vec::new());
        assert!(sources.file_missing(&file.file.id));
    }
}
