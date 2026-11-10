//! Gateway (WP5): machines, paired devices and host credentials, each with a detail page.

mod actions;
mod credentials;
mod devices;
mod machine;
mod pair;

use super::*;
use crate::ui::navigation::GATEWAY_ITEM;
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
            ServerMessage::GatewayConfigured { .. } => self.provider_registered(window, cx),
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
        &self,
        page: &GatewayPage,
        cx: &Context<Self>,
    ) -> AnyElement {
        match page {
            GatewayPage::Machine(gateway) => self.machine_page(gateway, cx),
            GatewayPage::Devices => self.devices_page(cx),
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
            GatewayPage::Devices | GatewayPage::GitHub => Vec::new(),
        }
    }

    /// The Gateway list: Pair gateway, then the gateways; the connected one opens onto its
    /// overview, devices and host credentials, like a Bot onto its settings.
    pub(in crate::ui) fn gateway_list(&self, cx: &Context<Self>) -> AnyElement {
        let mut list = crate::ui::navigation::sidebar_list()
            .child(
                SidebarRow::new("pair-gateway", "Pair gateway")
                    .leading(Leading::glyph("hi.plus"))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.pair_gateway_dialog(window, cx)),
                    ),
            )
            .child(sidebar_label("Gateways"));
        let machines = std::iter::once(GatewayRef::Local).chain(
            self.saved_gateways()
                .map(|endpoint| GatewayRef::Saved(endpoint.to_owned())),
        );
        for gateway in machines {
            let active = self.is_current(&gateway);
            list = list.child(self.machine_row(gateway, active, cx));
            if active {
                list = list
                    .children(self.sidebar_disclosure(GATEWAY_ITEM, || self.gateway_children(cx)));
            }
        }
        list.into_any_element()
    }

    fn machine_row(&self, gateway: GatewayRef, active: bool, cx: &Context<Self>) -> SidebarRow {
        let p = self.palette;
        let title = self.gateway_display_name(&gateway).into_owned();
        let row = self.page_row(
            SharedString::from(format!("gateway:{}", gateway.label())),
            title,
            Leading::glyph(gateway.glyph()),
            PageTab::Gateway(GatewayPage::Machine(gateway)),
            cx,
        );
        if !active {
            return row;
        }
        row.selected(false)
            .caret(self.disclosure_shown(GATEWAY_ITEM) > 0.5)
            .status(status_dot(match self.client.link {
                runtime::Link::Ready => p.signal,
                runtime::Link::Failed(_) => p.danger,
                runtime::Link::Connecting(_) => p.muted,
                runtime::Link::Reconnecting { .. } => p.warning,
            }))
    }

    /// The connected gateway's overview, devices and host credentials, text aligned with
    /// its name.
    fn gateway_children(&self, cx: &Context<Self>) -> Vec<SidebarRow> {
        let overview = self.page_row(
            "gateway-overview",
            "Overview",
            Leading::Gutter,
            PageTab::Gateway(GatewayPage::Machine(self.current_gateway())),
            cx,
        );
        let mut devices = self.page_row(
            "gateway-devices",
            "Devices",
            Leading::Gutter,
            PageTab::Gateway(GatewayPage::Devices),
            cx,
        );
        if let Some((_, clients)) = &self.client.state.clients {
            devices = devices.meta(clients.len().to_string());
        }
        let [github, ssh] = self.credential_rows(cx);
        vec![overview, devices, github, ssh]
    }
}
