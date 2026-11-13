//! The New Bot dialog (BotsView.swift `NewBotForm`) and the footer every Bots dialog shares.

use super::*;
use gpui_component::input::{InputEvent, InputState, TextareaState};

/// A dialog's heading: a 22 glyph on an accent tile, the title and a caption.
pub(in crate::ui) fn dialog_heading(
    glyph_name: &'static str,
    title: impl Into<SharedString>,
    caption: impl Into<SharedString>,
    p: &crate::ui::theme::Palette,
) -> Div {
    div()
        .flex()
        .items_center()
        .gap(space::S)
        .child(
            div()
                .size(height::TOUCH)
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .hero_corners()
                .bg(p.accent_soft)
                .child(icon(glyph_name, p).size(glyph::ICON)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(space::XXS)
                .child(heading(title))
                .child(
                    div()
                        .typography(Type::Caption)
                        .text_color(p.muted)
                        .child(caption.into()),
                ),
        )
}

/// The new Bot's face, name and description as they are typed.
fn preview(face: impl IntoElement, title: String, detail: String, muted: Hsla) -> Div {
    let or = |text: String, empty: &str| {
        if text.is_empty() {
            empty.to_owned()
        } else {
            text
        }
    };
    div().flex().items_center().gap(space::S).child(face).child(
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(space::XS)
            .child(
                div()
                    .typography(Type::Headline)
                    .truncate()
                    .child(or(title, "Your Bot")),
            )
            .child(
                div()
                    .typography(Type::Caption)
                    .text_color(muted)
                    .line_clamp(2)
                    .child(or(detail, "A purpose of its own")),
            ),
    )
}

impl Workspace {
    pub(in crate::ui) fn create_bot_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        let name = field("", "Name", window, cx);
        let description = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Operational description")
                .auto_grow(3, 6)
        });
        cx.subscribe(&name, |_, _, _: &InputEvent, cx| cx.notify())
            .detach();
        cx.subscribe(&description, |_, _, _: &InputEvent, cx| cx.notify())
            .detach();
        let view = cx.entity().downgrade();
        let epoch = self.client.epoch;
        let tint = self.accent.color();
        let focus = Entity::clone(&name);
        window.open_dialog(cx, move |dialog, window, cx| {
            let p = *cx.global::<crate::ui::theme::Palette>();
            let ready = view
                .upgrade()
                .is_some_and(|view| view.read(cx).gateway_dialog_ready(epoch));
            let (title, detail) = (
                name.read(cx).value().trim().to_owned(),
                description.read(cx).value().trim().to_owned(),
            );
            let mood = if title.is_empty() {
                BotMood::Idle
            } else {
                BotMood::Watching
            };
            let face = lively_face(
                "new-bot-face",
                tint,
                BotShape::Circle,
                mood,
                face::FORM,
                window,
                cx,
            );
            let view = WeakEntity::clone(&view);
            let inputs = (Entity::clone(&name), Entity::clone(&description));
            dialog
                .w(width::DIALOG)
                .child(
                    dialog_body()
                        .child(preview(face, title, detail, p.muted))
                        .child(labelled(
                            "Name",
                            text_field(&name).aria_label("Bot name"),
                            p.muted,
                        ))
                        .child(labelled(
                            "Operational description",
                            textarea(&description).aria_label("Operational description"),
                            p.muted,
                        ))
                        .child(div().typography(Type::Caption).text_color(p.muted).child(
                            "möbius assigns the handle, color, shape and current agent defaults.",
                        )),
                )
                .map(|dialog| {
                    dialog_actions(dialog, "Create", ready, move |_, cx| {
                        view.update(cx, |this, cx| {
                            this.submit_gateway_dialog(epoch, cx, |this, cx| {
                                this.create_bot(&inputs.0, &inputs.1, cx)
                            })
                        })
                        .unwrap_or(false)
                    })
                })
        });
        window.defer(cx, move |window, cx| {
            focus.update(cx, |input, cx| input.focus(window, cx));
        });
    }

    /// Creates a Bot with the dialog's trimmed name and description.
    fn create_bot(
        &mut self,
        name: &Entity<InputState>,
        description: &Entity<TextareaState>,
        cx: &mut Context<Self>,
    ) -> bool {
        let name = name.read(cx).value().trim().to_owned();
        let description = description.read(cx).value().trim().to_owned();
        let known = self
            .client
            .state
            .gateway
            .iter()
            .flat_map(|gateway| gateway.bots.iter().map(|bot| Arc::clone(&bot.id)))
            .collect();
        let request_id = crate::gateway::request_id();
        let message = ClientMessage::CreateBot {
            request_id: Arc::clone(&request_id),
            harness: HarnessKind::Mobius,
            name,
            description,
        };
        self.track_bots_request(request_id, Pending::Create(known), message, cx)
    }
}
