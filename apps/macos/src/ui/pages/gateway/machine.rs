//! One gateway machine: its connection, pairing codes and the explicit Connect action.

use super::*;

impl Workspace {
    pub(super) fn machine_page(&self, gateway: &GatewayRef, cx: &Context<Self>) -> AnyElement {
        let active = self.is_current(gateway);
        let endpoint = match gateway {
            GatewayRef::Local => self.local_endpoint(),
            GatewayRef::Saved(endpoint) => Some(endpoint.as_str()),
        };
        let subtitle = if active {
            self.gateway_summary().1
        } else {
            format!("{} · Not connected", self.gateway_display_name(gateway))
        };
        let mut body = page(
            SharedString::from(format!("gateway:{}", gateway.label())),
            PageHeader::new("Gateway").subtitle(subtitle),
        );
        let ready = active && self.client.link.is_ready();
        if ready {
            body = body.children(self.defaults_section(cx));
        }
        body = body
            .child(Section::new("Connection").child(self.connection_group(endpoint, active, cx)));
        if ready {
            // Another device reaches the local gateway over its tunnel, not loopback.
            let address = match gateway {
                GatewayRef::Local => crate::gateway::local::public_endpoint()
                    .map(Cow::Owned)
                    .or_else(|| endpoint.map(Cow::Borrowed)),
                GatewayRef::Saved(endpoint) => Some(Cow::Borrowed(endpoint.as_str())),
            };
            body = body
                .child(self.pairing_section(address.as_deref(), cx))
                .child(Section::new("Devices").child(Group::new().child(self.devices_row(cx))))
                .child(
                    Section::new("Host credentials")
                        .child(Group::new().children(self.credential_rows(cx))),
                );
        }
        body.into_any_element()
    }

    /// A row that opens one of the connected gateway's pages.
    pub(super) fn gateway_row(
        id: &'static str,
        title: &'static str,
        glyph: &'static str,
        page: GatewayPage,
        cx: &Context<Self>,
    ) -> Row {
        Row::new(id, title)
            .leading(Leading::glyph(glyph))
            .disclosure()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_page(PageTab::Gateway(page.clone()), window, cx); // rules-allow: the click handler is Fn and each open owns its page
            }))
    }

    /// What new chats start from: routing and the dictation model, both on one page.
    fn defaults_section(&self, cx: &Context<Self>) -> Option<Section> {
        let catalog = &self.client.state.gateway.as_ref()?.agent_catalog;
        let dictation = &catalog.dictation;
        let model = dictation
            .choices
            .iter()
            .find(|choice| choice.target == dictation.selection)
            .map(mobius_gateway::wire::DictationChoice::display);
        let routing = Self::gateway_row(
            "gateway-routing",
            "New chats",
            "hi.slidersHorizontal",
            GatewayPage::Routing,
            cx,
        )
        .subtitle("Auto routing");
        let dictation = Self::gateway_row(
            "gateway-dictation",
            "Default dictation model",
            "hi.mic01",
            GatewayPage::Routing,
            cx,
        );
        let dictation = match model {
            Some(model) => dictation.subtitle(model),
            None => dictation,
        };
        Some(Section::new("Defaults").child(Group::new().child(routing).child(dictation)))
    }

    fn devices_row(&self, cx: &Context<Self>) -> Row {
        let row = Self::gateway_row(
            "gateway-devices",
            "Paired devices",
            "hi.smartPhone01",
            GatewayPage::Devices,
            cx,
        );
        match &self.client.state.clients {
            Some((_, clients)) => row.value(clients.len().to_string()),
            None => row,
        }
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
            let status = Row::new("gateway-status", "Status").control(status_label(
                color,
                self.link_word(),
                cx,
            ));
            // A failure's reason is long; it reads under the title instead of beside it.
            group = group.child(if self.client.link.is_ready() {
                status
            } else {
                status.subtitle(self.link_label())
            });
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
