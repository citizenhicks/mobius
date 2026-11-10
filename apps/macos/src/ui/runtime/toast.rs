//! The one toast at the top centre: a tone-coloured glyph and the message. It hides
//! itself, a swipe up dismisses it, and one with an action runs it on click. Queued
//! notices wait their turn, so an error is never replaced unseen.

use super::*;
use std::time::Duration;

/// Below the titlebar, clear of the traffic lights.
const TOP: Pixels = px(56.);
const RISE: Pixels = px(12.);

pub(in crate::ui) struct Toast {
    id: u64,
    tone: Tone,
    message: SharedString,
    action: Option<navigation::NoticeAction>,
    reveal: motion::Reveal,
}

impl Workspace {
    /// Shows the next queued notice once the one on screen has gone; progress and success
    /// make way for it, errors and warnings keep their time.
    pub(in crate::ui) fn next_toast(&mut self, cx: &mut Context<Self>) {
        if self.shell.notices.is_empty() {
            return;
        }
        if let Some(toast) = &self.shell.toast {
            if matches!(toast.tone, Tone::Info | Tone::Success) {
                self.dismiss_toast(toast.id, cx);
            }
            return;
        }
        let (tone, message, action) = self.shell.notices.remove(0);
        let id = self.shell.next_toast_id;
        self.shell.next_toast_id += 1;
        let mut reveal = motion::Reveal::new(false);
        reveal.set(true, cx.reduce_motion(), motion::now());
        self.shell.toast = Some(Toast {
            id,
            tone,
            message: message.into(),
            action,
            reveal,
        });
        let hold = if matches!(tone, Tone::Error | Tone::Warning) {
            Duration::from_secs(7)
        } else {
            Duration::from_secs(4)
        };
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(hold).await;
            if let Err(error) = this.update(cx, |this, cx| this.dismiss_toast(id, cx)) {
                tracing::debug!(%error, "the window closed before its toast hid");
            }
        })
        .detach();
        cx.notify();
    }

    fn dismiss_toast(&mut self, id: u64, cx: &mut Context<Self>) {
        let still = cx.reduce_motion();
        if let Some(toast) = self.shell.toast.as_mut().filter(|toast| toast.id == id) {
            toast.reveal.set(false, still, motion::now());
            cx.notify();
        }
    }

    fn run_toast(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let action = self
            .shell
            .toast
            .as_mut()
            .filter(|toast| toast.id == id)
            .and_then(|toast| toast.action.take());
        self.dismiss_toast(id, cx);
        match action {
            Some(navigation::NoticeAction::Reveal(file)) => cx.reveal_path(&file),
            Some(navigation::NoticeAction::Storage) => self.open_storage_cleanup(window, cx),
            Some(navigation::NoticeAction::ComputerSetup) => {
                self.open_page(PageTab::Settings(SettingsPage::Security), window, cx);
            }
            Some(navigation::NoticeAction::Issue(action)) => {
                self.perform_issue_action(action, window, cx);
            }
            None => {}
        }
    }

    pub(in crate::ui) fn toast_layer(
        &mut self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let now = motion::now();
        let toast = self.shell.toast.as_ref()?;
        let shown = toast.reveal.shown(now);
        if toast.reveal.moving(now) {
            window.request_animation_frame();
        } else if shown == 0. {
            self.shell.toast = None;
            crate::host::platform::wake();
            return None;
        }
        let p = self.palette;
        let id = toast.id;
        Some(
            div()
                .absolute()
                .top(TOP - RISE * (1. - shown))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id(ElementId::NamedInteger("toast".into(), id))
                        .occlude()
                        .w(width::TOAST)
                        .flex()
                        .items_center()
                        .gap(space::S)
                        .px(space::L)
                        .py(space::L)
                        .tile_corners()
                        .bg(p.control)
                        .border_1()
                        .border_color(p.separator)
                        .shadow(chat::surface_shadow())
                        .opacity(shown)
                        .child(
                            icon(toast.tone.glyph(), &p)
                                .size(glyph::LEAD)
                                .text_color(toast.tone.color(&p)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(SharedString::clone(&toast.message)),
                        )
                        .on_scroll_wheel(cx.listener(
                            move |this, event: &ScrollWheelEvent, window, cx| {
                                if event.delta.pixel_delta(window.line_height()).y < -RISE / 3. {
                                    this.dismiss_toast(id, cx);
                                }
                            },
                        ))
                        .when(toast.action.is_some(), |card| {
                            card.cursor_pointer().on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.run_toast(id, window, cx);
                                },
                            ))
                        }),
                )
                .into_any_element(),
        )
    }
}
