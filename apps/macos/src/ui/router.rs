//! Typed routes and page dispatch.

use super::{
    AnyElement, Arc, Context, IntoElement, ParentElement, SharedString, Styled, Window, Workspace,
    div, navigation, pages,
};
use mobius::SessionId;
use mobius_gateway::wire::HarnessKind;
use navigation::Section;

/// One page of the workspace: the primary route, or a companion document.
#[derive(Clone, PartialEq, Debug)]
pub(super) enum PageTab {
    Draft,
    Chat(SessionId),
    Content(Arc<str>),
    Empty(Section),
    Bot(Arc<str>),
    BotSettings(Arc<str>, AgentSection),
    AgentDefaults(AgentSection),
    Providers,
    Provider(String),
    ProviderDraft(String),
    Gateway(GatewayPage),
    Extensions,
    Extension(String),
    Settings(SettingsPage),
    Event(String),
    Contribution(String),
}

impl PageTab {
    /// The rail section that owns this page; documents and contributions keep the current one.
    pub(super) fn section(&self) -> Option<Section> {
        match self {
            Self::Draft | Self::Chat(_) => Some(Section::Chats),
            Self::Empty(section) => Some(*section),
            Self::Bot(_) | Self::BotSettings(..) => Some(Section::Bots),
            Self::AgentDefaults(_)
            | Self::Providers
            | Self::Provider(_)
            | Self::ProviderDraft(_)
            | Self::Extensions
            | Self::Extension(_) => Some(Section::Mobius),
            Self::Gateway(_) => Some(Section::Gateway),
            Self::Settings(_) => Some(Section::Settings),
            Self::Event(_) => Some(Section::Events),
            Self::Content(_) | Self::Contribution(_) => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum GatewayPage {
    Machine(GatewayRef),
    Devices,
    GitHub,
    Ssh,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum GatewayRef {
    Local,
    Saved(String),
}

impl GatewayRef {
    pub(super) fn label(&self) -> &str {
        match self {
            Self::Local => "This Mac",
            Self::Saved(endpoint) => endpoint,
        }
    }

    pub(super) fn glyph(&self) -> &'static str {
        match self {
            Self::Local => "hi.hardDrives",
            Self::Saved(_) => "hi.cellTower",
        }
    }
}

/// An endpoint's host, without its scheme, path or port.
pub(super) fn host(endpoint: &str) -> &str {
    let rest = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, rest)| rest);
    let authority = rest.split('/').next().unwrap_or(rest);
    authority
        .rsplit_once(':')
        .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
        .map_or(authority, |(host, _)| host)
}

pub(super) fn is_loopback(endpoint: &str) -> bool {
    matches!(host(endpoint), "127.0.0.1" | "localhost" | "[::1]" | "::1")
}

/// möbius Cloud's hosted gateways live at `mobius-<24 hex>.thinkingsand.dev`
/// (MobiusCloudClient.ownsGateway).
pub(super) fn is_cloud(endpoint: &str) -> bool {
    host(endpoint)
        .strip_prefix("mobius-")
        .and_then(|id| id.strip_suffix(".thinkingsand.dev"))
        .is_some_and(|id| id.len() == 24 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SettingsPage {
    Account,
    Appearance,
    Security,
    Data,
    Shortcuts,
    Usage,
    WhatsNew,
}

impl SettingsPage {
    pub(super) const ALL: [Self; 7] = [
        Self::Account,
        Self::Appearance,
        Self::Security,
        Self::Data,
        Self::Shortcuts,
        Self::Usage,
        Self::WhatsNew,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Account => "Account",
            Self::Appearance => "Appearance",
            Self::Security => "Security",
            Self::Data => "Data",
            Self::Shortcuts => "Keyboard shortcuts",
            Self::Usage => "Usage",
            Self::WhatsNew => "What’s new",
        }
    }

    pub(super) fn glyph(self) -> &'static str {
        match self {
            Self::Account => "hi.userFocus",
            Self::Appearance => "hi.paintBoard",
            Self::Security => "hi.shield02",
            Self::Data => "hi.hardDrives",
            Self::Shortcuts => "hi.typeCursor",
            Self::Usage => "hi.chartBarDecreasing",
            Self::WhatsNew => "hi.info",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum AgentSection {
    Identity,
    Prompt,
    Model,
    /// Whether a möbius Bot remembers across chats.
    Memory,
    Capabilities,
}

impl AgentSection {
    const MOBIUS: [Self; 5] = [
        Self::Identity,
        Self::Prompt,
        Self::Model,
        Self::Memory,
        Self::Capabilities,
    ];
    /// A Bot on a harness this app has no settings for shows only who it is.
    const IDENTITY: [Self; 1] = [Self::Identity];
    /// Agent defaults are möbius settings with no identity of their own.
    pub(super) const DEFAULTS: [Self; 4] =
        [Self::Prompt, Self::Model, Self::Memory, Self::Capabilities];

    /// The sections a Bot on `harness` shows, in order.
    pub(super) const fn for_harness(harness: HarnessKind) -> &'static [Self] {
        match harness {
            HarnessKind::Mobius => &Self::MOBIUS,
            HarnessKind::Codex | HarnessKind::Claude => &Self::IDENTITY,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Identity => "Identity",
            Self::Prompt => "System prompt",
            Self::Model => "Model",
            Self::Memory => "Memory",
            Self::Capabilities => "Capabilities",
        }
    }

    pub(super) fn glyph(self) -> &'static str {
        match self {
            Self::Identity => "hi.userFocus",
            Self::Prompt => "hi.text",
            Self::Model => "hi.aiScan",
            Self::Memory => "hi.stickyNote02",
            Self::Capabilities => "hi.slidersHorizontal",
        }
    }
}

/// Navigation replaces the current page; opened documents retain stable identities.
pub(super) fn activate_page(tabs: &mut Vec<PageTab>, tab: PageTab) -> usize {
    if !matches!(tab, PageTab::Content(_))
        && let Some((index, page)) = tabs
            .iter_mut()
            .enumerate()
            .find(|(_, page)| !matches!(page, PageTab::Content(_)))
    {
        *page = tab;
        return index;
    }
    tabs.iter()
        .position(|existing| existing == &tab)
        .unwrap_or_else(|| {
            tabs.push(tab);
            tabs.len() - 1
        })
}

pub(super) fn selected_after_close(selected: usize, closed: usize, remaining: usize) -> usize {
    selected
        .saturating_sub(usize::from(closed < selected))
        .min(remaining.saturating_sub(1))
}

impl Workspace {
    /// Saved gateways other than this Mac's own loopback gateway.
    pub(super) fn saved_gateways(&self) -> impl Iterator<Item = &str> {
        self.shell
            .saved_endpoints
            .iter()
            .map(String::as_str)
            .filter(|endpoint| !is_loopback(endpoint))
    }

    /// This Mac's gateway address: the live one while connected to it, else the saved one.
    pub(super) fn local_endpoint(&self) -> Option<&str> {
        let live = self.client.endpoint.as_str();
        if self.client.local_gateway && !live.is_empty() {
            return Some(live);
        }
        self.shell
            .saved_endpoints
            .iter()
            .map(String::as_str)
            .find(|endpoint| is_loopback(endpoint))
    }

    pub(super) fn render_page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let page = self.page_tabs.get(self.active_page).cloned(); // rules-allow: each page renders through the workspace mutably
        let Some(page) = page else {
            return self.render_chat(window, cx);
        };
        match page {
            PageTab::Chat(_) if self.active_session_ready() => {
                let chat = self.chat_page(window, cx);
                self.with_closing_pane(chat, window, cx)
            }
            PageTab::Chat(_) | PageTab::Draft | PageTab::Empty(Section::Chats) => {
                let chat = self.render_chat(window, cx);
                self.with_closing_pane(chat, window, cx)
            }
            PageTab::Content(_) => self.content_page(window, cx),
            PageTab::Contribution(key) if key == pages::memory::PAGE => self.render_memory(cx),
            PageTab::Contribution(key) => self.render_contribution(&key, cx),
            PageTab::Empty(Section::Bots) => self.render_bots(window, cx),
            PageTab::Providers => self.render_providers(cx),
            PageTab::Extensions => self.render_extensions(cx),
            PageTab::Empty(Section::Events) => self.render_events(cx),
            PageTab::Bot(id) => self.render_bot(&id, window, cx),
            PageTab::BotSettings(id, section) => {
                self.render_agent_page(pages::bots::AgentScope::Bot(id), section, window, cx)
            }
            PageTab::AgentDefaults(section) => {
                self.render_agent_page(pages::bots::AgentScope::Defaults, section, window, cx)
            }
            PageTab::Extension(id) => self.render_extension(&id),
            PageTab::Provider(id) => self.render_provider(&id, window, cx),
            PageTab::ProviderDraft(service) => self.render_provider_draft(&service, window, cx),
            PageTab::Gateway(page) => self.render_gateway(&page, cx),
            PageTab::Settings(page) => self.render_settings(page, window, cx),
            PageTab::Event(id) => self.render_event(&id),
            PageTab::Empty(_) => not_yet_available(),
        }
    }

    /// The titlebar names chats and documents only; every other page shows its own heading.
    pub(super) fn page_title(&self, tab: &PageTab) -> SharedString {
        match tab {
            PageTab::Content(id) => self
                .content
                .tabs
                .get(id)
                .map(|tab| SharedString::clone(&tab.title))
                .unwrap_or_default(),
            PageTab::Chat(id) => {
                if let Some(bot) = self.conversation_bot(id) {
                    return bot.name.to_string().into();
                }
                self.client.state.session_record(id).map_or_else(
                    || "New chat".into(),
                    |session| navigation::session_title(session).into(),
                )
            }
            _ => SharedString::default(),
        }
    }

    /// Asks the owning package to refresh what `page` shows.
    pub(super) fn load_page_data(&mut self, page: &PageTab, cx: &mut Context<Self>) {
        if *page == PageTab::Settings(SettingsPage::Account) {
            self.cloud(crate::host::platform::CloudAction::Refresh);
        }
        if *page == PageTab::Settings(SettingsPage::Data) {
            self.refresh_storage(cx);
            self.measure_local_cache(cx);
        }
        if !self.client.link.is_ready() {
            return;
        }
        match page {
            PageTab::Bot(_) | PageTab::Empty(Section::Bots) => self.refresh_routines(cx),
            _ => self.refresh_for_page(page, cx),
        }
    }
}

/// Placeholder for routes whose package has not shipped a page yet.
pub(super) fn not_yet_available() -> AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .child("Not yet available")
        .into_any_element()
}
