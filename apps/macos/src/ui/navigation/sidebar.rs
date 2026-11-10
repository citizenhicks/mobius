//! The context sidebar's list area: the per-section dispatch and shared rows (R1).
//! Each package owns its section's list; rows only navigate, except one action row.

use super::*;
use crate::numeric::ToFloat;
use std::time::Instant;

/// The key of the connected gateway's rows (`disclosed_item`).
pub(in crate::ui) const GATEWAY_ITEM: &str = "gateway";

impl Workspace {
    /// The list item whose rows show beneath it: the selected Bot, or the connected
    /// gateway while it or one of its pages is open.
    pub(in crate::ui) fn disclosed_item(&self) -> Option<&str> {
        match self.primary_page()? {
            PageTab::Bot(id) | PageTab::BotSettings(id, _) => Some(id),
            PageTab::Gateway(GatewayPage::Machine(gateway)) if self.is_current(gateway) => {
                Some(GATEWAY_ITEM)
            }
            PageTab::Gateway(GatewayPage::Devices | GatewayPage::GitHub | GatewayPage::Ssh) => {
                Some(GATEWAY_ITEM)
            }
            page => pages::harness::MobiusGroup::of(page).map(pages::harness::MobiusGroup::key),
        }
    }

    /// Eases the disclosed item's rows open and the previous item's closed within one
    /// list; another section's list appears as it is.
    pub(in crate::ui) fn track_disclosure(&mut self, window: &Window, cx: &App) {
        let now = Instant::now();
        if cx.reduce_motion() {
            self.shell.disclosures.clear();
        }
        let section = self.navigation_section;
        let (was_section, was) = &self.shell.disclosed;
        if *was_section != section || was.as_deref() != self.disclosed_item() {
            let open = self.disclosed_item().map(str::to_owned);
            let (was_section, was) = std::mem::replace(&mut self.shell.disclosed, (section, None));
            let still = was_section != section || cx.reduce_motion();
            let reveals = &mut self.shell.disclosures;
            if still {
                reveals.clear();
            }
            for (key, open) in [(&was, false), (&open, true)] {
                if let Some(key) = key.as_deref().filter(|_| !still) {
                    reveals.set(key, open, false, now);
                }
            }
            self.shell.disclosed.1 = open;
        }
        self.shell.disclosures.settle(now);
        if !self.shell.disclosures.is_empty() {
            window.request_animation_frame();
        }
    }

    /// How much of the item `key`'s rows show, from 0 to 1, as they ease open or closed.
    pub(in crate::ui) fn disclosure_shown(&self, key: &str) -> f32 {
        let open = self.disclosed_item() == Some(key);
        self.shell.disclosures.shown(key, open, Instant::now())
    }

    /// `rows` beneath the item `key`, revealed from the top as the item opens and hidden
    /// as it closes; nothing while it is closed.
    pub(in crate::ui) fn sidebar_disclosure(
        &self,
        key: &str,
        rows: impl FnOnce() -> Vec<SidebarRow>,
    ) -> Option<Div> {
        let shown = self.disclosure_shown(key);
        (shown > 0.).then(|| {
            let rows = rows();
            let full = height::REGULAR * rows.len().to_f32();
            div().h(full * shown).overflow_hidden().children(rows)
        })
    }

    /// The selected section's list; the Chats catalog is placed by the sidebar itself.
    pub(in crate::ui) fn section_list(&self, cx: &Context<Self>) -> AnyElement {
        match self.navigation_section {
            Section::Chats => div().into_any_element(),
            Section::Bots => self.bots_list(cx),
            Section::Gateway => self.gateway_list(cx),
            Section::Mobius => self.mobius_list(cx),
            Section::Settings => self.settings_list(cx),
            Section::Events => self.events_list(cx),
        }
    }

    /// A row that opens `page` and shows selected while `page` is the main page.
    pub(in crate::ui) fn page_row(
        &self,
        id: impl Into<SharedString>,
        title: impl Into<SharedString>,
        leading: Leading,
        page: PageTab,
        cx: &Context<Self>,
    ) -> SidebarRow {
        SidebarRow::new(id.into(), title.into())
            .leading(leading)
            .selected(self.primary_page() == Some(&page))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_page(page.clone(), window, cx); // rules-allow: the click handler is Fn and each open owns its page
            }))
    }
}

/// The scrolling container every section list uses.
pub(in crate::ui) fn sidebar_list() -> Stateful<Div> {
    div()
        .id("section-list")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .pb(space::S)
}
