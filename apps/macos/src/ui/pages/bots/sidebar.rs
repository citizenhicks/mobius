//! The Bots list, and the agent defaults and extensions rows of the möbius list (WP4).

use crate::ui::Workspace;
use crate::ui::navigation::sidebar_list;
use crate::ui::primitives::{Leading, SidebarRow, glyph, icon, palette};
use crate::ui::router::{AgentSection, PageTab};
use crate::ui::theme::Accent;
use gpui::{AnyElement, Context, IntoElement as _, ParentElement as _, Styled as _};
use mobius_gateway::wire::BotRecord;
use std::sync::Arc;

impl Workspace {
    /// New Bot, then Bots by name; the selected Bot expands to its overview and settings.
    pub(in crate::ui) fn bots_list(&self, cx: &Context<Self>) -> AnyElement {
        let mut list = sidebar_list().child(
            SidebarRow::new("new-bot", "New Bot")
                .leading(Leading::glyph("hi.plus"))
                .disabled(!self.client.link.is_ready())
                .on_click(cx.listener(|this, _, window, cx| this.create_bot_dialog(window, cx))),
        );
        let Some(gateway) = &self.client.state.gateway else {
            return list.into_any_element();
        };
        let mut bots: Vec<_> = gateway.bots.iter().collect();
        bots.sort_by_key(|bot| bot.name.to_lowercase());
        for bot in bots {
            let face = Leading::Face(Accent::from(bot.tint).color(), bot.shape);
            let awaiting = gateway
                .background_approvals
                .iter()
                .any(|approval| approval.bot_id == bot.id);
            let mut row = self
                .page_row(
                    format!("bot:{}", bot.id),
                    Arc::clone(&bot.name),
                    face,
                    PageTab::Bot(Arc::clone(&bot.id)),
                    cx,
                )
                .meta(format!("@{}", bot.handle))
                .selected(false)
                .caret(self.disclosure_shown(&bot.id) > 0.5);
            if awaiting {
                row = row.status(icon("hi.bellDot", &palette(cx)).size(glyph::MARK));
            }
            if let Some(reason) = crate::ui::pages::harness::unavailable(bot.harness) {
                row = row.muted().subtitle(reason);
            }
            list = list
                .child(row)
                .children(self.sidebar_disclosure(&bot.id, || self.bot_children(bot, cx)));
        }
        list.into_any_element()
    }

    /// Overview and the settings sections, text aligned with their Bot's name.
    fn bot_children(&self, bot: &BotRecord, cx: &Context<Self>) -> Vec<SidebarRow> {
        let id = &bot.id;
        let overview = self.page_row(
            format!("bot-overview:{id}"),
            "Overview",
            Leading::Gutter,
            PageTab::Bot(Arc::clone(id)),
            cx,
        );
        let sections = AgentSection::for_harness(bot.harness)
            .iter()
            .map(|&section| {
                self.page_row(
                    format!("bot-{}:{id}", section.label()),
                    section.label(),
                    Leading::Gutter,
                    PageTab::BotSettings(Arc::clone(id), section),
                    cx,
                )
            });
        std::iter::once(overview).chain(sections).collect()
    }

    /// The agent defaults' sections, text aligned with their group's name.
    pub(in crate::ui) fn defaults_rows(&self, cx: &Context<Self>) -> Vec<SidebarRow> {
        AgentSection::DEFAULTS
            .into_iter()
            .map(|section| {
                self.page_row(
                    format!("defaults-{}", section.label()),
                    section.label(),
                    Leading::Gutter,
                    PageTab::AgentDefaults(section),
                    cx,
                )
            })
            .collect()
    }

    /// Install extension, then the installed extensions by name.
    pub(in crate::ui) fn extension_rows(&self, cx: &Context<Self>) -> Vec<SidebarRow> {
        let mut rows = vec![
            SidebarRow::new("install-extension", "Install extension")
                .leading(Leading::glyph("hi.plus"))
                .disabled(!self.client.link.is_ready())
                .on_click(
                    cx.listener(|this, _, window, cx| this.install_extension_dialog(window, cx)),
                ),
        ];
        let Some(gateway) = &self.client.state.gateway else {
            return rows;
        };
        let mut extensions: Vec<_> = gateway.agent_catalog.extensions.iter().collect();
        extensions.sort_by_key(|extension| extension.name.to_lowercase());
        for extension in extensions {
            let mut row = self
                .page_row(
                    format!("extension:{}", extension.id),
                    Arc::clone(&extension.name),
                    Leading::Gutter,
                    PageTab::Extension(extension.id.to_string()),
                    cx,
                )
                .meta(match extension.kind {
                    mobius_gateway::wire::ExtensionKind::Plugin => "Plugin",
                    mobius_gateway::wire::ExtensionKind::Skill => "Skill",
                });
            if !extension.hooks.is_empty() && !extension.hooks_trusted {
                row = row.status(icon("hi.shieldAlert", &palette(cx)).size(glyph::MARK));
            }
            rows.push(row);
        }
        rows
    }
}
