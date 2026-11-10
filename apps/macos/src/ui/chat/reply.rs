//! Replying to a message and reading one aloud (AppModelTranscript.swift beginReplying,
//! MessageSpeaker.swift), and the quote a reply shows (ReplyQuoteView).

use super::*;
use mobius::SessionId;
use mobius::protocol::MessageReply;

/// The accent bar beside quoted text.
const QUOTE_BAR: Pixels = px(3.);

/// The quoted message: an accent bar beside up to four caption lines on a panel tile.
pub(in crate::ui) fn reply_quote(p: &Palette, text: &str) -> Div {
    div()
        .flex()
        .items_start()
        .gap(space::S)
        .p(space::S)
        .tile_corners()
        .bg(p.panel.opacity(alpha::VEIL))
        .child(
            div()
                .w(QUOTE_BAR)
                .h_full()
                .min_h(Type::Caption.line_height())
                .pill_corners()
                .bg(p.accent),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .typography(Type::Caption)
                .whitespace_normal()
                .line_clamp(4)
                .child(text.to_owned()),
        )
}

/// Markdown as the words a voice reads: text and code, with breaks as pauses.
pub(super) fn plain_text(markdown: &str) -> String {
    use pulldown_cmark::{Event, Parser, TagEnd};
    let mut text = String::new();
    for event in Parser::new(markdown) {
        match event {
            Event::Text(part) | Event::Code(part) => text.push_str(&part),
            Event::SoftBreak => text.push(' '),
            Event::HardBreak
            | Event::End(TagEnd::Paragraph | TagEnd::Item | TagEnd::Heading(_)) => text.push('\n'),
            _ => {}
        }
    }
    text.trim().to_owned()
}

/// A message as one line of plain words, for the composer's reply bar.
fn one_line(markdown: &str) -> String {
    plain_text(markdown)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

impl Workspace {
    /// Quotes `text` by `author` above the composer; the next message sent in this chat
    /// replies to it.
    pub(in crate::ui) fn begin_reply(
        &mut self,
        target: mobius::protocol::MessageTarget,
        text: String,
        author: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self
            .client
            .state
            .selected_session_id
            .as_ref()
            .map(SessionId::share)
        else {
            return;
        };
        self.chat_ui.reply = Some((
            self.client.draft_key(&session),
            MessageReply { target, text },
            author,
        ));
        self.focus_composer(window, cx);
        cx.notify();
    }

    /// The quote belongs to the draft until its message has been enqueued.
    pub(in crate::ui) fn draft_reply(
        &self,
        session: &str,
    ) -> Option<(String, MessageReply, String)> {
        let key = self.client.draft_key(session);
        self.chat_ui
            .reply
            .as_ref()
            .filter(|(owner, ..)| owner == &key)
            .cloned() // rules-allow: the queued message carries its own quote while the draft keeps it until the enqueue is accepted
    }

    /// A refused send restores its quote without replacing a newer reply selection.
    pub(in crate::ui) fn restore_reply(&mut self, reply: Option<(String, MessageReply, String)>) {
        if self.chat_ui.reply.is_none() {
            self.chat_ui.reply = reply;
        }
    }

    /// The reply the accepted enqueue carried, clearing it.
    pub(in crate::ui) fn take_reply(&mut self, session: &str) -> Option<MessageReply> {
        let key = self.client.draft_key(session);
        self.chat_ui
            .reply
            .take_if(|(owner, ..)| owner == &key)
            .map(|(_, reply, _)| reply)
    }

    /// Reads a message aloud with the system voice, replacing any message being read.
    pub(in crate::ui) fn speak_message(&mut self, markdown: &str) {
        if let Some(mut speech) = self.chat_ui.speech.take() {
            if let Err(error) = speech.kill() {
                tracing::debug!(%error, "the previous message had already finished reading aloud");
            }
            if let Err(error) = speech.wait() {
                tracing::warn!(%error, "couldn't reap the previous read-aloud process");
            }
        }
        let text = plain_text(markdown);
        if text.is_empty() {
            return;
        }
        match crate::host::platform::speak(&text) {
            Ok(child) => self.chat_ui.speech = Some(child),
            Err(error) => self.report(format!("Couldn’t read the message aloud: {error}")),
        }
    }

    /// One compact bar above the composer text: the author and the quoted message on a
    /// single line, and Cancel reply.
    pub(super) fn composer_reply(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let (key, reply, author) = self.chat_ui.reply.as_ref()?;
        if self
            .client
            .state
            .selected_session_id
            .as_ref()
            .map(|id| self.client.draft_key(id))
            .as_ref()
            != Some(key)
        {
            return None;
        }
        let p = self.palette;
        let quote = one_line(&reply.text);
        Some(
            div()
                .px(space::L)
                .pt(space::S)
                .child(
                    div()
                        .id("composer-reply")
                        .role(gpui::accesskit::Role::Group)
                        .aria_label(format!("Replying to {author}: {quote}"))
                        .h(height::COMPACT)
                        .pl(space::S)
                        .pr(space::XS)
                        .flex()
                        .items_center()
                        .gap(space::S)
                        .tile_corners()
                        .bg(p.panel.opacity(alpha::VEIL))
                        .child(
                            div()
                                .w(QUOTE_BAR)
                                .h(glyph::INLINE)
                                .pill_corners()
                                .bg(p.accent),
                        )
                        .child(icon("hi.re", &palette(cx)).size(glyph::MARK))
                        .child(
                            div()
                                .flex_shrink_0()
                                .typography(Type::Caption)
                                .text_color(p.muted)
                                .child(format!("Replying to {author}")),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .typography(Type::Caption)
                                .text_color(p.text)
                                .child(quote),
                        )
                        .child(
                            mini_button("reply-cancel", "Cancel reply", "hi.x", cx).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.chat_ui.reply = None;
                                    cx.notify();
                                }),
                            ),
                        ),
                )
                .into_any_element(),
        )
    }
}
