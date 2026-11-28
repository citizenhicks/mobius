//! The SwiftUI composer: a 48 point pill at rest that opens into a card while in use
//! (ComposerView.swift, ComposerSurface.swift, ComposerOptionsView.swift).

use super::*;
use crate::numeric::{Saturating, ToFloat};
use gpui_component::input::Textarea;
use gpui_component::menu::ContextMenuExt;
use mobius::protocol::ActiveMessageDelivery;

/// DictationWaveform: the recording as a tape of dot columns, one per 100 ms entering at
/// the trailing edge and rolling toward the leading one by `rolled` of a pitch between
/// samples. Silence is a single dot, speech a column as tall as it was loud.
fn dictation_wave((levels, rolled): (Vec<f32>, f32), quiet: Hsla, loud: Hsla) -> impl IntoElement {
    const PITCH: f32 = 5.;
    const DOT: f32 = 3.;
    canvas(
        |_, _, _| {},
        move |bounds, (), window, _| {
            let (width, height) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
            let middle = height / 2.;
            let rows = ((middle - DOT / 2.) / PITCH).floor().max(1.);
            for index in 0..(width / PITCH).saturating_usize() + 2 {
                // The newest column slides in from one pitch beyond the trailing edge.
                let x = width + PITCH / 2. - (index.to_f32() + rolled) * PITCH;
                let level = levels
                    .len()
                    .checked_sub(index + 1)
                    .and_then(|sample| levels.get(sample).copied())
                    .unwrap_or(0.);
                let lit = (level * rows).round().saturating_i32();
                let edge = (x / 24.).min((width - x) / PITCH).clamp(0., 1.);
                let ink = if lit == 0 { quiet } else { loud }.opacity(edge);
                for row in -lit..=lit {
                    let origin = point(
                        bounds.origin.x + px(x - DOT / 2.),
                        bounds.origin.y + px(middle + row.to_f32() * PITCH - DOT / 2.),
                    );
                    window.paint_quad(pill_quad(Bounds::new(origin, size(px(DOT), px(DOT))), ink));
                }
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
        let row = div()
            .absolute()
            .left(inset::COMPOSER)
            .right(inset::COMPOSER)
            .bottom(inset::COMPOSER)
            .h(height::LARGE)
            .flex()
            .items_center()
            .gap(space::XXS);
        if self.dictating {
            return self.dictation_controls(row, cx);
        }
        row.child(self.attach_button(cx))
            .child(div().flex_1())
            .child(motion::fade_in(
                self.model_selector(cx),
                "composer-model",
                motion::STANDARD,
                cx,
            ))
            .child(self.dictate_button(cx))
            .child(self.composer_primary(cx))
            .into_any_element()
    }

    fn attach_button(&self, cx: &Context<Self>) -> AnyElement {
        let button =
            crate::ui::primitives::quiet_button("attach", "Add attachment", height::LARGE, cx)
                .pill_corners();
        let control = if self.can_attach_files() {
            button
                .on_click(cx.listener(|this, _, window, cx| this.choose_attachments(window, cx)))
                .into_any_element()
        } else {
            let reason = "Connect to a gateway and select a Bot that accepts files.";
            unavailable("attach-unavailable", button, reason, cx)
        };
        self.morph_control("composer-lead-mark", control, "hi.plus")
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

    fn dictate_button(&self, cx: &Context<Self>) -> AnyElement {
        let button = crate::ui::primitives::quiet_button("dictate", "Dictate", height::LARGE, cx)
            .pill_corners();
        #[cfg(feature = "snapshot")]
        let button = button.child(crate::ui::snapshot::probe("composer-dictate".into()));
        let control = match self.dictation_unavailable_reason() {
            Some(reason) => unavailable("dictate-unavailable", button, reason, cx),
            None => button
                .on_click(cx.listener(|this, _, _, cx| this.toggle_dictation(cx)))
                .into_any_element(),
        };
        self.morph_control("composer-mic-mark", control, "hi.mic01")
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

    /// Stop while a turn runs with nothing typed, Voice while empty, otherwise Send.
    fn composer_primary(&self, cx: &Context<Self>) -> AnyElement {
        use crate::ui::primitives::{morph_icon, primary_control};
        let interrupt = self.interrupt_target(cx);
        let slot = div()
            .id("composer-primary-slot")
            .relative()
            .size(height::LARGE)
            .flex()
            .items_center()
            .justify_center();
        // Keep the mark in one slot while the action switches between a menu, a popover and a button.
        let mark = |name, dim: bool| {
            let ink = self.palette.on_button_accent();
            div().absolute().child(morph_icon(
                "primary-mark",
                name,
                glyph::LEAD,
                if dim { ink.opacity(alpha::DIM) } else { ink },
            ))
        };
        if let Some(turn_id) = interrupt {
            return slot
                .child(primary_control("stop", "Stop").on_click(cx.listener(
                    move |this, _, _, cx| {
                        let turn_id = Arc::clone(&turn_id);
                        this.submit_op(Op::Interrupt { turn_id }, cx);
                    },
                )))
                .child(mark("hi.stopFill", false))
                .into_any_element();
        }
        let empty = self.draft_is_empty(cx);
        if empty && self.queued_draft.is_none() {
            let voice = primary_control("voice", "Start voice chat");
            let reason = self.voice_unavailable_reason();
            let voice = match reason {
                Some(reason) => unavailable("voice-unavailable", voice, reason.to_owned(), cx),
                None => voice
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_voice(cx)))
                    .into_any_element(),
            };
            return slot
                .child(voice)
                .child(mark("hi.audioWave01", reason.is_some()))
                .into_any_element();
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
        let send = primary_control("send", label)
            .disabled(empty || !self.attachments_ready())
            .on_click(cx.listener(|this, _, window, cx| this.send_message(window, cx)));
        let Some(current) = delivery else {
            return slot
                .child(send)
                .child(mark(glyph, empty || !self.attachments_ready()))
                .into_any_element();
        };
        let alternate = match current {
            ActiveMessageDelivery::Steer => ActiveMessageDelivery::Queue,
            ActiveMessageDelivery::Queue => ActiveMessageDelivery::Steer,
        };
        let view = cx.entity().downgrade();
        slot.child(send).child(mark(glyph, empty || !self.attachments_ready()))
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

    /// A composer control whose glyph morphs in place as its action changes.
    fn morph_control(&self, slot: &'static str, control: AnyElement, glyph: &'static str) -> Div {
        div()
            .relative()
            .size(height::LARGE)
            .flex()
            .items_center()
            .justify_center()
            .child(control)
            .child(div().absolute().child(crate::ui::primitives::morph_icon(
                slot,
                glyph,
                glyph::LEAD,
                self.palette.muted,
            )))
    }

    /// ComposerView.swift DictationComposerBar on the card's foot: cancel, the tape, stop
    /// and send stand where attach, the model, dictate and send do.
    fn dictation_controls(&self, row: Div, cx: &Context<Self>) -> AnyElement {
        use crate::ui::primitives::quiet_button;
        let finishing = self.dictation_draft.finishing;
        let cancel = quiet_button("cancel-dictation", "Cancel dictation", height::LARGE, cx)
            .pill_corners()
            .on_click(cx.listener(|this, _, window, cx| this.cancel_dictation(window, cx)));
        let stop = quiet_button("stop-dictation", "Stop dictation", height::LARGE, cx)
            .pill_corners()
            .on_click(cx.listener(|this, _, _, cx| this.toggle_dictation(cx)));
        #[cfg(feature = "snapshot")]
        let stop = stop.child(crate::ui::snapshot::probe("composer-dictation-stop".into()));
        let wave = dictation_wave(
            self.dictation_tape(),
            self.palette.muted,
            self.palette.accent,
        );
        row.child(self.morph_control("composer-lead-mark", cancel.into_any_element(), "hi.x"))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .px(space::S)
                    .child(motion::fade_in(
                        wave,
                        "dictation-wave",
                        motion::STANDARD,
                        cx,
                    )),
            )
            .child(if finishing {
                div()
                    .size(height::LARGE)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Spinner::new(glyph::LEAD).color(self.palette.muted))
            } else {
                self.morph_control("composer-mic-mark", stop.into_any_element(), "hi.stopFill")
            })
            .child(
                div()
                    .size(height::LARGE)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(primary_button("send", "Send", "hi.arrowUp02", cx).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.dictation_draft.send_on_final = true;
                            this.dictation_draft.finishing = true;
                            this.native.finish_dictation();
                            cx.notify();
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
