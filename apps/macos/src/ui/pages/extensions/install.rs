//! The install dialog (ExtensionsView.swift `InstallExtensionForm`).

use super::super::bots::{Pending, dialog_heading};
use super::*;
use gpui_component::input::InputEvent;

impl Workspace {
    pub(in crate::ui) fn install_extension_dialog(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.install_dialog("", None, window, cx);
    }

    /// The install dialog; a rejected install reopens it with the gateway's message.
    pub(in crate::ui) fn install_dialog(
        &self,
        source: &str,
        error: Option<Arc<str>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = field(
            source,
            "https://github.com/owner/repository.git",
            window,
            cx,
        );
        cx.subscribe(&input, |_, _, _: &InputEvent, cx| cx.notify())
            .detach();
        let view = cx.entity().downgrade();
        let (epoch, ready) = (self.client.epoch, self.client.link.is_ready());
        let focus = Entity::clone(&input);
        window.open_dialog(cx, move |dialog, _, cx| {
            let p = *cx.global::<crate::ui::theme::Palette>();
            let enabled = ready && !input.read(cx).value().trim().is_empty();
            let (view, input) = (WeakEntity::clone(&view), Entity::clone(&input));
            dialog
                .w(width::DIALOG)
                .child(dialog_body().child(dialog_heading("hi.squaresFour", "Install extension", "Git URL", &p))
                .child(text_field(&input).aria_label("Extension Git URL"))
                .child(div().typography(Type::Caption).text_color(p.muted).child(
                    "An HTTPS Git URL, or a GitHub tree URL pointing at a branch and subdirectory. The gateway clones it, pins an immutable snapshot, and reads its package manifest.",
                ))
                .children(error.as_ref().map(|error| div().typography(Type::Caption).text_color(p.danger).child(SharedString::from(Arc::clone(error))))))
                .map(|dialog| dialog_actions(dialog, "Install", enabled, move |_, cx| {
                    let source = input.read(cx).value().trim().to_owned();
                    view.update(cx, |this, cx| {
                        this.submit_gateway_dialog(epoch, cx, |this, cx| {
                            let request_id = crate::gateway::request_id();
                            let message = ClientMessage::InstallExtension {
                                request_id: Arc::clone(&request_id),
                                source: source.clone(), // rules-allow: the install request and its pending entry each own the source
                                reference: None,
                                subdirectory: None,
                            };
                            this.track_bots_request(request_id, Pending::Install(source), message, cx);
                        })
                    })
                    .unwrap_or(false)
                }))
        });
        window.defer(cx, move |window, cx| {
            focus.update(cx, |input, cx| input.focus(window, cx));
        });
    }
}
