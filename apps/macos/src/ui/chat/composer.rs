//! The SwiftUI composer: a 48 point pill at rest that opens into a card while in use
//! (ComposerView.swift, ComposerSurface.swift, ComposerOptionsView.swift).

use super::*;
use crate::numeric::{Saturating, ToFloat};
use gpui_component::input::Textarea;
use gpui_component::menu::ContextMenuExt;
use mobius::protocol::ActiveMessageDelivery;

/// DictationWaveform: the recording's recent loudness as 4 wide bars every 7 points,
/// newest on the right, older bars fading out to the left. The tape rolls left by
/// `rolled` of a step between samples, so a live recording visibly moves.
fn dictation_wave((levels, rolled): (Vec<f32>, f32), ink: Hsla) -> impl IntoElement {
    const STEP: f32 = 7.;
    const BAR: f32 = 4.;
    canvas(
        |_, _, _| {},
        move |bounds, (), window, _| {
            let (width, height) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
            let count = (width / STEP).saturating_usize().max(1);
            let leading = (width - (count - 1).to_f32() * STEP - BAR) / 2.;
            for index in 0..count {
                let level = (levels.len() + index)
                    .checked_sub(count)
                    .and_then(|sample| levels.get(sample).copied())
                    .unwrap_or(0.);
                let bar = 4. + level * (height - 4.);
                let origin = point(
                    bounds.origin.x + px(leading + (index.to_f32() - rolled) * STEP),
                    bounds.origin.y + px((height - bar) / 2.),
                );
                let alpha = ((index.to_f32() + 1. - rolled) / (count.to_f32() * 0.4)).clamp(0., 1.);
                window.paint_quad(pill_quad(
                    Bounds::new(origin, size(px(BAR), px(bar))),
                    ink.opacity(alpha),
                ));
            }
        },
    )
    .size_full()
}

pub(in crate::ui) fn surface_shadow() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: black().opacity(alpha::TINT),
        offset: point(px(0.), px(6.)),
        blur_radius: px(20.),
        spread_radius: px(0.),
        inset: false,
    }]
}

/// The card every composer state shares: fill, hairline and shadow. Over a window
/// background it frosts, and drops the shadow where nothing solid would hide it.
pub(super) fn composer_card(p: &Palette, cx: &App) -> Div {
    let card = div().relative();
    backdrop::surface(card, p.card, cx)
        .border_1()
        .border_color(p.separator.opacity(alpha::EDGE))
        .when(!p.is_veiled() || backdrop::frosted(cx), |card| {
            card.shadow(surface_shadow())
        })
}

