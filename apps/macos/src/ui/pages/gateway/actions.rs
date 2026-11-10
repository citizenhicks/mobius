//! A gateway machine's header actions (GatewayDetailView's toolbar): Forget, Re-pair, Rename,
//! Connect or Reconnect, and Create code.

use super::*;
use gpui_component::WindowExt;
use gpui_component::button::ButtonVariant;
use gpui_component::dialog::DialogButtonProps;
use std::rc::Rc;

impl Workspace {
    pub(in crate::ui) fn connection(gateway: &GatewayRef) -> Connection {
        match gateway {
            GatewayRef::Local => Connection::Local,
            GatewayRef::Saved(endpoint) => Connection::Saved(endpoint.clone()), // rules-allow: Connection owns its endpoint, the GatewayRef stays with the router
        }
    }

    /// Destructive first and the primary action last, as every page header orders them.
    pub(super) fn machine_actions(
        &self,
        gateway: &GatewayRef,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let active = self.is_current(gateway);
        let mut actions = Vec::new();
        if let GatewayRef::Saved(endpoint) = gateway {
            let forget = SharedString::from(endpoint);
            let repair = SharedString::clone(&forget);
            actions.push(
                toolbar_button("forget-gateway", "Forget gateway…", "hi.trash", cx)
                    .on_click(cx.listener(move |_, _, window, cx| {
                        Self::forget_gateway_dialog(&forget, window, cx);
                    }))
                    .into_any_element(),
            );
            actions.push(
                toolbar_button("repair-gateway", "Re-pair…", "hi.link", cx)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.repair_gateway_dialog(&repair, window, cx);
                    }))
                    .into_any_element(),
            );
        }
        let rename = Rc::new(gateway.clone()); // rules-allow: the 'static listeners share one copy of the page's gateway
        let target = Rc::clone(&rename);
        actions.push(
            toolbar_button("rename-gateway", "Rename gateway…", "hi.pencilSimple", cx)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.rename_gateway_dialog(Rc::clone(&rename), window, cx);
                }))
                .into_any_element(),
        );
        let connect =
            cx.listener(move |this, _, _, cx| this.connect(Self::connection(&target), cx));
        actions.push(if active {
            toolbar_button("reconnect-gateway", "Reconnect", "hi.refresh03", cx)
                .on_click(connect)
                .into_any_element()
        } else {
            toolbar_button("connect-gateway", "Connect", "hi.plugsConnected", cx)
                .text_color(self.palette.accent)
                .on_click(connect)
                .into_any_element()
        });
        if active && self.client.link.is_ready() && !self.pairing_code_valid() {
            actions.push(
                toolbar_button("create-pairing-code", "Create one-time code", "hi.key", cx)
                    .text_color(self.palette.accent)
                    .on_click(cx.listener(|this, _, _, cx| this.create_pairing_code(cx)))
                    .into_any_element(),
            );
        }
        actions
    }

    pub(super) fn create_pairing_code(&mut self, cx: &mut Context<Self>) {
        let request_id = crate::gateway::request_id();
        self.send(ClientMessage::CreatePairingCode { request_id }, cx);
    }

    pub(super) fn pairing_code_valid(&self) -> bool {
        let now = chrono::Utc::now().timestamp();
        self.client
            .state
            .pairing_code
            .as_ref()
            .is_some_and(|(_, expires)| *expires > now)
    }

    /// The Pair dialog with this gateway's address filled in, for a fresh one-time code.
    fn repair_gateway_dialog(&self, endpoint: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.endpoint.update(cx, |input, cx| {
            input.set_value(endpoint.to_owned(), window, cx);
        });
        self.pairing_code
            .update(cx, |input, cx| input.set_value(String::new(), window, cx));
        self.pair_gateway_dialog(window, cx);
    }

    /// Names the gateway on this Mac only (GatewayAccount.displayName).
    pub(in crate::ui) fn rename_gateway_dialog(
        &self,
        gateway: impl Into<Rc<GatewayRef>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let gateway: Rc<GatewayRef> = gateway.into();
        let current: Rc<str> = self.gateway_display_name(&gateway).into();
        let name = field(&current, default_name(&gateway), window, cx);
        let (view, muted, focus) = (
            cx.entity().downgrade(),
            self.palette.muted,
            Entity::clone(&name),
        );
        window.open_dialog(cx, move |dialog, _, _| {
            let (name, view, gateway) = (
                Entity::clone(&name),
                WeakEntity::clone(&view),
                Rc::clone(&gateway),
            );
            let (input, current) = (Entity::clone(&name), Rc::clone(&current));
            let submit = move |_: &mut Window, cx: &mut App| {
                let value = name.read(cx).value().trim().to_owned();
                if *value == *current {
                    return true;
                }
                view.update(cx, |this, cx| this.rename_gateway(&gateway, value, cx))
                    .is_ok()
            };
            dialog
                .title(heading("Rename gateway"))
                .w(width::DIALOG)
                .child(
                    dialog_body()
                        .child(div().typography(Type::Caption).text_color(muted).child(
                            "The name shows on this Mac only. Leave it empty to use the default.",
                        ))
                        .child(labelled(
                            "Gateway name",
                            text_field(&input).aria_label("Gateway name"),
                            muted,
                        )),
                )
                .map(|dialog| dialog_actions(dialog, "Rename", true, submit))
        });
        window.defer(cx, move |window, cx| {
            focus.update(cx, |input, cx| input.focus(window, cx));
        });
    }

    fn rename_gateway(&mut self, gateway: &GatewayRef, name: String, cx: &mut Context<Self>) {
        let key = gateway.label().to_owned();
        if name.is_empty() || name == default_name(gateway) {
            self.config.gateway_names.remove(&key);
        } else {
            self.config.gateway_names.insert(key, name);
        }
        let names = self.config.gateway_names.clone(); // rules-allow: the saved preferences keep their own copy of the workspace's map
        self.save_preferences(
            "gateway names",
            |preferences| preferences.gateway_names = names,
            cx,
        );
        cx.notify();
    }

    fn forget_gateway_dialog(endpoint: &SharedString, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.entity().downgrade();
        let endpoint = SharedString::clone(endpoint);
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (view, endpoint) = (WeakEntity::clone(&view), SharedString::clone(&endpoint));
            alert
                .title(heading("Forget this gateway?"))
                .description("You will need to pair with this gateway again.")
                .width(width::DIALOG)
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Forget gateway")
                        .ok_variant(ButtonVariant::Danger)
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    view.update(cx, |this, cx| this.forget_gateway(&endpoint, window, cx))
                        .is_ok()
                })
        });
    }

    fn forget_gateway(&mut self, endpoint: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.forget_saved_gateway(endpoint, cx) {
            let page = GatewayPage::Machine(GatewayRef::Local);
            self.open_page(PageTab::Gateway(page), window, cx);
        }
    }

    /// Removes a saved gateway and its credential, returning to this Mac's gateway if it was in use.
    pub(in crate::ui) fn forget_saved_gateway(
        &mut self,
        endpoint: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let forgotten =
            mobius_gateway::gateway_accounts::GatewayAccounts::load().and_then(|mut accounts| {
                accounts.forget(endpoint);
                accounts.save()
            });
        if let Err(error) = forgotten {
            self.report(error.to_string());
            return false;
        }
        self.shell.reload_gateways();
        if !self.client.local_gateway && self.client.endpoint.as_str() == endpoint {
            self.connect(Connection::Local, cx);
        }
        true
    }
}
