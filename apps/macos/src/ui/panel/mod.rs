mod agent_browser;
mod browser;
mod code;
mod desktop;
mod diff;
mod file_actions;
mod file_kind;
mod file_tree;
mod files;
mod geometry;
mod launcher;
mod native;
mod source;
mod tab_state;
mod tabs;
mod terminal;
mod toolbar;
mod transcript;
pub(in crate::ui) use code::CodeDoc;
pub(in crate::ui) use diff::DiffDoc;
pub(in crate::ui) use file_actions::FileDoc;
pub(in crate::ui) use file_kind::FileKind;
pub(in crate::ui) use source::FilesDoc;
pub(in crate::ui) use tab_state::{TabOwner, TabState};
pub(in crate::ui) use terminal::TerminalDoc;
pub(in crate::ui) use transcript::RoutineDoc;

use super::*;
use gpui_component::Disableable;
use mobius::protocol::EventMsg;
use mobius_gateway::wire::ServerMessage;
use native::NativeView;
use toolbar::{GIVE_WAY, document, line_count, meta, path_label, spacer, toolbar, totals};

/// Package-owned UI state; new fields belong here rather than on `Workspace`.
#[derive(Default)]
pub(in crate::ui) struct ContentState {
    pub(in crate::ui) tabs: BTreeMap<Arc<str>, ContentTab>,
    agent_browsers: BTreeMap<String, agent_browser::AgentBrowserView>,
    browsers: BTreeMap<String, browser::BrowserTab>,
    desktop: Option<desktop::DesktopTab>,
    pub(in crate::ui) navigation: TabState,
    /// Capability commands in flight, whose preview answer opens as a tab.
    commands: std::collections::VecDeque<Arc<str>>,
}

/// A side tab's own scroll with the page padding, its column no wider than a page.
fn side_tab_body(id: &str, body: AnyElement) -> AnyElement {
    div()
        .id(SharedString::from(format!("side-tab:{id}")))
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .child(
            div()
                .w_full()
                .max_w(width::PAGE)
                .mx_auto()
                .p(space::L)
                .flex()
                .flex_col()
                .child(body),
        )
        .into_any_element()
}

/// Why the gateway turned a submission down: SwiftUI settles a request on any of these.
fn rejection(event: &EventMsg) -> Option<&str> {
    match event {
        EventMsg::SubmissionRejected(event) => Some(&event.message),
        EventMsg::Warning(event) => Some(&event.message),
        EventMsg::Error(event) => Some(&event.message),
        _ => None,
    }
}

impl ContentState {
    /// Remembers a capability command submitted to the gateway, bounded to the newest.
    pub(in crate::ui) fn track_command(&mut self, submission: Arc<str>) {
        if self.commands.len() == 64 {
            self.commands.pop_front();
        }
        self.commands.push_back(submission);
    }

    fn settle_command(&mut self, submission: &str) -> bool {
        let position = self.commands.iter().position(|id| **id == *submission);
        position
            .and_then(|index| self.commands.remove(index))
            .is_some()
    }
}

impl Workspace {
    /// A command's transcript opens beside the chat; its rejection fails the waiting tab.
    pub(in crate::ui) fn content_on_message(
        &mut self,
        message: &ServerMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.browser_on_message(message, window, cx);
        match message {
            ServerMessage::DesktopControlState { enabled, .. } => {
                self.desktop_execution_changed(*enabled, cx);
            }
            ServerMessage::AgentEvent {
                session_id, record, ..
            } => {
                let Some(submission) = record.event.submission_id.as_deref() else {
                    return;
                };
                if let Some(preview) = self
                    .client
                    .state
                    .arrived_preview
                    .as_ref()
                    .and_then(|id| self.client.state.previews.get(id))
                    .map(std::rc::Rc::clone)
                    && self.content.settle_command(submission)
                {
                    self.present_preview(session_id, submission, preview, cx);
                } else if let Some(reason) = rejection(&record.event.msg)
                    && self.content.settle_command(submission)
                {
                    self.fail_loading(submission, reason, cx);
                }
            }
            ServerMessage::Rejected {
                request_id,
                message,
                ..
            } if self.content.settle_command(request_id) => {
                self.fail_loading(request_id, message, cx);
            }
            _ => {}
        }
    }

