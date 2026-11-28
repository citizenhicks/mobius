//! Gateway (WP5), a section of Settings: the connected machine's page, which opens its
//! routing, paired devices and host credentials.

mod actions;
mod routing;
pub(super) use routing::RoutingInputs;
mod credentials;
mod devices;
mod machine;
mod pair;

use super::*;
use mobius_gateway::wire::ServerMessage;
use std::borrow::Cow;

impl Workspace {
    pub(in crate::ui) fn config_on_message(
        &mut self,
        message: &ServerMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match message {
            ServerMessage::GatewayConfigured { request_id, .. } => {
                self.provider_registered(request_id, window, cx);
            }
            ServerMessage::PairingCode { .. } => self.tick_pairing_code(cx),
            _ => {}
        }
    }

    /// Refreshes the gateway records a Config page and its sidebar show.
    pub(in crate::ui) fn refresh_for_page(&mut self, page: &PageTab, cx: &mut Context<Self>) {
        match page {
            PageTab::Gateway(_) => {
                self.refresh_devices(cx);
                self.refresh_host_credentials(cx);
            }
            PageTab::Settings(SettingsPage::Account | SettingsPage::Usage)
            | PageTab::Provider(_)
            | PageTab::ProviderPart(..)
            | PageTab::Providers => self.refresh_profile(cx),
            PageTab::Event(id) => {
                self.mark_events_read(Some(id));
                self.refresh_events(cx);
            }
            PageTab::Empty(crate::ui::navigation::Section::Events) => self.refresh_events(cx),
            page if crate::ui::pages::memory::is_memory_page(page) => self.refresh_memories(cx),
            _ => {}
        }
    }

    pub(in crate::ui) fn render_gateway(
        &mut self,
        page: &GatewayPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match page {
            GatewayPage::Machine(gateway) => self.machine_page(gateway, cx),
            GatewayPage::Devices => self.devices_page(cx),
            GatewayPage::Routing => self.routing_page(window, cx),
            GatewayPage::GitHub => self.github_page(cx),
            GatewayPage::Ssh => self.ssh_page(cx),
        }
    }
}

/// "This Mac", or a saved gateway's host.
fn default_name(gateway: &GatewayRef) -> &str {
    match gateway {
        GatewayRef::Local => "This Mac",
        GatewayRef::Saved(endpoint) => host(endpoint),
    }
}

impl Workspace {
    /// Connected to a möbius Cloud gateway whose plan this Mac hasn't read yet.
    pub(in crate::ui) fn cloud_plan_unknown(&self) -> bool {
        !self.config.cloud.signed_in
            && !self.client.local_gateway
            && is_cloud(self.client.endpoint.as_str())
    }

    /// The connection's state in a word, as the Gateway card and page say it.
    fn link_word(&self) -> &'static str {
        match self.client.link {
            runtime::Link::Ready => "Ready",
            runtime::Link::Connecting(_) => "Connecting",
            runtime::Link::Reconnecting { .. } => "Reconnecting",
            runtime::Link::Failed(_) => "Not connected",
        }
    }

    /// The connected gateway's name and state, with the state's tone.
    pub(in crate::ui) fn gateway_summary(&self) -> (Hsla, String) {
        let tone = self.client.link.tone().color(&self.palette);
        (
            tone,
            format!("{} · {}", self.gateway_name(), self.link_word()),
        )
    }

    /// What the app calls `gateway`: the user's own name for it, then "möbius Cloud" or
    /// "möbius Cloud Plus" for a hosted gateway (as on iPhone), then the machine name while
    /// connected, then `default_name`.
    pub(in crate::ui) fn gateway_display_name<'a>(
        &'a self,
        gateway: &'a GatewayRef,
    ) -> Cow<'a, str> {
        if let Some(name) = self.config.gateway_names.get(gateway.label()) {
            return Cow::Borrowed(name);
        }
        if let GatewayRef::Saved(endpoint) = gateway
            && is_cloud(endpoint)
        {
            let cloud = &self.config.cloud;
            let plan = cloud.plan.as_deref().filter(|_| cloud.subscribed);
            return Cow::Owned(format!("möbius {}", plan.unwrap_or("Cloud")));
        }
        match &self.client.state.gateway {
            Some(ready) if self.is_current(gateway) => Cow::Borrowed(&ready.machine_name),
            _ => Cow::Borrowed(default_name(gateway)),
        }
    }
}

fn transport(endpoint: &str) -> &'static str {
    match endpoint.split_once("://").map(|(scheme, _)| scheme) {
        Some("wss") => "WebSocket TLS",
        Some("ws") => "WebSocket",
        Some("tls") => "TLS",
        _ => "Loopback TCP",
    }
}

impl Workspace {
    pub(super) fn gateway_actions(
        &self,
        page: &GatewayPage,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        match page {
            GatewayPage::Machine(gateway) => self.machine_actions(gateway, cx),
            GatewayPage::Ssh => vec![self.create_ssh_identity_button(cx)],
            GatewayPage::Devices | GatewayPage::GitHub | GatewayPage::Routing => Vec::new(),
        }
    }
}
