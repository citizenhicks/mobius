mod catalog;
mod chats;
mod commands;
mod history;
mod menus;
mod projects;
mod recaps;
pub(in crate::ui) mod shortcuts;
mod sidebar;
mod switcher;
mod updates;
pub(in crate::ui) use sidebar::{GATEWAY_ITEM, sidebar_list};

pub(super) use history::History;
pub use menus::install_menus;
pub(in crate::ui) use menus::{ActiveShortcuts, apply_shortcuts};
pub(in crate::ui) use updates::{CheckForUpdates, UpdateUi};

pub(super) use catalog::CatalogState;
#[cfg(feature = "snapshot")]
pub(in crate::ui) use catalog::Organization;
pub(in crate::ui) use catalog::{project_id, session_title, workspace_name};
#[cfg(feature = "snapshot")]
pub(in crate::ui) use recaps::RecapBody;

use super::*;
use mobius::SessionId;
use mobius_gateway::wire::{HarnessKind, ServerMessage};

pub(in crate::ui) enum NoticeAction {
    Reveal(std::path::PathBuf),
    Storage,
    Issue(crate::ui::transcript::IssueAction),
    /// Settings → Security, where computer control is set up.
    ComputerSetup,
}

/// Package-owned UI state; new fields belong here rather than on `Workspace`.
pub(in crate::ui) struct ShellState {
    /// Saved endpoints, reloaded at account refresh and pairing boundaries.
    pub(in crate::ui) saved_endpoints: Vec<String>,
    /// Toasts with a tone, drained by the runtime poll.
    /// With a file to show in Finder when the toast is clicked, such as a download.
    pub(in crate::ui) notices: Vec<(Tone, String, Option<NoticeAction>)>,
    /// The notice on screen; the rest of `notices` waits behind it.
    pub(in crate::ui) toast: Option<crate::ui::runtime::Toast>,
    pub(in crate::ui) next_toast_id: u64,
    pub(in crate::ui) sidebar_width: Pixels,
    pub(in crate::ui) pinned_bots: BTreeMap<String, String>,
    pub(in crate::ui) last_sessions: BTreeMap<String, String>,
    pub(in crate::ui) full_access_notices: std::collections::BTreeSet<String>,
    /// The last chat has been offered once, on the first Ready.
    pub(in crate::ui) restored: bool,
    /// The "finish computer control setup" toast has shown this launch.
    pub(in crate::ui) computer_setup_noticed: bool,
    /// The usage lines the menu bar item last showed.
    pub(in crate::ui) menu_bar_usage: Vec<String>,
    pub(in crate::ui) updates: UpdateUi,
    /// The list and item whose rows showed last frame (`disclosed_item`).
    disclosed: (Section, Option<String>),
    /// Rows easing open or closed beneath their item.
    disclosures: motion::Reveals,
    /// "While you were away" recaps and each chat's last-read point.
    pub(in crate::ui) recaps: recaps::RecapState,
    pub(in crate::ui) bot_rail_scroll: ScrollHandle,
    pub(in crate::ui) catalog_scroll: ScrollHandle,
}

impl Default for ShellState {
    fn default() -> Self {
        Self::restored(&mut super::preferences::Preferences::default())
    }
}

impl ShellState {
    pub(in crate::ui) fn restored(preferences: &mut super::preferences::Preferences) -> Self {
        Self {
            saved_endpoints: crate::gateway::saved_endpoints().unwrap_or_default(),
            notices: Vec::new(),
            toast: None,
            next_toast_id: 0,
            sidebar_width: preferences
                .sidebar_width
                .map_or(width::SIDEBAR, px)
                .clamp(width::SIDEBAR_MIN, width::SIDEBAR_MAX),
            last_sessions: std::mem::take(&mut preferences.last_sessions),
            pinned_bots: std::mem::take(&mut preferences.pinned_bots),
            full_access_notices: std::mem::take(&mut preferences.full_access_notices),
            restored: false,
            computer_setup_noticed: false,
            menu_bar_usage: Vec::new(),
            updates: UpdateUi::default(),
            disclosed: (Section::default(), None),
            disclosures: motion::Reveals::default(),
            recaps: recaps::RecapState::default(),
            bot_rail_scroll: ScrollHandle::new(),
            catalog_scroll: ScrollHandle::new(),
        }
    }
}

