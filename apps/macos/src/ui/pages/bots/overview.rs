//! A Bot's overview: face, description, routines, run history, chats and configuration.

use super::menu::more_menu;
use crate::ui::Workspace;
use crate::ui::navigation::Section as Rail;
use crate::ui::primitives::{
    BotMood, EmptyState, Group, Leading, PageHeader, Row, Section, ago, button, face, lively_face,
    page, single_line, toolbar_button,
};
use crate::ui::router::{AgentSection, PageTab};
use crate::ui::theme::Accent;
use gpui::{AnyElement, Context, IntoElement as _, ParentElement as _, SharedString, Window};
use gpui_component::Disableable as _;
use gpui_component::button::ButtonVariants as _;
use mobius_gateway::wire::{BotRecord, HarnessConfig, SessionActivityState};
use std::sync::Arc;

const CHATS_PAGE: usize = 5;

impl Workspace {
    /// The Bots landing: the first Bot by name, or the empty state.
    pub(in crate::ui) fn render_bots(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let first = self.first_bot_id();
        if let Some(id) = first {
            return self.render_bot(&id, window, cx);
        }
        let create = button("create-first-bot", "New Bot")
            .primary()
            .disabled(!self.client.link.is_ready())
            .on_click(cx.listener(|this, _, window, cx| this.create_bot_dialog(window, cx)));
        page("bots", PageHeader::new("Bots"))
            .child(
                EmptyState::new("bot.face", "No Bots yet")
                    .detail("Durable agents and their routines.")
                    .action(create),
            )
            .into_any_element()
    }

    pub(in crate::ui) fn render_bot(
        &self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(bot) = self.client.state.bot(id) else {
            return page("bot", PageHeader::new("Bot unavailable"))
                .child(
                    EmptyState::new("bot.face", "Bot unavailable")
                        .detail("This Bot is no longer available."),
                )
                .into_any_element();
        };
        page(
            SharedString::from(format!("bot:{id}")),
            self.bot_header(bot, window, cx),
        )
        .child(
            Section::new("Description").child(
                Group::new().child(Row::new("bot-description", Arc::clone(&bot.description))),
            ),
        )
        .child(self.routines_section(bot, cx))
        .child(self.runs_section(id, cx))
        .child(self.chats_section(bot, cx))
        .child(self.configuration_section(bot, cx))
        .into_any_element()
    }

    /// Thinking while one of its chats runs, watching while one waits for approval.
    pub(in crate::ui) fn bot_mood(&self, id: &str) -> BotMood {
        let states = || {
            self.client
                .state
                .sessions
                .iter()
                .filter(move |session| *session.session_context.owner_id == *id)
                .map(|session| session.activity.state)
        };
        if !self.client.link.is_ready() {
            BotMood::Sleepy
        } else if states().any(|state| state == SessionActivityState::AwaitingApproval) {
            BotMood::Watching
        } else if states().any(|state| state == SessionActivityState::Running) {
            BotMood::Thinking
        } else {
            BotMood::Idle
        }
    }

