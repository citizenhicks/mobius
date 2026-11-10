use super::*;
use gpui_component::menu::ContextMenuExt;

impl Workspace {
    pub(in crate::ui) fn pinned_bot(&self) -> Option<&BotRecord> {
        self.client
            .state
            .bot(self.shell.pinned_bots.get(self.client.endpoint.as_str())?)
    }

    pub(super) fn set_bot_pinned(&mut self, id: &str, pinned: bool, cx: &mut Context<Self>) {
        let endpoint = self.client.endpoint.as_str().to_owned();
        if pinned {
            self.shell.pinned_bots.insert(endpoint, id.to_owned());
        } else {
            self.shell.pinned_bots.remove(&endpoint);
        }
        let pins = self.shell.pinned_bots.clone(); // rules-allow: the preferences file being saved needs its own copy of the pinned map the shell keeps
        self.save_preferences(
            "pinned Bot",
            |preferences| preferences.pinned_bots = pins,
            cx,
        );
        cx.notify();
    }

    pub(in crate::ui) fn pinned_bot_button(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let bot = self.pinned_bot()?;
        let session = bot.conversation_session_id.share();
        let id = Arc::clone(&bot.id);
        let button = Button::new("pinned-bot")
            .ghost()
            .size(height::LARGE)
            .p_0()
            .toggle(self.selected_chat() == Some(session.as_ref()), cx)
            .tooltip(Arc::clone(&bot.name))
            .accessibility_label(format!("Talk to {}", bot.name))
            .child(BotFace::new(
                Accent::from(bot.tint).color(),
                bot.shape,
                glyph::LEAD,
            ))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_page(PageTab::Chat(session.share()), window, cx);
            }));
        Some(Self::bot_pin_context_menu(button, id, cx))
    }

    pub(in crate::ui) fn bot_pin_context_menu(
        button: Button,
        id: Arc<str>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let view = cx.entity().downgrade();
        button
            .context_menu(move |menu, _, cx| {
                let pinned = view
                    .upgrade()
                    .is_some_and(|view| view.read(cx).pinned_bot().is_some_and(|bot| bot.id == id));
                menu::fill_menu(
                    menu,
                    &view,
                    &[MenuAction::new(
                        if pinned { "Unpin Bot" } else { "Pin Bot" },
                        if pinned {
                            "hi.pushPinSlash"
                        } else {
                            "hi.pushPin"
                        },
                        {
                            let id = Arc::clone(&id);
                            move |this, _, cx| this.set_bot_pinned(&id, !pinned, cx)
                        },
                    )],
                    cx,
                )
            })
            .into_any_element()
    }
}
