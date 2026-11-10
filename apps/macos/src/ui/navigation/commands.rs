//! The ⌘K palette: actions with their shortcuts, then chats, sections, Bots, providers and tabs.

use super::catalog::{session_title, workspace_name};
use super::shortcuts::SHORTCUTS;
use super::*;
use gpui_component::command::{Command, CommandGroup, CommandItem, CommandState};
use std::rc::Rc;

#[derive(Clone)]
enum Target {
    /// The item's Action already ran; only the palette closes.
    Action,
    Section(Section),
    Page(PageTab),
}

/// Groups and their targets, indexed alike by `[section][row]`.
#[derive(Default)]
struct Palette {
    groups: Vec<CommandGroup>,
    targets: Vec<Vec<Target>>,
}

impl Palette {
    fn group(&mut self, label: &str, items: Vec<(CommandItem, Target)>) {
        if items.is_empty() {
            return;
        }
        let (items, targets): (Vec<_>, Vec<_>) = items.into_iter().unzip();
        self.groups
            .push(CommandGroup::new().label(label.to_owned()).items(items));
        self.targets.push(targets);
    }
}

fn action(label: &str, glyph: &str, action: Box<dyn Action>, cx: &App) -> (CommandItem, Target) {
    let item = CommandItem::new()
        .label(label.to_owned())
        .icon(icon(glyph, &palette(cx)))
        .action(action);
    (item, Target::Action)
}

fn page(
    label: impl Into<SharedString>,
    glyph: &str,
    target: Target,
    cx: &App,
) -> (CommandItem, Target) {
    (
        CommandItem::new()
            .label(label)
            .icon(icon(glyph, &palette(cx))),
        target,
    )
}

impl Workspace {
    fn palette_chats(&self) -> Vec<(CommandItem, Target)> {
        let mut sessions: Vec<_> = self.client.state.sessions.iter().collect();
        sessions.sort_by_key(|s| (!s.pinned, std::cmp::Reverse(s.updated_at)));
        let bots = self
            .client
            .state
            .gateway
            .as_ref()
            .map(|gateway| &gateway.bots);
        let muted = self.palette.muted;
        sessions
            .into_iter()
            .map(|session| {
                let label = SharedString::from(session_title(session));
                let path = SharedString::from(
                    session
                        .session_context
                        .workspace_label
                        .clone() // rules-allow: the palette item owns its path keyword while the catalog keeps its own
                        .unwrap_or_default(),
                );
                let handle = bots
                    .and_then(|bots| {
                        bots.iter()
                            .find(|b| b.id == session.session_context.owner_id)
                    })
                    .map(|bot| format!("@{}", bot.handle));
                let keywords = [
                    Some(session.session_id.to_string().into()),
                    handle.map(SharedString::from),
                    Some(SharedString::clone(&path)),
                ];
                let name = SharedString::from(workspace_name(&path).to_owned());
                let glyph = if session.pinned {
                    "hi.pushPin"
                } else {
                    "hi.note01"
                };
                let item = CommandItem::new()
                    .label(SharedString::clone(&label))
                    .keywords(keywords.into_iter().flatten())
                    .child(move |_, cx| {
                        div()
                            .flex()
                            .items_center()
                            .gap(space::S)
                            .w_full()
                            .child(icon(glyph, &palette(cx)).size(glyph::INLINE))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(SharedString::clone(&label)),
                            )
                            .child(
                                div()
                                    .typography(Type::Caption)
                                    .text_color(muted)
                                    .child(SharedString::clone(&name)),
                            )
                    });
                (
                    item,
                    Target::Page(PageTab::Chat(session.session_id.share())),
                )
            })
            .collect()
    }

    fn palette_model(&self, cx: &App) -> Palette {
        let mut palette = Palette::default();
        let actions = SHORTCUTS.iter().filter_map(|shortcut| {
            let glyph = shortcut.palette?;
            Some(action(shortcut.label, glyph, (shortcut.action)(), cx))
        });
        palette.group("Actions", actions.collect());
        palette.group("Chats", self.palette_chats());
        let sections = Section::ALL.map(|section| {
            page(
                section.label(),
                section.glyph(),
                Target::Section(section),
                cx,
            )
        });
        palette.group("Go to", sections.into());
        let gateway = self.client.state.gateway.as_ref();
        let bots = gateway.iter().flat_map(|gateway| &gateway.bots).map(|bot| {
            let label = SharedString::from(format!("{} · @{}", bot.name, bot.handle));
            let (tint, shape) = (Accent::from(bot.tint).color(), bot.shape);
            (
                CommandItem::new()
                    .label(SharedString::clone(&label))
                    .child(move |_, _| {
                        div()
                            .flex()
                            .items_center()
                            .gap(space::S)
                            .child(BotFace::new(tint, shape, glyph::INLINE))
                            .child(SharedString::clone(&label))
                    }),
                Target::Page(PageTab::Bot(Arc::clone(&bot.id))),
            )
        });
        palette.group("Bots", bots.collect());
        let providers = gateway
            .iter()
            .flat_map(|g| &g.provider_instances)
            .map(|provider| {
                let id = provider.selection.instance.clone(); // rules-allow: the palette target owns its provider id while the gateway keeps its own
                let label = provider.label.clone(); // rules-allow: the palette item owns its label while the gateway keeps its own
                page(
                    label,
                    "hi.plugsConnected",
                    Target::Page(PageTab::Provider(id)),
                    cx,
                )
            });
        palette.group("Providers", providers.collect());
        let tabs = self.visible_content().map(|tab| {
            page(
                SharedString::clone(&tab.title),
                "hi.folderOpen",
                Target::Page(PageTab::Content(Arc::clone(&tab.id))),
                cx,
            )
        });
        palette.group("Open tabs", tabs.collect());
        palette
    }

    pub(in crate::ui) fn command_palette(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Palette { groups, targets } = self.palette_model(cx);
        let state = cx.new(|cx| CommandState::new(window, cx));
        let focus = state.read(cx).focus_handle(cx);
        let view = cx.entity().downgrade();
        let targets = Rc::new(targets);
        window.open_dialog(cx, move |dialog, _, _| {
            let (view, targets) = (WeakEntity::clone(&view), Rc::clone(&targets));
            let command = groups
                .iter()
                .cloned() // rules-allow: the dialog builder runs on every render and each Command owns its groups
                .fold(
                    Command::new(&state)
                        .placeholder("Search chats, Bots, providers and commands…")
                        .bordered(false)
                        // Inside the dialog's hairline, so its fill keeps the dialog's corners.
                        .inner_corners(radius::TILE, inset::HAIRLINE)
                        .max_h(height::DIALOG_LIST_MAX),
                    Command::group,
                );
            dialog
                .width(width::PALETTE)
                .close_button(false)
                .p_0()
                .child(command.on_confirm(move |index, window, cx| {
                    let target = targets
                        .get(index.section)
                        .and_then(|g| g.get(index.row))
                        .cloned(); // rules-allow: the confirm handler is Fn and the opened page owns its target
                    window.close_dialog(cx);
                    let opened = view.update(cx, |this, cx| match target {
                        Some(Target::Section(section)) => this.select_section(section, window, cx),
                        Some(Target::Page(PageTab::Bot(id))) => this.open_bot_page(id, window, cx),
                        Some(Target::Page(page)) => this.open_page(page, window, cx),
                        Some(Target::Action) | None => {}
                    });
                    if let Err(error) = opened {
                        tracing::debug!(%error, "palette choice on a released workspace");
                    }
                }))
        });
        window.defer(cx, move |window, cx| focus.focus(window, cx));
    }
}
