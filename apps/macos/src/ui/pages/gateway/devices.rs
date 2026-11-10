//! Devices paired with the connected gateway, on one page where each can be unpaired.

use super::*;
use mobius_gateway::wire::ClientStatus;

fn connection_label(connections: usize) -> String {
    match connections {
        0 => "Offline".into(),
        1 => "Connected".into(),
        count => format!("{count} connections"),
    }
}

impl Workspace {
    pub(in crate::ui) fn refresh_devices(&mut self, cx: &mut Context<Self>) {
        self.send(
            ClientMessage::ListClients {
                request_id: crate::gateway::request_id(),
            },
            cx,
        );
    }

    /// Connected devices first, then by name.
    fn sorted_clients(&self) -> Option<(&str, Vec<&ClientStatus>)> {
        let (current, clients) = self.client.state.clients.as_ref()?;
        let mut clients: Vec<_> = clients.iter().collect();
        clients.sort_by_key(|client| (client.connections == 0, client.label.to_lowercase()));
        Some((current, clients))
    }

    /// The connected gateway's paired devices, each with Unpair except this Mac's own.
    pub(super) fn devices_page(&self, cx: &Context<Self>) -> AnyElement {
        let header =
            PageHeader::new("Devices").subtitle(format!("Paired with {}", self.gateway_name()));
        let page = page("gateway-devices", header);
        let Some((current, clients)) = self.sorted_clients() else {
            let empty = if self.client.link.is_ready() {
                EmptyState::new("hi.smartPhone01", "Loading devices").loading()
            } else {
                EmptyState::new("hi.smartPhone01", "Not connected")
                    .detail("Connect to this gateway to see the devices paired with it.")
            };
            return page.child(empty).into_any_element();
        };
        let mut group = Group::new()
            .footer("An unpaired device loses access and must pair again to reconnect.");
        for client in clients {
            let status = connection_label(client.connections);
            let row = Row::new(
                SharedString::from(format!("device:{}", client.client_id)),
                client.label.as_str(),
            )
            .leading(Leading::glyph("hi.hardDrives"))
            .subtitle(status);
            group = group.child(if client.client_id == current {
                row.control(status_label(self.palette.muted, "This device", cx))
            } else {
                let id = SharedString::from(&client.client_id);
                row.control(
                    button(SharedString::from(format!("unpair:{id}")), "Unpair…")
                        .danger()
                        .outline()
                        .disabled(!self.client.link.is_ready())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.unpair_device_dialog(&id, window, cx);
                        })),
                )
            });
        }
        page.child(Section::new("Paired devices").child(group))
            .into_any_element()
    }

    fn unpair_device_dialog(&self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, clients)) = &self.client.state.clients else {
            return;
        };
        let Some(client) = clients.iter().find(|client| client.client_id == id) else {
            return;
        };
        let client_id = SharedString::from(&client.client_id);
        self.confirm_destructive(
            format!("Unpair {}?", client.label),
            "This device will lose access and must pair again to reconnect.",
            "Unpair device",
            window,
            cx,
            move |this, _, cx| {
                this.send(
                    ClientMessage::UnpairClient {
                        request_id: crate::gateway::request_id(),
                        client_id: client_id.to_string(),
                    },
                    cx,
                );
                this.refresh_devices(cx);
            },
        );
    }
}