    fn bot_header(
        &self,
        bot: &BotRecord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PageHeader {
        let tint = Accent::from(bot.tint).color();
        let face = lively_face(
            format!("bot-overview:{}", bot.id),
            tint,
            bot.shape,
            self.bot_mood(&bot.id),
            face::FORM,
            window,
            cx,
        );
        let subtitle = match crate::ui::pages::harness::unavailable(bot.harness) {
            Some(reason) => format!("@{} · {reason}", bot.handle),
            None => format!("@{}", bot.handle),
        };
        PageHeader::new(Arc::clone(&bot.name))
            .leading(face)
            .subtitle(subtitle)
    }

    /// The Bot the Bots landing shows: the first by name.
    pub(in crate::ui) fn first_bot_id(&self) -> Option<Arc<str>> {
        self.client.state.gateway.as_ref().and_then(|gateway| {
            gateway
                .bots
                .iter()
                .min_by_key(|bot| bot.name.to_lowercase())
                .map(|bot| Arc::clone(&bot.id))
        })
    }

    /// More (Rename…, Delete Bot…), Edit, New routine and New chat.
    pub(in crate::ui) fn bot_page_actions(&self, id: &str, cx: &Context<Self>) -> Vec<AnyElement> {
        let Some(bot) = self.client.state.bot(id) else {
            return Vec::new();
        };
        // This release offers run actions only for the möbius harness.
        let ready =
            self.client.link.is_ready() && crate::ui::pages::harness::available(bot.harness);
        let (chat, edit, routine) = (
            Arc::clone(&bot.id),
            Arc::clone(&bot.id),
            Arc::clone(&bot.id),
        );
        let more = more_menu(
            toolbar_button("bot-more", "More", "hi.dotsThree", cx),
            self.bot_manage_actions(bot),
            cx,
        );
        vec![
            more,
            toolbar_button("bot-edit", "Edit Bot", "hi.slidersHorizontal", cx)
                .on_click(cx.listener(move |this, _, window, cx| {
                    let page = PageTab::BotSettings(Arc::clone(&edit), AgentSection::Identity);
                    this.open_page(page, window, cx);
                }))
                .into_any_element(),
            toolbar_button("new-routine", "New routine", "hi.plus", cx)
                .disabled(!ready || self.routine_workspaces().is_empty())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.routine_dialog(Arc::clone(&routine), None, window, cx);
                }))
                .into_any_element(),
            toolbar_button("bot-new-chat", "New chat", "hi.notePencil", cx)
                .disabled(!ready)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.new_chat_for(Arc::clone(&chat), window, cx);
                }))
                .into_any_element(),
        ]
    }

    /// The Bot's chats, most recent first; each row opens its chat.
    fn chats_section(&self, bot: &BotRecord, cx: &Context<Self>) -> Section {
        let mut chats: Vec<_> = self
            .client
            .state
            .sessions
            .iter()
            .filter(|session| session.session_context.owner_id == bot.id)
            .collect();
        chats.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        let key = format!("chats:{}", bot.id);
        let shown = self.shown(&key, CHATS_PAGE);
        let mut group = Group::new();
        if chats.is_empty() {
            group = group.child(Row::empty("no-chats", "No chats yet."));
        }
        for session in chats.iter().take(shown) {
            let id = session.session_id.share();
            let title = single_line(&crate::ui::navigation::session_title(session));
            group = group.child(
                Row::new(SharedString::from(format!("bot-chat:{id}")), title)
                    .truncate()
                    .leading(Leading::Glyph("hi.note01".into()))
                    .value(ago(session.updated_at))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_page(PageTab::Chat(id.share()), window, cx);
                    })),
            );
        }
        if chats.len() > shown {
            group = group.child(Self::show_more(key, CHATS_PAGE, cx));
        }
        Section::new("Conversations").child(group)
    }

    /// One row per settings section with its current value; each opens that section.
    fn configuration_section(&self, bot: &BotRecord, cx: &Context<Self>) -> Section {
        let color = SharedString::new_static(Accent::from(bot.tint).label());
        let not_set = |text: SharedString| {
            if text.is_empty() {
                "Not set".into()
            } else {
                text
            }
        };
        let values: Vec<SharedString> = match &bot.config.config {
            HarnessConfig::Mobius(config) => {
                let enabled = self.client.state.gateway.as_ref().map_or(0, |gateway| {
                    gateway
                        .agent_catalog
                        .middleware_features
                        .iter()
                        .filter(|feature| {
                            feature.required || config.middleware.enabled(&feature.id)
                        })
                        .count()
                });
                vec![
                    color,
                    not_set(single_line(&config.system_prompt)),
                    self.model_summary(&config.provider).into(),
                    format!("{enabled} enabled").into(),
                ]
            }
            HarnessConfig::Codex(_) | HarnessConfig::Claude(_) => vec![color],
        };
        let mut group = Group::new();
        for (&section, value) in AgentSection::for_harness(bot.harness).iter().zip(values) {
            let id = Arc::clone(&bot.id);
            group = group.child(
                Row::new(
                    SharedString::from(format!("bot-config:{}", section.label())),
                    section.label(),
                )
                .leading(Leading::glyph(section.glyph()))
                .value(value)
                .disclosure()
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_page(PageTab::BotSettings(Arc::clone(&id), section), window, cx);
                })),
            );
        }
        Section::new("Configuration").child(group)
    }

    /// Where Bot pages go when their Bot disappears.
    pub(in crate::ui) fn open_bots_landing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_page(PageTab::Empty(Rail::Bots), window, cx);
    }
}

impl Workspace {
    pub(in crate::ui) fn open_bot_page(
        &mut self,
        id: Arc<str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_page(PageTab::Bot(id), window, cx);
    }
}
