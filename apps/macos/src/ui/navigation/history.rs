use super::{Context, PageTab, Window, Workspace};
use mobius::SessionId;

#[derive(Clone, PartialEq)]
struct Entry {
    active: PageTab,
    primary: Option<PageTab>,
}

pub(in crate::ui) struct History {
    pages: Vec<Entry>,
    cursor: usize,
    replaying: bool,
}

impl Default for History {
    fn default() -> Self {
        Self {
            pages: vec![Entry {
                active: PageTab::Draft,
                primary: Some(PageTab::Draft),
            }],
            cursor: 0,
            replaying: false,
        }
    }
}

impl History {
    pub(in crate::ui) fn record(&mut self, tabs: &[PageTab], active: usize) {
        let Some(active) = tabs.get(active) else {
            return;
        };
        let entry = Entry {
            active: active.clone(), // rules-allow: history keeps its own copy of the page the tabs show
            primary: tabs
                .iter()
                .find(|tab| !matches!(tab, PageTab::Content(_)))
                .cloned(), // rules-allow: history keeps its own copy of the page the tabs show
        };
        if self.replaying || self.pages.get(self.cursor) == Some(&entry) {
            return;
        }
        self.pages.truncate(self.cursor + 1);
        self.pages.push(entry);
        self.cursor += 1;
    }

    pub(in crate::ui) fn can_move(&self, forward: bool) -> bool {
        if forward {
            self.cursor + 1 < self.pages.len()
        } else {
            self.cursor > 0
        }
    }

    /// Drops every entry that shows `page`, such as a deleted chat.
    pub(in crate::ui) fn forget(&mut self, page: &PageTab) {
        let keep = |entry: &Entry| &entry.active != page && entry.primary.as_ref() != Some(page);
        let cursor = self.pages.get(self.cursor).and_then(|current| {
            self.pages
                .iter()
                .filter(|entry| keep(entry))
                .position(|kept| kept == current)
        });
        self.pages.retain(keep);
        if self.pages.is_empty() {
            *self = Self::default();
            return;
        }
        self.cursor = cursor.unwrap_or(self.pages.len() - 1);
    }

    fn step(&mut self, forward: bool) -> Option<Entry> {
        if !self.can_move(forward) {
            return None;
        }
        if forward {
            self.cursor += 1;
        } else {
            self.cursor -= 1;
        }
        self.pages.get(self.cursor).cloned() // rules-allow: history keeps the entry while the page it restores takes its own copy
    }
}

impl Workspace {
    /// Deletes the chats `ids` from the gateway and forgets them here, starting a new chat
    /// when the open one goes.
    pub(in crate::ui) fn delete_chats(
        &mut self,
        ids: &[SessionId],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.send(
            mobius_gateway::wire::ClientMessage::DeleteSessions {
                request_id: crate::gateway::request_id(),
                session_ids: ids.iter().map(SessionId::share).collect(),
                selection: mobius::backend::session_files::SessionFileSelection::All,
            },
            cx,
        );
        self.runtime.thumbnails.forget_chats(ids);
        for id in ids {
            self.set_unread(id, false);
            self.history.forget(&PageTab::Chat(id.share()));
            self.forget_chat_tabs(id, cx);
        }
        let selected = self.client.state.selected_session_id.as_ref();
        if selected.is_some_and(|selected| ids.contains(selected)) {
            self.new_chat(window, cx);
        }
        self.prune_thumbnails();
    }

    pub(in crate::ui) fn navigate_history(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        while let Some(entry) = self.history.step(forward) {
            if !self.page_exists(&entry.active)
                || entry
                    .primary
                    .as_ref()
                    .is_some_and(|page| !self.page_exists(page))
            {
                continue;
            }
            // Restoring the primary may open a session; its later reply preserves the selected document.
            self.history.replaying = true;
            if matches!(entry.active, PageTab::Content(_))
                && let Some(primary) = entry.primary
            {
                self.restore_history_page(primary, window, cx);
            }
            self.restore_history_page(entry.active, window, cx);
            self.history.replaying = false;
            break;
        }
        cx.notify();
    }

    /// Whether a recorded page still has something to show; deleted chats and Bots are skipped.
    fn page_exists(&self, page: &PageTab) -> bool {
        let gateway = self.client.state.gateway.as_ref();
        match page {
            PageTab::Content(id) => self.content.tabs.contains_key(id),
            PageTab::Chat(id) => self
                .client
                .state
                .sessions
                .iter()
                .any(|s| &s.session_id == id),
            PageTab::Bot(id) | PageTab::BotSettings(id, _) => {
                gateway.is_some_and(|g| g.bots.iter().any(|bot| &bot.id == id))
            }
            PageTab::Provider(id) => gateway.is_some_and(|g| {
                g.provider_instances
                    .iter()
                    .any(|provider| &provider.selection.instance == id)
            }),
            _ => true,
        }
    }

    fn restore_history_page(&mut self, page: PageTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(section) = page.section() {
            self.navigation_section = section;
        }
        match page {
            PageTab::Draft => self.new_chat(window, cx),
            PageTab::Bot(id) => self.open_bot_page(id, window, cx),
            PageTab::Provider(id) => self.open_page(PageTab::Provider(id), window, cx),
            page => self.open_page(page, window, cx),
        }
    }
}