impl ShellState {
    pub(in crate::ui) fn reload_gateways(&mut self) {
        self.saved_endpoints = crate::gateway::saved_endpoints().unwrap_or_default();
    }
}

impl Workspace {
    pub(in crate::ui) fn shell_on_message(
        &mut self,
        message: &ServerMessage,
        window: &Window,
        cx: &Context<Self>,
    ) {
        match message {
            ServerMessage::AgentEvent { .. } => {}
            ServerMessage::SessionOpened { payload, .. } => {
                let id = &payload.session.session_id;
                self.set_unread(id, false);
                if let Some(session) = self.client.state.session_record(id) {
                    self.shell.recaps.read(session);
                }
                self.persist_shell(cx);
            }
            _ => self.track_activity(window),
        }
    }
}

#[derive(Clone, Copy, Default, Eq, PartialEq, Debug)]
pub(super) enum Section {
    #[default]
    Chats,
    Bots,
    Gateway,
    /// The möbius harness: its providers, extensions and agent defaults.
    Mobius,
    Settings,
    Events,
}

impl Section {
    pub(super) const ALL: [Self; 6] = [
        Self::Chats,
        Self::Bots,
        Self::Gateway,
        Self::Mobius,
        Self::Settings,
        Self::Events,
    ];

    const WORK: [Self; 2] = [Self::Chats, Self::Bots];
    const GATEWAY: [Self; 2] = [Self::Gateway, Self::Mobius];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Chats => "Chats",
            Self::Bots => "Bots",
            Self::Gateway => "Gateway",
            Self::Settings => "Settings",
            Self::Events => "Event Centre",
            Self::Mobius => pages::harness::name(HarnessKind::Mobius),
        }
    }

    pub(super) fn glyph(self) -> &'static str {
        match self {
            Self::Chats => "hi.note01",
            Self::Bots => "bot.face",
            Self::Gateway => "hi.cellTower",
            Self::Settings => "hi.gear",
            Self::Events => "hi.bell",
            Self::Mobius => pages::harness::glyph(HarnessKind::Mobius),
        }
    }

    /// The rail's glyph: the outline at rest and the filled one when selected, or the
    /// möbius logo.
    fn rail_glyphs(self, attention: bool) -> RailGlyph {
        let (outline, filled) = match self {
            Self::Mobius => return RailGlyph::MobiusLogo,
            Self::Bots => ("bot.face.line", "bot.face"),
            Self::Events if attention => ("hi.bellDot", "hi.bellDot.fill"),
            Self::Chats => ("hi.note01", "hi.note01.fill"),
            Self::Gateway => ("hi.cellTower", "hi.cellTower.fill"),
            Self::Settings => ("hi.gear", "hi.gear.fill"),
            Self::Events => ("hi.bell", "hi.bell.fill"),
        };
        RailGlyph::Pair(outline, filled)
    }
}

impl Workspace {
    pub(super) fn toggle_navigation(&mut self, cx: &mut Context<Self>) {
        self.navigation_visible = !self.navigation_visible;
        self.persist_shell(cx);
        cx.notify();
    }

    pub(super) fn select_section(
        &mut self,
        section: Section,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigation_section = section;
        self.navigation_visible = true;
        match section {
            Section::Bots => {
                if let Some(id) = self
                    .client
                    .state
                    .gateway
                    .as_ref()
                    .and_then(|gateway| gateway.bots.first())
                    .map(|bot| Arc::clone(&bot.id))
                {
                    self.open_bot_page(id, window, cx);
                } else {
                    self.open_page(PageTab::Empty(section), window, cx);
                }
            }
            Section::Mobius => {
                let first = self
                    .client
                    .state
                    .gateway
                    .as_ref()
                    .and_then(|gateway| gateway.provider_instances.first())
                    .map(|provider| provider.selection.instance.clone()); // rules-allow: the page owns its provider id while the gateway keeps its own
                let page = first.map_or(PageTab::Providers, PageTab::Provider);
                self.open_page(page, window, cx);
            }
            Section::Settings => {
                self.open_page(PageTab::Settings(SettingsPage::Account), window, cx);
            }
            Section::Gateway => {
                let page = GatewayPage::Machine(self.current_gateway());
                self.open_page(PageTab::Gateway(page), window, cx);
            }
            Section::Chats => {
                if let Some(id) = self
                    .client
                    .state
                    .selected_session_id
                    .as_ref()
                    .map(SessionId::share)
                {
                    self.open_page(PageTab::Chat(id), window, cx);
                } else if !self.draft_selected() {
                    self.new_chat(window, cx);
                }
            }
            Section::Events => {
                self.open_page(PageTab::Empty(section), window, cx);
            }
        }
        cx.notify();
    }

