//! Companion routes belong to a chat; their documents and native resources stay alive.

use super::*;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::ui) enum TabOwner {
    Chat(SessionId),
    Draft,
    /// Documents opened outside a chat keep the existing shared page semantics.
    Global,
}

impl TabOwner {
    pub(in crate::ui) fn of(tabs: &[PageTab]) -> Self {
        match tabs
            .iter()
            .find(|page| !matches!(page, PageTab::Content(_)))
        {
            Some(PageTab::Chat(id)) => Self::Chat(id.share()),
            Some(PageTab::Draft) => Self::Draft,
            _ => Self::Global,
        }
    }
}

#[derive(Default)]
struct ChatTabs {
    tabs: Vec<Arc<str>>,
    files: BTreeMap<Arc<str>, Arc<str>>,
    active: Option<Arc<str>>,
    last: Option<Arc<str>>,
}

impl ChatTabs {
    fn remove(&mut self, id: &str) {
        self.tabs.retain(|tab| **tab != *id);
        self.files.retain(|_, tab| **tab != *id);
        if self.active.as_deref() == Some(id) {
            self.active = None;
        }
        if self.last.as_deref() == Some(id) {
            self.last = None;
        }
    }
}

#[derive(Default)]
pub(in crate::ui) struct TabState {
    pages: BTreeMap<TabOwner, ChatTabs>,
}

impl TabState {
    pub(in crate::ui) fn remember(&mut self, tabs: &[PageTab], active: usize) {
        let page = self.pages.entry(TabOwner::of(tabs)).or_default();
        page.tabs = tabs
            .iter()
            .filter_map(|tab| match tab {
                PageTab::Content(id) => Some(Arc::clone(id)),
                _ => None,
            })
            .collect();
        page.active = tabs.get(active).and_then(|tab| match tab {
            PageTab::Content(id) => Some(Arc::clone(id)),
            _ => None,
        });
        if page.active.is_some() {
            page.last.clone_from(&page.active);
        }
    }

    pub(in crate::ui) fn insert(&mut self, owner: TabOwner, id: Arc<str>) {
        let page = self.pages.entry(owner).or_default();
        if !page.tabs.contains(&id) {
            page.tabs.push(id);
        }
    }

    pub(in crate::ui) fn contains(&self, owner: &TabOwner, id: &str) -> bool {
        self.pages
            .get(owner)
            .is_some_and(|page| page.tabs.iter().any(|tab| **tab == *id))
    }

    pub(in crate::ui) fn owner(&self, id: &str) -> Option<&TabOwner> {
        self.pages
            .iter()
            .find_map(|(owner, page)| page.tabs.iter().any(|tab| **tab == *id).then_some(owner))
    }

    pub(in crate::ui) fn file_id(&self, owner: &TabOwner, id: Arc<str>) -> Arc<str> {
        if self.contains(owner, &id) {
            id
        } else if let Some(existing) = self.pages.get(owner).and_then(|page| page.files.get(&id)) {
            Arc::clone(existing)
        } else {
            format!("{owner:?}:file:{id}").into()
        }
    }

    pub(in crate::ui) fn register_file(&mut self, owner: &TabOwner, key: Arc<str>) -> Arc<str> {
        let id = self.file_id(owner, Arc::clone(&key));
        if key != id {
            self.pages
                .entry(owner.clone()) // rules-allow: the registry retains the caller's file owner
                .or_default()
                .files
                .insert(key, Arc::clone(&id));
        }
        id
    }

    pub(in crate::ui) fn remove_chat(&mut self, session: &SessionId) -> Vec<Arc<str>> {
        self.pages
            .remove(&TabOwner::Chat(session.share()))
            .map_or_else(Vec::new, |page| page.tabs)
    }

    pub(in crate::ui) fn remove(&mut self, id: &str) {
        for page in self.pages.values_mut() {
            page.remove(id);
        }
    }

    pub(in crate::ui) fn detach(&mut self, owner: &TabOwner, id: &str) {
        if let Some(page) = self.pages.get_mut(owner) {
            page.remove(id);
        }
    }

    pub(in crate::ui) fn last(&self, owner: &TabOwner) -> Option<&Arc<str>> {
        let page = self.pages.get(owner)?;
        page.last
            .as_ref()
            .filter(|id| page.tabs.contains(id))
            .or_else(|| page.tabs.first())
    }

    pub(in crate::ui) fn sessions(&self) -> impl Iterator<Item = &SessionId> {
        self.pages.iter().filter_map(|(owner, page)| match owner {
            TabOwner::Chat(id) if !page.tabs.is_empty() => Some(id),
            _ => None,
        })
    }