impl Workspace {
    /// `over_document`: floating over a full-width tab, where the card keeps no page band.
    pub(super) fn composer_view(
        &self,
        over_document: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let surface = if let Some(reason) = self.composer_unavailable() {
            composer_card(&self.palette, cx)
                .px(space::L)
                .py(space::L)
                .typography(Type::Caption)
                .text_color(self.palette.muted)
                .child(format!("{reason}."))
                .into_any_element()
        } else if self.dictating {
            self.dictation_bar(cx)
        } else if self.voice_active {
            self.voice_composer(window, cx)
        } else {
            self.composer_surface(window, cx)
        };
        div()
            .flex_shrink_0()
            .px(space::L)
            .pb(space::S)
            .child(
                div()
                    .mx_auto()
                    .w_full()
                    .max_w(width::TRANSCRIPT)
                    .flex()
                    .flex_col()
                    .gap(space::S)
                    .children(self.slot_widgets(FrontendSlot::ComposerHeader, cx))
                    .children(self.mention_card(cx))
                    .children(self.picker_card(cx))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(space::XS)
                            .child(self.composer_pills(cx))
                            .map(|column| {
                                if over_document {
                                    // The card's fill is translucent, so the sheet goes beneath it.
                                    column.child(
                                        backdrop::floating(
                                            div().relative(),
                                            self.palette.sheet,
                                            cx,
                                        )
                                        .child(surface),
                                    )
                                } else {
                                    column.child(surface)
                                }
                            }),
                    ),
            )
            .into_any_element()
    }

    /// Always the expanded SwiftUI card: a resting pill made the composer jump.
    fn composer_surface(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        let accent = self.palette.accent;
        let view = cx.entity().downgrade();
        composer_card(&self.palette, cx)
            .when(
                self.composer.read(cx).focus_handle(cx).is_focused(window),
                |card| card.border_color(accent.opacity(alpha::EDGE)),
            )
            .drag_over::<ExternalPaths>(move |style, _, _, _| style.border_color(accent))
            .children(self.composer_reply(cx))
            .children(self.composer_context(cx))
            .child(
                div()
                    .px(space::L)
                    .pt(space::S)
                    .pb(space::XS)
                    .when(self.queued_draft.is_some(), |el| el.opacity(alpha::DIM))
                    .child(
                        Textarea::new(&self.composer)
                            .appearance(false)
                            .bordered(false)
                            .p_0()
                            .typography(Type::Body)
                            .aria_label("Message")
                            .on_paste(move |item, _, cx| {
                                view.update(cx, |this, cx| this.paste_into_composer(item, cx))
                                    .unwrap_or(false)
                            }),
                    )
                    // After the field, so it paints over the text the field just laid out.
                    .children(self.composer_links(cx)),
            )
            // The controls ride over this strip at the card's foot.
            .child(div().h(height::LARGE + inset::COMPOSER))
            .child(self.composer_controls(cx))
            .into_any_element()
    }

    fn composer_controls(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .absolute()
            .left(inset::COMPOSER)
            .right(inset::COMPOSER)
            .bottom(inset::COMPOSER)
            .h(height::LARGE)
            .flex()
            .items_center()
            .gap(space::XXS)
            .child(self.attach_button(cx))
            .child(div().flex_1())
            .child(self.model_selector(cx))
            .child(self.voice_button(cx))
            .child(self.composer_primary(cx))
            .into_any_element()
    }

    fn attach_button(&self, cx: &Context<Self>) -> AnyElement {
        let button = composer_button("attach", "Add attachment", "hi.plus", cx);
        if !self.can_attach_files() {
            let reason = "Connect to a gateway and select a Bot that accepts files.";
            return unavailable("attach-unavailable", button, reason, cx);
        }
        button
            .on_click(cx.listener(|this, _, window, cx| this.choose_attachments(window, cx)))
            .into_any_element()
    }

    /// Why dictation cannot start now.
    fn dictation_unavailable_reason(&self) -> Option<&'static str> {
        if !self.client.link.is_ready() {
            return Some("Connect to a gateway to dictate.");
        }
        (!(self.draft_selected() || self.active_session_ready()))
            .then_some("Wait for the chat to open before dictating.")
    }

    fn voice_button(&self, cx: &Context<Self>) -> AnyElement {
        let button = composer_button("voice", "Start voice chat", "hi.audioWave01", cx);
        if let Some(reason) = self.voice_unavailable_reason() {
            return unavailable("voice-unavailable", button, reason.to_owned(), cx);
        }
        button
            .on_click(cx.listener(|this, _, _, cx| this.toggle_voice(cx)))
            .into_any_element()
    }

    /// Whether the composer holds nothing to send: no text and no attachments.
    pub(in crate::ui) fn draft_is_empty(&self, cx: &App) -> bool {
        self.composer.read(cx).value().trim().is_empty() && self.draft_attachments().is_empty()
    }

    /// The turn the composer would stop: shown when a turn runs and there is nothing to send.
    fn interrupt_target(&self, cx: &App) -> Option<Arc<str>> {
        self.draft_is_empty(cx)
            .then(|| {
                self.client
                    .state
                    .session
                    .as_ref()?
                    .active_turn_ids
                    .first()
                    .map(Arc::clone)
            })
            .flatten()
    }

    /// Delivery for a message sent while a turn runs; `None` starts a new turn.
    fn active_delivery(&self) -> Option<ActiveMessageDelivery> {
        let session = self.client.state.session.as_ref()?;
        (!session.active_turn_ids.is_empty()).then(|| {
            if session.active_message_delivery == ActiveMessageDelivery::Steer {
                ActiveMessageDelivery::Steer
            } else {
                ActiveMessageDelivery::Queue
            }
        })
    }

    /// Stop while a turn runs with nothing typed, Dictate while empty, otherwise Send.
    fn composer_primary(&self, cx: &Context<Self>) -> AnyElement {
        let interrupt = self.interrupt_target(cx);
        let slot = div()
            .size(height::LARGE)
            .flex()
            .items_center()
            .justify_center();
        if let Some(turn_id) = interrupt {
            return slot
                .child(
                    primary_button("stop", "Stop", "hi.stopFill", cx).on_click(cx.listener(
                        move |this, _, _, cx| {
                            let turn_id = Arc::clone(&turn_id);
                            this.submit_op(Op::Interrupt { turn_id }, cx);
                        },
                    )),
                )
                .into_any_element();
        }
        let empty = self.draft_is_empty(cx);
        if empty && self.queued_draft.is_none() {
            let dictate = primary_button("dictate", "Dictate", "hi.mic01", cx);
            let dictate = match self.dictation_unavailable_reason() {
                Some(reason) => unavailable("dictate-unavailable", dictate, reason, cx),
                None => dictate
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_dictation(cx)))
                    .into_any_element(),
            };
            return slot.child(dictate).into_any_element();
        }
        let delivery = self.active_delivery();
        let (label, glyph) = delivery_label(delivery);
        let pending = self.queued_draft.is_some() || !self.client.link.is_ready();
        if pending {
            return slot
                .child(
                    Button::new("send")
                        .primary()
                        .size(height::PRIMARY)
                        .p_0()
                        .pill_corners()
                        .accessibility_label("Sending")
                        .disabled(true)
                        .child(Spinner::new(glyph::LEAD).color(self.palette.on_button_accent())),
                )
                .into_any_element();
        }
        let send = primary_button("send", label, glyph, cx)
            .disabled(empty || !self.attachments_ready())
            .on_click(cx.listener(|this, _, window, cx| this.send_message(window, cx)));
        let Some(current) = delivery else {
            return slot.child(send).into_any_element();
        };
        let alternate = match current {
            ActiveMessageDelivery::Steer => ActiveMessageDelivery::Queue,
            ActiveMessageDelivery::Queue => ActiveMessageDelivery::Steer,
        };
        let view = cx.entity().downgrade();
        slot.id("send-slot")
            .child(send)
            .context_menu(move |menu, _, cx| {
                let (label, glyph) = delivery_label(Some(alternate));
                let view = WeakEntity::clone(&view);
                menu.item(
                    gpui_component::menu::PopupMenuItem::new(label)
                        .icon(icon(glyph, &palette(cx)))
                        .on_click(move |_, window, cx| {
                            if let Err(error) = view.update(cx, |this, cx| {
                                this.send_message_as(Some(alternate), window, cx);
                            }) {
                                tracing::debug!(%error, "the workspace closed before the alternate send");
                            }
                        }),
                )
            })
            .into_any_element()
    }

    /// ComposerView.swift DictationComposerBar: cancel, listening, stop, send.
    fn dictation_bar(&self, cx: &Context<Self>) -> AnyElement {
        composer_card(&self.palette, cx)
            .h(height::COMPOSER_IDLE)
            .px(inset::COMPOSER)
            .flex()
            .items_center()
            .gap(space::S)
            .child(
                composer_button("cancel-dictation", "Cancel dictation", "hi.x", cx)
                    .on_click(cx.listener(|this, _, window, cx| this.cancel_dictation(window, cx))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h(height::LARGE)
                    .child(dictation_wave(self.dictation_tape(), self.palette.muted)),
            )
            .child(
                composer_button("stop-dictation", "Stop dictation", "hi.stopFill", cx)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_dictation(cx))),
            )
            .child(
                div()
                    .size(height::LARGE)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(primary_button("send", "Send", "hi.arrowUp02", cx).on_click(
                        cx.listener(|this, _, window, cx| {
                            this.stop_dictation();
                            this.send_message(window, cx);
                        }),
                    )),
            )
            .into_any_element()
    }

    /// ComposerView.swift RealtimeVoiceComposer: the equalizer over mute, status and end.
    fn voice_composer(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let (mute_label, mute_glyph) = if self.voice_muted {
            ("Unmute", "hi.micOff01")
        } else {
            ("Mute", "hi.mic01")
        };
        composer_card(&self.palette, cx)
            .flex()
            .flex_col()
            .gap(space::S)
            .child(
                div()
                    .px(space::L)
                    .pt(space::S)
                    .child(self.voice_wave(window, cx)),
            )
            .child(
                div()
                    .px(inset::COMPOSER)
                    .pb(inset::COMPOSER)
                    .flex()
                    .items_center()
                    .gap(space::S)
                    .child({
                        let mute = composer_button("voice-mute", mute_label, mute_glyph, cx)
                            .toggle(self.voice_muted, cx);
                        if self.voice_connected {
                            mute.on_click(cx.listener(|this, _, _, _| {
                                this.native.mute_voice(!this.voice_muted);
                            }))
                            .into_any_element()
                        } else {
                            unavailable(
                                "voice-mute-unavailable",
                                mute,
                                "The call is still connecting.",
                                cx,
                            )
                        }
                    })
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .justify_center()
                            .text_color(self.palette.muted)
                            .typography(Type::Label)
                            .when(!self.voice_connected, |status| {
                                status.child(shimmer_text("Connecting voice…"))
                            }),
                    )
                    .child(
                        div()
                            .size(height::LARGE)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                primary_button("end-voice", "End voice chat", "hi.stopFill", cx)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.stop_voice();
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

fn delivery_label(delivery: Option<ActiveMessageDelivery>) -> (&'static str, &'static str) {
    match delivery {
        None => ("Send", "hi.arrowUp02"),
        Some(ActiveMessageDelivery::Steer) => ("Send as Steer", "hi.arrowUpRight01"),
        Some(ActiveMessageDelivery::Queue) => ("Send as Queue", "hi.queue01"),
    }
}

/// Muted text with the running shimmer; static in inactive windows or Reduce Motion.
pub(super) fn shimmer_text(text: impl Into<SharedString>) -> AnyElement {
    crate::ui::primitives::Shimmer::new(text).into_any_element()
}
