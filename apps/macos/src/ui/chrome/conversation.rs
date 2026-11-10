//! The persistent Bot conversation keeps its identity in a compact main header.

use super::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Styled, Window, Workspace,
    div, face, height, space, transparent_black, width,
};

impl Workspace {
    pub(in crate::ui) fn bot_conversation_header(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        // The chat's controls stay in the titlebar (page_header), as for every chat.
        let bot = self.active_conversation_bot()?;
        let avatar = self.conversation_avatar(
            bot,
            self.chat_face(face::HERO, window, cx),
            transparent_black(),
        );
        Some(
            div()
                .id("bot-conversation-header")
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(height::BOT_CHAT_HEADER)
                .flex()
                .flex_col()
                .flex_shrink_0()
                .items_center()
                .gap(space::XXS)
                .pt(space::XS)
                .pb(space::S)
                .child(avatar)
                .child(self.conversation_name(&bot.name).max_w(width::NOTE))
                .into_any_element(),
        )
    }
}