    fn contribution_open(&self) -> Option<&str> {
        match self.primary_page() {
            Some(PageTab::Contribution(key)) if self.is_navigation_contribution(key) => Some(key),
            _ => None,
        }
    }

    fn rail_section(&self, section: Section, cx: &Context<Self>) -> Button {
        let selected = self.navigation_section == section && self.contribution_open().is_none();
        let attention = section == Section::Events && self.events_need_attention();
        rail_section_button(
            section.label(),
            section.label(),
            section.rail_glyphs(attention),
            selected,
            cx,
        )
        .when(attention && !selected, |button| {
            button.text_color(self.palette.warning)
        })
        .on_click(cx.listener(move |this, _, window, cx| {
            this.select_section(section, window, cx);
        }))
    }

    pub(super) fn navigation_rail(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .id("navigation-rail")
            .w(width::RAIL)
            .h_full()
            .flex_shrink_0()
            .pt(space::S)
            .pb(space::S)
            .flex()
            .flex_col()
            .items_center()
            .gap(space::S)
            .children(self.pinned_bot_button(cx))
            .children(Section::WORK.map(|section| self.rail_section(section, cx)))
            .children(self.navigation_rail_widgets(cx))
            .child(
                hairline(&self.palette)
                    .flex_shrink_0()
                    .w(px(20.))
                    .my(space::XS),
            )
            .children(Section::GATEWAY.map(|section| self.rail_section(section, cx)))
            .child(div().flex_1())
            .child(self.update_rail_button(cx))
            .child(self.rail_section(Section::Events, cx))
            .child(self.rail_section(Section::Settings, cx))
            .into_any_element()
    }

    pub(super) fn navigation_context(&self, cx: &mut Context<Self>) -> AnyElement {
        let title = self.contribution_open().map_or_else(
            || self.navigation_section.label().to_owned(),
            |key| self.contribution_title(key),
        );
        let chats = self.contribution_open().is_none() && self.navigation_section == Section::Chats;
        let actions = if chats {
            self.chats_actions(cx)
        } else {
            vec![
                toolbar_button("search", "Search Möbius · ⌘K", "hi.magnifyingGlass", cx)
                    .on_click(cx.listener(|this, _, window, cx| this.command_palette(window, cx)))
                    .into_any_element(),
            ]
        };
        div()
            .id("navigation-context")
            .w_full()
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .px(space::S)
            .child(
                sidebar_header(title, actions)
                    .pb(space::S)
                    .child(div().flex().child(self.gateway_switcher(cx))),
            )
            .when_some(self.contribution_open(), |view, key| {
                view.child(if key == crate::ui::pages::memory::PAGE {
                    self.memory_sidebar(cx)
                } else {
                    div().flex_1().into_any_element()
                })
            })
            .when(chats, |view| {
                view.child({
                    let key = region::key((
                        self.gateway_revision,
                        self.regions.catalog_revision,
                        self.selected_chat(),
                        &self.client.state.selected_session_id,
                        &self.navigation_catalog,
                        self.client
                            .state
                            .gateway
                            .iter()
                            .flat_map(|gateway| &gateway.bots)
                            .map(|bot| self.bot_mood(&bot.id))
                            .collect::<Vec<_>>(),
                        (self.appearance, self.accent, self.client.link.is_ready()),
                    ));
                    let style = StyleRefinement::default()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0();
                    div()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .child(Self::cached_region(&self.regions.catalog, key, style, cx))
                })
            })
            .when(!chats && self.contribution_open().is_none(), |view| {
                view.child(self.section_list(cx))
            })
            .into_any_element()
    }
}