    fn fail_loading(&mut self, request: &str, message: &str, cx: &mut Context<Self>) {
        for tab in self.content.tabs.values_mut() {
            if matches!(&tab.content, TabContent::Loading { request: pending, .. } if **pending == *request)
            {
                tab.content = TabContent::Failed(message.to_owned());
                cx.notify();
            }
        }
    }
}

impl TabContent {
    /// Drawn by a native QuickLook, terminal or browser view over the pane.
    pub(in crate::ui) fn shows_native(&self) -> bool {
        match self {
            Self::File(_) | Self::Terminal(_) | Self::Browser | Self::Desktop => true,
            Self::Files(doc) => doc.shows_native(),
            _ => false,
        }
    }
}

impl Workspace {
    pub(super) fn active_content(&self) -> Option<&ContentTab> {
        let PageTab::Content(id) = self.page_tabs.get(self.active_page)? else {
            return None;
        };
        self.content.tabs.get(id)
    }

    pub(super) fn open_content(&mut self, tab: ContentTab, cx: &mut Context<Self>) {
        let owner = match &tab.content {
            TabContent::Loading {
                session_id: Some(id),
                ..
            }
            | TabContent::Preview { session_id: id, .. } => TabOwner::Chat(id.share()),
            _ => TabOwner::of(&self.page_tabs),
        };
        self.open_content_for(owner, tab, cx);
    }

    pub(super) fn open_content_for(
        &mut self,
        owner: TabOwner,
        tab: ContentTab,
        cx: &mut Context<Self>,
    ) {
        let id = Arc::clone(&tab.id);
        let visible = owner == TabOwner::of(&self.page_tabs);
        self.content.navigation.insert(owner, Arc::clone(&id));
        self.content.tabs.insert(Arc::clone(&id), tab);
        self.unify_file_tab(&id, cx);
        if visible {
            self.activate_tab(PageTab::Content(id));
        }
        cx.notify();
    }

    pub(in crate::ui) fn visible_content(&self) -> impl Iterator<Item = &ContentTab> {
        self.page_tabs.iter().filter_map(|page| match page {
            PageTab::Content(id) => self.content.tabs.get(id),
            _ => None,
        })
    }

