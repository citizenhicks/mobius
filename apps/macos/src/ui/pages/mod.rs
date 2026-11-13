mod background;
pub(super) mod bots;
mod cloud;
pub(in crate::ui) use cloud::SUBSCRIPTIONS;
mod events;
mod extensions;
mod gateway;
pub(super) mod harness;
pub(super) mod memory;
pub(super) mod providers;
mod settings;
mod shortcuts;
mod usage;
mod widgets;

use super::*;
use crate::gateway::Connection;
use gpui_component::{Disableable, WindowExt as _};
use mobius_gateway::wire::ClientMessage;

/// Config package UI state; new fields belong here rather than on `Workspace`.
#[derive(Default)]
pub(in crate::ui) struct ConfigUi {
    usage_aggregation: usage::Aggregation,
    /// The endpoint being paired, whose page shows the pairing progress.
    pairing_tick: Option<Task<()>>,
    /// Event id to the revision last read; kept for this session.
    event_reads: BTreeMap<String, String>,
    /// The native Cloud account state, as Swift last reported it.
    cloud: crate::host::platform::CloudStatus,
    /// Whether any Cloud status has arrived; until then the account shows as loading.
    cloud_loaded: bool,
    /// Names the user gave gateways, by gateway label.
    gateway_names: BTreeMap<String, String>,
    shortcuts: shortcuts::ShortcutsUi,
    pub(in crate::ui) color_edits: [Option<settings::ColorEdit>; 2],
    pub(in crate::ui) backdrop: crate::ui::backdrop::BackdropUi,
}

impl ConfigUi {
    pub(in crate::ui) fn restored(preferences: &mut super::preferences::Preferences) -> Self {
        Self {
            gateway_names: std::mem::take(&mut preferences.gateway_names),
            backdrop: crate::ui::backdrop::BackdropUi::restored(preferences),
            ..Self::default()
        }
    }
}

/// A pairing or device code on the inset fill.
pub(super) fn code_box(code: &str, p: &Palette) -> Div {
    div()
        .px(space::S)
        .py(space::S)
        .control_corners()
        .bg(p.inset)
        .typography(Type::CodeDisplay)
        .child(code.to_owned())
}

impl Workspace {
    /// The page's own actions for the titlebar's trailing edge, icon-only: destructive
    /// first and the primary action last, as the chat header orders its buttons.
    pub(in crate::ui) fn page_actions(
        &self,
        page: &PageTab,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        match page {
            PageTab::Empty(crate::ui::navigation::Section::Bots) => self
                .first_bot_id()
                .map_or_default(|id| self.bot_page_actions(&id, cx)),
            PageTab::Bot(id) => self.bot_page_actions(id, cx),
            PageTab::BotSettings(id, _) => {
                self.agent_status_actions(&bots::AgentScope::Bot(Arc::clone(id)), cx)
            }
            PageTab::AgentDefaults(_) => self.agent_status_actions(&bots::AgentScope::Defaults, cx),
            PageTab::Provider(_) | PageTab::ProviderDraft(_) => self.provider_actions_for(page, cx),
            PageTab::Extensions => self
                .first_extension_id()
                .map_or_default(|id| self.extension_actions(&id, cx)),
            PageTab::Extension(id) => self.extension_actions(id, cx),
            PageTab::Gateway(page) => self.gateway_actions(page, cx),
            PageTab::Settings(SettingsPage::Account) => self.account_actions(cx),
            PageTab::Settings(SettingsPage::Shortcuts) => Self::shortcut_actions(cx),
            PageTab::Event(id) => self.event_actions(id, cx),
            PageTab::Contribution(key) if key == memory::PAGE && self.memory.is_open() => {
                self.memory_actions(cx)
            }
            PageTab::Contribution(key) => self.contribution_actions(key, cx),
            PageTab::Draft
            | PageTab::Chat(_)
            | PageTab::Content(_)
            | PageTab::Empty(_)
            | PageTab::Providers
            | PageTab::Settings(_) => Vec::new(),
        }
    }
}
