//! The Bot's persistent conversation uses the same chat, transcript and composer.

use super::*;
use gpui_component::scroll::ScrollableMask;
use gpui_component::skeleton::Skeleton;
use mobius_gateway::wire::BotRecord;

impl Workspace {
    /// Identifies a canonical conversation through the gateway's Bot catalog.
    pub(in crate::ui) fn conversation_bot(&self, session_id: &str) -> Option<&BotRecord> {
        self.client
            .state
            .gateway
            .as_ref()?
            .bots
            .iter()
            .find(|bot| *bot.conversation_session_id == *session_id)
    }

    pub(in crate::ui) fn active_conversation_bot(&self) -> Option<&BotRecord> {
        let PageTab::Chat(id) = self.primary_page()? else {
            return None;
        };
        self.conversation_bot(id)
    }

    /// Status is shared by the sidebar avatar and the conversation's header.
    pub(in crate::ui) fn conversation_avatar(
        &self,
        bot: &BotRecord,
        face: impl IntoElement,
        background: Hsla,
    ) -> Div {
        let mood = self.bot_mood(&bot.id);
        let color = match mood {
            BotMood::Sleepy => self.palette.muted,
            BotMood::Watching => self.palette.warning,
            BotMood::Idle
            | BotMood::Thinking
            | BotMood::Listening
            | BotMood::Dizzy
            | BotMood::Happy
            | BotMood::Wink => {
                if self.is_unread(&bot.conversation_session_id) {
                    self.palette.accent
                } else {
                    self.palette.signal
                }
            }
        };
        let status = if mood == BotMood::Thinking {
            Spinner::new(glyph::RING)
                .color(self.palette.accent)
                .into_any_element()
        } else {
            div()
                .size(glyph::DOT)
                .pill_corners()
                .bg(color)
                .into_any_element()
        };
        div().relative().flex_shrink_0().child(face).child(
            div()
                .absolute()
                .top_0()
                .right_0()
                .p(space::XXS)
                .pill_corners()
                .bg(background)
                .child(status),
        )
    }

    fn conversation_status(&self, bot: &BotRecord) -> &'static str {
        match self.bot_mood(&bot.id) {
            BotMood::Sleepy => "offline",
            BotMood::Watching => "approval required",
            BotMood::Thinking => "running",
            BotMood::Idle
            | BotMood::Listening
            | BotMood::Dizzy
            | BotMood::Happy
            | BotMood::Wink => {
                if self.is_unread(&bot.conversation_session_id) {
                    "unread messages"
                } else {
                    "ready"
                }
            }
        }
    }

    pub(in crate::ui) fn bot_conversation_rail(&self, cx: &Context<Self>) -> AnyElement {
        let mut bots: Vec<_> = self
            .client
            .state
            .gateway
            .iter()
            .flat_map(|gateway| &gateway.bots)
            .collect();
        bots.sort_by_key(|bot| bot.name.to_lowercase());
        if bots.is_empty() && !self.client.link.is_ready() {
            return loading_rail();
        }
        let items = bots.into_iter().map(|bot| {
            let session_id = bot.conversation_session_id.share();
            let avatar = self.conversation_avatar(
                bot,
                BotFace::new(Accent::from(bot.tint).color(), bot.shape, face::HERO),
                self.palette.sheet,
            );
            #[cfg(feature = "snapshot")]
            let avatar = avatar.child(snapshot::probe(format!("bot-conversation-face:{}", bot.id)));
            let button = Button::new(SharedString::from(format!("bot-conversation:{}", bot.id)))
                .ghost()
                .h_auto()
                .p(space::S)
                .tile_corners()
                .toggle(self.selected_chat() == Some(session_id.as_ref()), cx)
                .tooltip(format!("Talk to {}", bot.name))
                .accessibility_label(format!(
                    "Talk to {}, {}",
                    bot.name,
                    self.conversation_status(bot)
                ))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(space::XXS)
                        .child(avatar)
                        .child(self.conversation_name(&bot.name)),
                );
            if !self.client.link.is_ready() {
                let reason = format!("Connect to a gateway to talk to {}.", bot.name);
                return unavailable(
                    format!("bot-conversation-unavailable:{}", bot.id),
                    button,
                    reason,
                    cx,
                );
            }
            let button = button.on_click(cx.listener(move |this, _, window, cx| {
                this.open_page(PageTab::Chat(session_id.share()), window, cx);
            }));
            Self::bot_pin_context_menu(button, Arc::clone(&bot.id), cx)
        });
        let scroll = &self.shell.bot_rail_scroll;
        div()
            .relative()
            .flex_shrink_0()
            .w_full()
            .min_w_0()
            .child(
                div()
                    .id("bot-conversation-rail")
                    .flex()
                    .w_full()
                    .gap(space::XS)
                    .py(space::S)
                    .overflow_x_scroll()
                    .track_scroll(scroll)
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .mx_auto()
                            .gap(space::XS)
                            .children(items),
                    ),
            )
            .child(ScrollableMask::vertical_wheel(scroll))
            .into_any_element()
    }

    /// Uses the existing pill surface for the Bot name in both navigation and chrome.
    pub(in crate::ui) fn conversation_name(&self, name: &str) -> Div {
        div()
            .typography(Type::Label)
            .bg(self.palette.card)
            .pill_corners()
            .px(space::S)
            .py(space::XS)
            .truncate()
            .child(name.to_owned())
    }
}

/// Holds the faces rail's height while Bots load, so the list does not drop when they land.
fn loading_rail() -> AnyElement {
    div()
        .id("bot-conversation-rail")
        .flex()
        .flex_shrink_0()
        .w_full()
        .justify_center()
        .gap(space::XS)
        .py(space::S)
        .children((0..3).map(|_| {
            div()
                .p(space::S)
                .flex()
                .flex_col()
                .items_center()
                .gap(space::XXS)
                .child(Skeleton::new().size(face::HERO).pill_corners())
                .child(
                    Skeleton::new()
                        .w(face::HERO)
                        .h(height::COMPACT - space::XS)
                        .pill_corners(),
                )
        }))
        .into_any_element()
}
