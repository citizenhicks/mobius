//! One gateway machine: its connection, pairing codes and the explicit Connect action.

use super::*;

impl Workspace {
    pub(super) fn machine_page(&self, gateway: &GatewayRef, cx: &Context<Self>) -> AnyElement {
        let active = self.is_current(gateway);
        let endpoint = match gateway {
            GatewayRef::Local => self.local_endpoint(),
            GatewayRef::Saved(endpoint) => Some(endpoint.as_str()),
        };
        let title = self.gateway_display_name(gateway).into_owned();
        let status = if active {
            self.link_label()
        } else {
            "Not connected".to_owned()
        };
        let subtitle = [Some(status), endpoint.map(transport).map(str::to_owned)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        let header = PageHeader::new(title).subtitle(subtitle);
        let mut body = page(
            SharedString::from(format!("gateway:{}", gateway.label())),
            header,
        )
        .child(Section::new("Connection").child(self.connection_group(endpoint, active, cx)));
        if active && self.client.link.is_ready() {
            // Another device reaches the local gateway over its tunnel, not loopback.
            let address = match gateway {
                GatewayRef::Local => crate::gateway::local::public_endpoint()
                    .map(Cow::Owned)
                    .or_else(|| endpoint.map(Cow::Borrowed)),
                GatewayRef::Saved(endpoint) => Some(Cow::Borrowed(endpoint.as_str())),
            };
            body = body.child(self.pairing_section(address.as_deref(), cx));
        }
        body.into_any_element()
    }

    fn connection_group(&self, endpoint: Option<&str>, active: bool, cx: &Context<Self>) -> Group {
        let p = self.palette;
        let mut group = Group::new();
        if active {
            let color = match self.client.link {
                runtime::Link::Ready => p.signal,
                runtime::Link::Failed(_) => p.danger,
                runtime::Link::Connecting(_) | runtime::Link::Reconnecting { .. } => p.warning,
            };
            group = group.child(Row::new("gateway-status", "Status").control(status_label(
                color,
                self.link_label(),
                cx,
            )));
        }
        if let Some(endpoint) = endpoint {
            group = group
                .child(
                    Row::new("gateway-endpoint", "Endpoint")
                        .mono(endpoint.to_owned())
                        .control(copy_button("copy-endpoint", "Copy endpoint", endpoint, cx)),
                )
                .child(Row::new("gateway-transport", "Transport").value(transport(endpoint)));
        }
        if let Some(ready) = self.client.state.gateway.as_ref().filter(|_| active) {
            group = group
                .child(Row::new("gateway-version", "Version").mono(&ready.gateway_version))
                .child(
                    Row::new("gateway-protocol", "Wire protocol")
                        .mono(format!("v{}", mobius_gateway::wire::PROTOCOL_VERSION)),
                )
                .child(Row::new("gateway-bots", "Bots").value(ready.bots.len().to_string()))
                .child(
                    Row::new("gateway-chats", "Chats")
                        .value(self.client.state.sessions.len().to_string()),
                )
                .child(
                    Row::new("gateway-limit", "Active chat limit")
                        .value(ready.max_active_sessions.to_string()),
                );
        }
        group
    }

    fn pairing_section(&self, endpoint: Option<&str>, cx: &Context<Self>) -> Section {
        let mut group = Group::new()
            .footer("Enter the address and a fresh code on the other device to pair it.");
        if let Some(endpoint) = endpoint {
            group = group.child(
                Row::new("pair-address", "Address")
                    .mono(endpoint.to_owned())
                    .control(copy_button("copy-address", "Copy address", endpoint, cx)),
            );
        }
        let now = chrono::Utc::now().timestamp();
        match self
            .client
            .state
            .pairing_code
            .as_ref()
            .filter(|(_, expires)| *expires > now)
        {
            Some((code, expires)) => {
                let minutes = (*expires - now + 59) / 60;
                group = group.child(
                    Row::new("pair-code", "Code")
                        .subtitle(format!("Expires in {minutes} min"))
                        .mono(code.to_owned())
                        .control(copy_button("copy-pairing-code", "Copy code", code, cx)),
                );
            }
            // The page's one next step: creating the code the other device will enter.
            None => {
                group = group.child(
                    Row::new("pair-code", "Code")
                        .subtitle("Valid for a few minutes once created.")
                        .control(
                            settings_button("create-pairing-code", "New code")
                                .primary()
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.create_pairing_code(cx)),
                                ),
                        ),
                );
            }
        }
        Section::new("Pair another device").child(group)
    }

    /// Redraws every 30 seconds while a pairing code is valid, so its expiry stays current.
    pub(super) fn tick_pairing_code(&mut self, cx: &Context<Self>) {
        self.config.pairing_tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(30))
                    .await;
                let valid = this.update(cx, |this, cx| {
                    cx.notify();
                    this.pairing_code_valid()
                });
                if !valid.unwrap_or(false) {
                    break;
                }
            }
        }));
    }
}

fn copy_text(text: &str) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let text = text.to_owned();
    move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone())) // rules-allow: every clipboard write takes its own String
}

pub(super) fn copy_button(id: &'static str, label: &'static str, text: &str, cx: &App) -> Button {
    inline_button(id, label, "hi.copy", cx).on_click(copy_text(text))
}