    pub(super) fn chat_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let chat = self.render_chat(window, cx);
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(chat)
            .into_any_element()
    }

    pub(super) fn content_page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = self.render_content(window, cx);
        let split = self.pane_split(window, cx);
        if let Some(full) = split
            && let Some(shown) = self.pane_sliding(full)
        {
            let chat = self.chat_page(window, cx);
            return sliding_pane(chat, content, shown, full);
        }
        if let Some(width) = self.pane_split(window, cx) {
            let chat = self.chat_page(window, cx);
            gpui_component::h_resizable("chat-content-panes")
                .with_state(&self.content_resize)
                .child(
                    gpui_component::resizable_panel()
                        .size_range(width::CHAT_MIN..Pixels::MAX)
                        .child(chat),
                )
                .child(
                    gpui_component::resizable_panel()
                        .size(width)
                        .size_range(width::DOC_MIN..Pixels::MAX)
                        .fixed()
                        .child(content),
                )
                .into_any_element()
        } else if self.document_composer_inset(window, cx).is_some() {
            div()
                .relative()
                .flex()
                .flex_col()
                .size_full()
                .child(content)
                .child(self.document_composer(window, cx))
                .into_any_element()
        } else {
            content
        }
    }

    /// What the side tab `id` offers a message: its file or its page.
    pub(in crate::ui) fn tab_mention(&self, id: &str, cx: &App) -> Option<chat::Mention> {
        match &self.content.tabs.get(id)?.content {
            TabContent::Code(doc) => Some(match &doc.origin {
                code::Origin::Workspace(path) => chat::Mention::Path(path.to_string()),
                code::Origin::Local(path) => chat::Mention::Local(path.to_owned()),
            }),
            TabContent::Files(doc) => doc.selected_path().map(|path| {
                self.chat_file_tab(id)
                    .filter(|file| doc.selected_display_path() == Some(file.name.as_str()))
                    .map_or_else(|| doc.mention(path), chat::Mention::ChatFile)
            }),
            TabContent::File(path) => Some(self.chat_file_tab(id).map_or_else(
                || chat::Mention::Local(path.path.clone()), // rules-allow: the mention leaves the tab and owns its path
                chat::Mention::ChatFile,
            )),
            TabContent::Browser => self
                .browser_address(id, cx)
                .map(|url| chat::Mention::Page(url.to_string())),
            TabContent::AgentBrowser => self
                .agent_browser_url(id)
                .map(|url| chat::Mention::Page(url.to_owned())),
            _ => None,
        }
    }

    /// The final tab-id component is the stored file UUID, after the chat's namespace.
    fn chat_file_tab(&self, id: &str) -> Option<mobius::protocol::SessionFileReference> {
        let (_, id) = id.rsplit_once(':')?;
        self.chat_files()
            .into_iter()
            .find(|(file, _)| file.id == id)
            .map(|(file, _)| file.to_owned())
    }

    /// The chat, with the pane it just hid still sliding out at its edge.
    pub(super) fn with_closing_pane(
        &mut self,
        chat: AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some((id, full, shown)) = self.pane_closing() else {
            return chat;
        };
        let tab = self.content.tabs.get(id).cloned(); // rules-allow: rendering borrows the workspace mutably, so it draws a copy of the tab
        let Some(tab) = tab else {
            return chat;
        };
        let pane = self.render_tab(&tab, window, cx);
        sliding_pane(chat, pane, shown, full)
    }

    pub(super) fn render_content(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.track_files_motion(window, cx);
        self.refresh_active_preview();
        let tab = self.active_content().cloned(); // rules-allow: rendering borrows the workspace mutably, so it draws a copy of the tab
        let Some(tab) = tab else {
            return EmptyState::new("hi.folderOpen", "Open a file, terminal or transcript")
                .detail("Your work stays beside the conversation.")
                .into_any_element();
        };
        self.render_tab(&tab, window, cx)
    }

    fn render_tab(
        &mut self,
        tab: &ContentTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match &tab.content {
            TabContent::Loading { .. } => document(
                None,
                EmptyState::new(tab.glyph(), format!("Opening {}", tab.title)).loading(),
                cx,
            ),
            TabContent::Failed(message) => document(
                None,
                EmptyState::new(Tone::Error.glyph(), format!("Couldn’t open {}", tab.title))
                    .detail(message.clone()), // rules-allow: the empty state owns its copy of the failure message
                cx,
            ),
            TabContent::Code(doc) => self.code_document(tab, doc, window, cx),
            TabContent::Diff(doc) => self.diff_document(&tab.id, doc, cx),
            TabContent::Files(doc) => self.files_document(&tab.id, doc, window, cx),
            TabContent::Preview { preview, .. } => self.preview_document(tab, preview, window, cx),
            TabContent::Routine(preview) => self.routine_document(tab, preview, window, cx),
            TabContent::Launcher => self.launcher_document(cx),
            TabContent::Terminal(doc) => self.terminal_document(&tab.id, doc, cx),
            TabContent::AgentBrowser => self.agent_browser_document(&tab.id, cx),
            TabContent::Browser => self.browser_document(&tab.id, cx),
            TabContent::Desktop => self.desktop_document(cx),
            TabContent::File(path) => document(
                toolbar()
                    .child(path_label(&path.display_path, tab.glyph(), cx))
                    .child(spacer())
                    .child(Self::file_actions(path, &path.display_path, cx)),
                Self::native_content(
                    &tab.id,
                    NativeView::File(path.path.to_string_lossy().into_owned()),
                    cx,
                ),
                cx,
            ),
            TabContent::Detail { detail, .. } => {
                let body = self.detail_tab_body(detail, window, cx);
                let actions = self.detail_tab_actions(detail, cx);
                document(
                    (!actions.is_empty()).then(|| toolbar().child(spacer()).children(actions)),
                    side_tab_body(&tab.id, body),
                    cx,
                )
            }
        }
    }
}

/// The chat beside a pane clipped to `shown` of its `full` width as it slides in or out.
/// Both sides are flex containers like the resizable panels they settle into, so the
/// chat's `flex_1` column keeps its height and the composer stays at the bottom.
fn sliding_pane(chat: AnyElement, pane: AnyElement, shown: Pixels, full: Pixels) -> AnyElement {
    div()
        .flex()
        .size_full()
        .child(div().flex().flex_1().min_w_0().h_full().child(chat))
        .child(
            div()
                .w(shown)
                .h_full()
                .flex_shrink_0()
                .overflow_hidden()
                .child(
                    div()
                        .relative()
                        .flex()
                        .w(full)
                        .h_full()
                        .child(pane)
                        .map(|view| {
                            #[cfg(feature = "snapshot")]
                            let view = view.child(snapshot::probe("pane:content".into()));
                            view
                        }),
                ),
        )
        .into_any_element()
}
