//! The gateway link as the interface presents it, and the connection lifecycle:
//! user connects, automatic reconnects that keep the workspace, and gateway switches.

use super::*;
use mobius::SessionId;

/// How the interface colours each link state.
pub(in crate::ui) trait LinkTone {
    fn tone(&self) -> Tone;
}

impl LinkTone for Link {
    fn tone(&self) -> Tone {
        match self {
            Self::Connecting(_) => Tone::Info,
            Self::Ready => Tone::Success,
            Self::Reconnecting { .. } => Tone::Warning,
            Self::Failed(_) => Tone::Error,
        }
    }
}

impl Workspace {
    /// The single place that changes the connection state and tells the native bridge.
    pub(in crate::ui) fn set_link(&mut self, link: Link, cx: &mut Context<Self>) {
        if !link.is_ready() {
            self.bots.disconnect(cx);
            self.shell.recaps = Default::default();
        }
        self.native.set_connected(link.is_ready());
        self.client.set_link(link);
    }

    /// What the app calls the current gateway (`gateway_display_name`).
    pub(in crate::ui) fn gateway_name(&self) -> String {
        self.gateway_display_name(&self.current_gateway())
            .into_owned()
    }

    pub(in crate::ui) fn link_label(&self) -> String {
        match &self.client.link {
            Link::Connecting(_) if self.client.local_gateway => {
                "Starting the local gateway…".into()
            }
            Link::Connecting(label) if label.is_empty() => "Connecting…".into(),
            Link::Connecting(label) => format!("Connecting to {label}…"),
            Link::Ready if self.client.local_gateway => "Local gateway".into(),
            Link::Ready => format!("Connected to {}", self.gateway_name()),
            Link::Reconnecting { endpoint, .. } => {
                let gateway = endpoint.as_ref().map_or(GatewayRef::Local, |endpoint| {
                    GatewayRef::Saved(endpoint.clone()) // rules-allow: the link keeps its endpoint; the label needs a gateway of its own
                });
                format!("Reconnecting to {}…", self.gateway_display_name(&gateway))
            }
            Link::Failed(message) => format!("Disconnected: {message}"),
        }
    }

    /// Connects to `connection`; the gateway already open and ready is left alone.
    pub(in crate::ui) fn connect(&mut self, connection: Connection, cx: &mut Context<Self>) {
        match self.client.connect(connection) {
            Ok(false) => return,
            Ok(true) => self.set_link(Link::Connecting(String::new()), cx),
            Err(error) => self.report(error),
        }
        cx.notify();
    }

    pub(in crate::ui) fn connecting(
        &mut self,
        label: String,
        same_endpoint: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if same_endpoint {
            self.client.reopen = true;
            self.cancel_session_selection();
        } else {
            self.switch_gateway(window, cx);
        }
        self.set_link(Link::Connecting(label), cx);
    }