    pub(in crate::ui) fn activate(
        &mut self,
        tabs: &mut Vec<PageTab>,
        active: usize,
        target: PageTab,
    ) -> usize {
        let owner = TabOwner::of(tabs);
        if let PageTab::Content(id) = &target {
            if !self.contains(&owner, id) {
                return active;
            }
        } else {
            let next = TabOwner::of(std::slice::from_ref(&target));
            if next != owner {
                self.remember(tabs, active);
                *tabs = vec![target];
                if let Some(page) = self.pages.get(&next) {
                    tabs.extend(page.tabs.iter().map(|id| PageTab::Content(Arc::clone(id))));
                    return page
                        .active
                        .as_ref()
                        .and_then(|id| {
                            tabs.iter()
                                .position(|tab| matches!(tab, PageTab::Content(tab) if tab == id))
                        })
                        .unwrap_or(0);
                }
                return 0;
            }
        }
        let active = activate_page(tabs, target);
        self.remember(tabs, active);
        active
    }

    pub(in crate::ui) fn session_opened(
        &mut self,
        tabs: &mut Vec<PageTab>,
        active: usize,
        session: &SessionId,
    ) -> usize {
        let target = PageTab::Chat(session.share());
        let owner = TabOwner::of(tabs);
        if owner == TabOwner::Draft {
            self.remember(tabs, active);
            if let Some(page) = self.pages.remove(&TabOwner::Draft) {
                self.pages.insert(TabOwner::Chat(session.share()), page);
            }
            if let Some(primary) = tabs
                .iter_mut()
                .find(|page| !matches!(page, PageTab::Content(_)))
            {
                *primary = target;
            }
            return active;
        }
        if owner == TabOwner::Global || owner == TabOwner::Chat(session.share()) {
            return active;
        }
        self.activate(tabs, active, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_switch_restores_its_three_tabs_and_active_document() {
        let chat = |id: &str| PageTab::Chat(id.parse().unwrap());
        let mut state = TabState::default();
        let mut tabs = vec![chat("a")];
        let mut active = 0;
        for id in ["file", "diff", "terminal"] {
            state.insert(TabOwner::of(&tabs), id.into());
            active = state.activate(&mut tabs, active, PageTab::Content(id.into()));
        }
        active = state.activate(&mut tabs, active, PageTab::Content("diff".into()));
        active = state.activate(&mut tabs, active, chat("b"));
        assert_eq!(tabs, vec![chat("b")]);
        state.insert(TabOwner::of(&tabs), "b-file".into());
        active = state.activate(&mut tabs, active, PageTab::Content("b-file".into()));
        active = state.activate(&mut tabs, active, chat("a"));
        assert_eq!(
            tabs,
            vec![
                chat("a"),
                PageTab::Content("file".into()),
                PageTab::Content("diff".into()),
                PageTab::Content("terminal".into())
            ]
        );
        assert_eq!(tabs[active], PageTab::Content("diff".into()));
        // A late result for B stays parked and cannot open B's document in A.
        state.insert(TabOwner::Chat("b".parse().unwrap()), "late-b".into());
        active = state.activate(&mut tabs, active, PageTab::Content("late-b".into()));
        assert_eq!(tabs[active], PageTab::Content("diff".into()));
        active = state.activate(&mut tabs, active, chat("b"));
        assert!(tabs.contains(&PageTab::Content("late-b".into())));
        assert_eq!(tabs[active], PageTab::Content("b-file".into()));
    }

    #[test]
    fn created_chat_adopts_draft_tabs_without_losing_selection() {
        let mut state = TabState::default();
        let mut tabs = vec![PageTab::Draft];
        let file = state.register_file(&TabOwner::Draft, "/project/README.md".into());
        state.insert(TabOwner::Draft, Arc::clone(&file));
        let active = state.activate(&mut tabs, 0, PageTab::Content(Arc::clone(&file)));
        state.insert(TabOwner::Draft, "terminal".into());
        let active = state.activate(&mut tabs, active, PageTab::Content("terminal".into()));
        let session = "created".parse().unwrap();
        let mut active = state.session_opened(&mut tabs, active, &session);
        assert_eq!(tabs[0], PageTab::Chat(session));
        assert_eq!(tabs[active], PageTab::Content("terminal".into()));
        assert_eq!(
            state.file_id(&TabOwner::of(&tabs), "/project/README.md".into()),
            file,
            "reopening a draft file after chat creation must reuse its edit buffer"
        );
        state.remove("terminal");
        assert_eq!(state.last(&TabOwner::of(&tabs)), Some(&file));
        let page = PageTab::Settings(SettingsPage::Appearance);
        active = state.activate(&mut tabs, active, page.clone());
        active = state.session_opened(&mut tabs, active, &"resumed".parse().unwrap());
        assert_eq!(
            tabs[active], page,
            "background resume must keep the selected page"
        );
    }

    #[test]
    fn closing_a_shared_machine_tab_detaches_only_its_chat() {
        let a = TabOwner::Chat("a".parse().unwrap());
        let b = TabOwner::Chat("b".parse().unwrap());
        let mut state = TabState::default();
        state.insert(a.clone(), "computer".into()); // rules-allow: the test retains A to close only its tab
        state.insert(b.clone(), "computer".into()); // rules-allow: the test retains B to verify its tab remains
        state.detach(&a, "computer");
        assert!(!state.contains(&a, "computer"));
        assert!(state.contains(&b, "computer"));
        assert_eq!(state.owner("computer"), Some(&b));
    }
}
