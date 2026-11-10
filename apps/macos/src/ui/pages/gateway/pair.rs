//! Pair gateway dialog (PairingView.swift); progress and errors arrive as toasts.

use super::*;
use gpui_component::WindowExt;
use std::cell::Cell;
use std::rc::Rc;

impl Workspace {
    /// Asks for another gateway's address and one-time code, then pairs and connects.
    pub(in crate::ui) fn pair_gateway_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        let (endpoint, code) = (
            Entity::clone(&self.endpoint),
            Entity::clone(&self.pairing_code),
        );
        endpoint.update(cx, |input, cx| {
            input.set_placeholder("wss://gateway.example", window, cx);
        });
        code.update(cx, |input, cx| {
            input.set_placeholder("One-time code", window, cx);
            input.set_masked(true, window, cx);
        });
        let view = cx.entity().downgrade();
        let (muted, danger) = (self.palette.muted, self.palette.danger);
        let missing = Rc::new(Cell::new(false));
        let focus = Entity::clone(&endpoint);
        window.open_dialog(cx, move |dialog, _, _| {
            let submit = {
                let (endpoint, code, view, missing) =
                    (Entity::clone(&endpoint), Entity::clone(&code), WeakEntity::clone(&view), Rc::clone(&missing));
                move |window: &mut Window, cx: &mut App| {
                    let address = endpoint.read(cx).value().trim().to_owned();
                    let secret = code.read(cx).value().trim().to_owned();
                    if address.is_empty() || secret.is_empty() {
                        missing.set(true);
                        window.refresh();
                        return false;
                    }
                    view.update(cx, |this, cx| this.pair_gateway(address, secret, cx))
                        .is_ok()
                }
            };
            dialog
                .title(heading("Pair a gateway"))
                .w(width::DIALOG)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(space::L)
                        .child(
                            div()
                                .typography(Type::Caption)
                                .text_color(muted)
                                .child("Enter the address and one-time code from the gateway you want to use. For möbius Cloud, create the code in the iPhone or iPad app. Bots are AI agents. They can make mistakes. Review important answers and actions."),
                        )
                        .child(labelled("Gateway address", text_field(&endpoint).aria_label("Gateway address"), muted))
                        .child(labelled("One-time code", text_field(&code).aria_label("One-time code"), muted))
                        .when(missing.get(), |body| {
                            body.child(
                                div()
                                    .typography(Type::Caption)
                                    .text_color(danger)
                                    .child("Enter the gateway address and its one-time code."),
                            )
                        }),
                )
                .map(|dialog| dialog_actions(dialog, "Pair", true, submit))
        });
        window.defer(cx, move |window, cx| {
            focus.update(cx, |input, cx| input.focus(window, cx));
        });
    }

    /// Pairs where the user is: the page stays put and toasts report progress.
    pub(in crate::ui) fn pair_gateway(
        &mut self,
        endpoint: String,
        code: String,
        cx: &mut Context<Self>,
    ) {
        let gateway = GatewayRef::Saved(endpoint);
        let message = format!("Pairing with {}…", self.gateway_display_name(&gateway));
        self.announce(Tone::Info, message);
        let GatewayRef::Saved(endpoint) = gateway else {
            unreachable!("built as a saved gateway above")
        };
        self.connect(Connection::Pair { endpoint, code }, cx);
    }
}