    /// Leaves the previous gateway's session state; pages that still mean something stay.
    fn switch_gateway(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.provider_setup.reset();
        self.bots.disconnect(cx);
        self.bots = Default::default();
        self.memory = Default::default();
        self.model_picker = None;
        self.expanded.clear();
        self.stop_voice();
        self.save_draft(cx);
        self.draft_workspace = UNSELECTED_FOLDER.into();
        self.draft_bot = None;
        self.interrupt_uploads();
        self.stop_dictation();
        self.cancel_session_selection();
        self.runtime.draft_owner = None;
        self.client.reopen = false;
        self.runtime.desktop_held = false;
        self.forget_agent_browsers(cx);
        self.forget_browsers();
        self.forget_desktops();
        for tab in self.content.tabs.values() {
            self.native.close_panel(&tab.id);
        }
        self.content.tabs.clear();
        self.content.navigation = Default::default();
        self.transcript_scrolls.clear();
        let primary = std::mem::take(&mut self.page_tabs)
            .into_iter()
            .find(|tab| !matches!(tab, PageTab::Content(_)))
            .map_or(PageTab::Draft, gateway_page);
        self.page_tabs = vec![primary];
        self.active_page = 0;
        self.history = Default::default();
        self.native.hide_panel();
        if !self.client.endpoint.is_empty() {
            self.composer
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.client.state = GatewayState::default();
        self.client.storage = Default::default();
        self.storage = Default::default();
        self.runtime.thumbnails.clear_files();
    }

    pub(super) fn connected(
        &mut self,
        endpoint: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.shell.reload_gateways();
        let resumed = self.client.connected(&endpoint);
        self.set_link(Link::Connecting(endpoint), cx);
        if !resumed {
            self.runtime.draft_owner = None;
            self.load_draft("", window, cx);
        }
    }

    pub(super) fn reconnecting(
        &mut self,
        endpoint: Option<String>,
        attempt: u32,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if attempt == 1 && message == crate::gateway::GATEWAY_STARTING {
            self.announce(
                Tone::Info,
                "Your gateway is starting. Reconnecting automatically.".into(),
            );
        } else if attempt == 1 {
            self.report(format!("Lost the gateway connection: {message}"));
        }
        self.client.reopen = true;
        self.runtime.desktop_held = false;
        self.cancel_session_selection();
        self.set_link(
            Link::Reconnecting {
                endpoint,
                attempt,
                message,
            },
            cx,
        );
    }

    /// The connection reopens `session_id` right behind authentication; when this window
    /// holds that chat's events, it keeps them and takes only the newer ones.
    pub(super) fn resuming(&mut self, request_id: Arc<str>, session_id: SessionId) {
        self.client.resume(request_id, session_id);
    }

    /// After Ready: re-open the chat a reconnect interrupted and land sections that lost their page.
    pub(super) fn resume_after_ready(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.client.settle() {
            let name = self.gateway_name();
            self.announce(Tone::Success, format!("Paired with {name}"));
            return;
        }
        if self
            .client
            .state
            .gateway
            .as_ref()
            .is_some_and(|gateway| gateway.provider_instances.is_empty())
        {
            let message = "Connect a model provider in Providers to start chatting.";
            self.announce(Tone::Info, message.into());
        }
        // A möbius Cloud gateway is named by its plan, which the Account page otherwise reads.
        if self.cloud_plan_unknown() {
            self.cloud(crate::host::platform::CloudAction::Refresh);
        }
        self.restore_last_chat(window, cx);
        self.select_primary_page(window, cx);
    }

    /// Opens the primary page's chat unless it is open, or lands a section on its page.
    pub(super) fn select_primary_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((index, tab)) = self
            .page_tabs
            .iter()
            .enumerate()
            .find(|(_, tab)| !matches!(tab, PageTab::Content(_)))
        else {
            return;
        };
        let section = match tab {
            PageTab::Chat(_) => None,
            PageTab::Empty(navigation::Section::Bots) => Some(navigation::Section::Bots),
            PageTab::Providers => Some(navigation::Section::Mobius),
            _ => return,
        };
        match section {
            Some(section) => self.select_section(section, window, cx),
            None if !self.active_session_ready() => self.select_page(index, window, cx),
            None => {}
        }
    }
}

/// Where a page lands on another gateway: chats and documents become a draft.
fn gateway_page(page: PageTab) -> PageTab {
    use navigation::Section;
    match page {
        PageTab::Gateway(_) | PageTab::Settings(_) | PageTab::AgentDefaults(_) => page,
        PageTab::Event(_) | PageTab::Empty(Section::Events) => PageTab::Empty(Section::Events),
        PageTab::Bot(_) | PageTab::BotSettings(..) | PageTab::Empty(Section::Bots) => {
            PageTab::Empty(Section::Bots)
        }
        PageTab::Provider(_) | PageTab::ProviderDraft(_) | PageTab::Providers => PageTab::Providers,
        PageTab::Extension(_) | PageTab::Extensions => PageTab::Extensions,
        _ => PageTab::Draft,
    }
}
