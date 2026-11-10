//! The Chats sidebar: header actions, the inline filter and the cached catalog region.

use super::catalog::{Catalog, Organization, Project, session_title, workspace_name};
use super::*;
use crate::ui::chrome::{MenuScope, session_menu};
use gpui_component::Side;
use gpui_component::collapsible::Collapsible;
use gpui_component::menu::{ContextMenuExt, DropdownMenu, PopupMenu, PopupMenuItem};
use mobius_gateway::wire::{BotShape, SessionActivityState, SessionRecord};
use std::collections::BTreeSet;

impl Workspace {
    /// The chat open as the primary page, which the catalog shows selected.
    pub(in crate::ui) fn selected_chat(&self) -> Option<&str> {
        match self.primary_page() {
            Some(PageTab::Chat(id)) => Some(id),
            _ => None,
        }
    }

    pub(super) fn chats_actions(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let catalog = &self.navigation_catalog;
        let attention = self
            .client
            .state
            .sessions
            .iter()
            .any(|session| catalog.needs_attention(session));
        let bell = match (catalog.attention_only, attention) {
            (true, _) => "hi.bellOff",
            (false, true) => "hi.bellDot",
            (false, false) => "hi.bell",
        };
        let bell_label = if catalog.attention_only {
            "Show all chats"
        } else {
            "Show active and unread chats"
        };
        let recaps = self.recaps_available().then(|| {
            toolbar_button("chat-recaps", "While you were away", "hi.aiSearchLines", cx)
                .on_click(cx.listener(|this, _, window, cx| this.open_recaps(window, cx)))
                .into_any_element()
        });
        let search = toolbar_button(
            "chat-search",
            "Search Möbius · ⌘K",
            "hi.magnifyingGlass",
            cx,
        )
        .on_click(cx.listener(|this, _, window, cx| this.command_palette(window, cx)))
        .into_any_element();
        let bell = toolbar_button("chat-attention", bell_label, bell, cx)
            .toggle(catalog.attention_only, cx)
            .on_click(cx.listener(|this, _, _, cx| {
                let catalog = &mut this.navigation_catalog;
                catalog.attention_only = !catalog.attention_only;
                cx.notify();
            }))
            .into_any_element();
        [
            Some(search),
            recaps,
            Some(bell),
            Some(self.organize_menu(cx)),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    fn organize_menu(&self, cx: &Context<Self>) -> AnyElement {
        let view = cx.entity().downgrade();
        let current = self.navigation_catalog.organization;
        let filter = self.navigation_catalog.bot_filter.clone(); // rules-allow: the 'static menu builder needs its own copy of the filter
        let mut bots: Vec<_> = self
            .client
            .state
            .gateway
            .iter()
            .flat_map(|gateway| &gateway.bots)
            .map(|bot| {
                (
                    Arc::clone(&bot.id),
                    SharedString::from(format!("{} (@{})", bot.name, bot.handle)),
                    Accent::from(bot.tint).color(),
                    bot.shape,
                )
            })
            .collect();
        bots.sort_by_key(|(_, label, _, _)| label.to_lowercase());
        toolbar_button(
            "chat-organize",
            "Organize and filter chats",
            "hi.dotsThree",
            cx,
        )
        .toggle(!filter.is_empty(), cx)
        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
            organize_items(menu, &view, current, &filter, &bots)
        })
        .on_open_change(cx.listener(|this, open, _, cx| this.overlay_changed(*open, cx)))
        .into_any_element()
    }

    /// The catalog region; its key lives beside the sidebar that places it.
    pub(in crate::ui) fn catalog_region(&self, _: &mut Window, cx: &Context<Self>) -> AnyElement {
        #[cfg(feature = "snapshot")]
        crate::ui::snapshot::count_render(2);
        let current = self
            .client
            .state
            .session
            .as_ref()
            .and_then(|session| session.workspace.as_ref())
            .map(|workspace| workspace.id.as_str());
        let catalog = self.navigation_catalog.catalog(
            self.client
                .state
                .sessions
                .iter()
                .filter(|session| self.conversation_bot(&session.session_id).is_none()),
            current,
            self.navigation_catalog
                .pinned_in(self.client.endpoint.as_str()),
        );
        let list = sidebar_list()
            .id("chat-catalog")
            .track_scroll(&self.shell.catalog_scroll)
            .child(self.bot_conversation_rail(cx))
            .child(
                SidebarRow::new("new-chat", "New chat")
                    .leading(Leading::glyph("hi.notePencil"))
                    .on_click(cx.listener(|this, _, window, cx| this.new_chat(window, cx))),
            );
        if catalog.is_empty() {
            return list.child(self.catalog_empty()).into_any_element();
        }
        list.children(self.catalog_groups(&catalog, cx))
            .into_any_element()
    }

    fn catalog_groups(&self, catalog: &Catalog<'_>, cx: &Context<Self>) -> Vec<AnyElement> {
        let mut groups = Vec::new();
        if !catalog.pinned.is_empty() {
            let collapsed = self.navigation_catalog.pinned_collapsed;
            groups.push(
                div()
                    .id("pinned-header")
                    .cursor_pointer()
                    .role(gpui::accesskit::Role::Button)
                    .aria_label(if collapsed {
                        "Pinned, collapsed"
                    } else {
                        "Pinned, expanded"
                    })
                    .child(sidebar_label("Pinned").caret(!collapsed))
                    .on_click(cx.listener(|this, _, _, cx| {
                        let catalog = &mut this.navigation_catalog;
                        catalog.pinned_collapsed = !catalog.pinned_collapsed;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
            groups.push(
                Collapsible::new()
                    .motion_id("pinned-reveal")
                    .open(!collapsed)
                    .content(
                        div()
                            .w_full()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .children(
                                catalog
                                    .pinned
                                    .iter()
                                    .map(|s| self.catalog_row(s, false, cx)),
                            )
                            .into_any_element(),
                    )
                    .into_any_element(),
            );
        }
        if !catalog.projects.is_empty() {
            groups.push(sidebar_label("Projects").into_any_element());
            groups.extend(catalog.projects.iter().map(|p| self.catalog_project(p, cx)));
        }
        if !catalog.recent.is_empty() {
            groups.push(sidebar_label("Recent").into_any_element());
            groups.extend(catalog.recent.iter().map(|s| self.catalog_row(s, true, cx)));
        }
        groups
    }

    fn catalog_empty(&self) -> AnyElement {
        let catalog = &self.navigation_catalog;
        if !self.client.link.is_ready() && self.client.state.sessions.is_empty() {
            return loading_rows(6).into_any_element();
        }
        let message = if catalog.attention_only {
            "No chats need attention"
        } else if !catalog.bot_filter.is_empty() {
            "No chats for the selected Bots"
        } else {
            "No chats yet"
        };
        div()
            .pt(space::L)
            .flex()
            .flex_col()
            .items_center()
            .child(icon("hi.note01", &self.palette).size(glyph::HERO))
            .child(sidebar_note(message))
            .into_any_element()
    }

    fn catalog_project(&self, project: &Project<'_>, cx: &Context<Self>) -> AnyElement {
        let id: Arc<str> = project.id.into();
        let collapsed = self.navigation_catalog.collapsed.contains(&id);
        let name = SharedString::from(workspace_name(project.path).to_owned());
        let key = SharedString::from(format!("project:{id}"));
        let group = SharedString::from(format!("project-header:{id}"));
        let revealed = self.navigation_catalog.menu_open.as_deref() == Some(key.as_ref());
        let toggle = Arc::clone(&id);
        let header = SidebarRow::new(SharedString::clone(&key), SharedString::clone(&name))
            .leading(Leading::glyph(if collapsed {
                "hi.folder"
            } else {
                "hi.folderOpen"
            }))
            .caret(!collapsed)
            .actions_slot(
                SharedString::clone(&group),
                height::COMPACT * 2. + space::XXS,
                revealed,
            )
            .when(project.pinned, |row| {
                row.status(icon("hi.pushPin", &palette(cx)).size(glyph::MARK))
            })
            .accessibility_label(format!(
                "{name}, {}{}",
                if project.pinned { "pinned, " } else { "" },
                if collapsed { "collapsed" } else { "expanded" }
            ))
            .tooltip(project.path.to_owned())
            .on_click(cx.listener(move |this, _, _, cx| {
                let collapsed = &mut this.navigation_catalog.collapsed;
                if !collapsed.remove(&toggle) {
                    collapsed.insert(Arc::clone(&toggle));
                }
                cx.notify();
            }));
        let path = project.path.to_owned();
        let new_chat = row_button(
            SharedString::from(format!("new-in:{id}")),
            format!("New chat in {name}"),
            "hi.notePencil",
            cx,
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            // The header row underneath would take the click too and fold the project.
            cx.stop_propagation();
            this.new_chat(window, cx);
            this.draft_workspace = path.clone().into(); // rules-allow: the click handler is Fn and the draft owns its workspace path
        }));
        let view = cx.entity().downgrade();
        let more = {
            let (view, id) = (WeakEntity::clone(&view), Arc::clone(&id));
            more_button(
                format!("more-{key}"),
                "More project actions",
                key,
                cx,
                move |menu, _, cx| projects::project_menu(menu, &view, &id, cx),
            )
        };
        let menu_id = Arc::clone(&id);
        div()
            .mb(space::S)
            .child(
                div()
                    .id(SharedString::clone(&group))
                    .group(SharedString::clone(&group))
                    .relative()
                    .child(header)
                    .child(
                        row_actions(SharedString::clone(&group), revealed, true)
                            .child(new_chat)
                            .child(more),
                    )
                    .children(probe(&group))
                    .context_menu(move |menu, _, cx| {
                        projects::project_menu(menu, &view, &menu_id, cx)
                    }),
            )
            .child(
                Collapsible::new()
                    .motion_id(SharedString::from(format!("project-reveal:{id}")))
                    .open(!collapsed)
                    .content(self.catalog_project_rows(project, cx)),
            )
            .into_any_element()
    }

    fn catalog_project_rows(&self, project: &Project<'_>, cx: &Context<Self>) -> AnyElement {
        let id = project.id;
        let visible = self.navigation_catalog.visible(id);
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .children(
                project
                    .sessions
                    .iter()
                    .take(visible)
                    .map(|session| self.catalog_row(session, false, cx)),
            )
            .when(visible < project.sessions.len(), |view| {
                let id: Arc<str> = id.into();
                view.child(
                    show_more_row(SharedString::from(format!("more:{id}")))
                        .accessibility_label(format!(
                            "Show more chats in {}",
                            workspace_name(project.path)
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let more = visible + catalog::PAGE;
                            this.navigation_catalog
                                .visible
                                .insert(Arc::clone(&id), more);
                            cx.notify();
                        })),
                )
            })
            .into_any_element()
    }

    fn catalog_row(
        &self,
        session: &SessionRecord,
        show_workspace: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let id = session.session_id.share();
        let title = SharedString::from(session_title(session));
        let key = SharedString::from(format!("chat:{id}"));
        let group = SharedString::from(format!("chat-row:{id}"));
        let revealed = self.navigation_catalog.menu_open.as_deref() == Some(key.as_ref());
        let mut row = SidebarRow::new(SharedString::clone(&key), SharedString::clone(&title))
            .scroll_title()
            .when(cx.reduce_motion(), |row| row.tooltip(title))
            .selected(self.selected_chat() == Some(id.as_ref()))
            .actions_slot(SharedString::clone(&group), height::COMPACT, revealed)
            .on_click(cx.listener({
                let id = id.share();
                move |this, _, window, cx| {
                    this.open_page(PageTab::Chat(id.share()), window, cx);
                }
            }));
        if show_workspace {
            row = row.flush();
            if let Some(path) = &session.session_context.workspace_label {
                row = row.subtitle(workspace_name(path).to_owned());
            }
        }
        if let Some(status) = self.session_status(session) {
            row = row.status(status);
        }
        let view = cx.entity().downgrade();
        let more = {
            let (view, id) = (WeakEntity::clone(&view), id.share());
            more_button(
                format!("more-{key}"),
                "More chat actions",
                key,
                cx,
                move |menu, window, cx| session_menu(menu, &view, &id, MenuScope::Row, window, cx),
            )
        };
        let reveal = SharedString::from(format!("chat-reveal:{id}"));
        let row = div()
            .id(SharedString::clone(&group))
            .group(SharedString::clone(&group))
            .w_full()
            .min_w_0()
            .relative()
            .child(row)
            .child(
                row_actions(SharedString::clone(&group), revealed, false)
                    .child(more)
                    .map(|actions| {
                        #[cfg(feature = "snapshot")]
                        let actions = actions
                            .child(crate::ui::snapshot::probe(format!("row-actions:chat:{id}")));
                        actions
                    }),
            )
            .children(probe(&group))
            .context_menu(move |menu, window, cx| {
                session_menu(menu, &view, &id, MenuScope::Row, window, cx)
            })
            .into_any_element();
        // Rows slide when others arrive, leave or reorder; a chat that appears later grows in.
        let row = motion::fade_in(div().w_full().child(row), reveal, motion::STANDARD, cx);
        let grow =
            motion::smooth(format!("grow:{group}"), row).grow_in(self.navigation_catalog.tracking);
        motion::slide(format!("slide:{group}"), grow).into_any_element()
    }

    /// The status of chat `id`, as its sidebar row shows it.
    pub(in crate::ui) fn chat_status(&self, id: &str) -> Option<AnyElement> {
        let session = self.client.state.session_record(id)?;
        self.session_status(session)
    }

    /// Needing the reader, running, awaiting approval, or finished while unread
    /// (SessionActivityIndicator).
    fn session_status(&self, session: &SessionRecord) -> Option<AnyElement> {
        let p = self.palette;
        let id = &session.session_id;
        let (state, unread) = (session.activity.state, self.is_unread(id));
        #[cfg(feature = "snapshot")]
        let (state, unread) = crate::ui::snapshot::activity(id).unwrap_or((state, unread));
        if session.activity.attention > 0 {
            return Some(attention_pill(session.activity.attention, &p).into_any_element());
        }
        if state == SessionActivityState::AwaitingApproval {
            return Some(
                icon("hi.mousePointerClick", &p)
                    .size(glyph::MARK)
                    .text_color(p.warning)
                    .into_any_element(),
            );
        }
        match state {
            SessionActivityState::Running => Some(Spinner::new(glyph::MARK).into_any_element()),
            SessionActivityState::Idle if unread => Some(
                div()
                    .size(glyph::UNREAD)
                    .pill_corners()
                    .bg(p.accent)
                    .into_any_element(),
            ),
            SessionActivityState::Idle | SessionActivityState::AwaitingApproval => None,
        }
    }

    pub(in crate::ui) fn is_unread(&self, session_id: &str) -> bool {
        self.navigation_catalog.unread.contains(session_id)
    }

    pub(in crate::ui) fn set_unread(&mut self, session_id: &SessionId, unread: bool) {
        if unread {
            self.navigation_catalog.unread.insert(session_id.share());
        } else {
            self.navigation_catalog.unread.remove(session_id);
        }
    }

    /// Marks chats that finished in the background as unread and tells the user (flows-10):
    /// a toast while the window is active, a system notification otherwise.
    pub(super) fn track_activity(&mut self, window: &Window) {
        let active = window.is_window_active();
        let visible = self.primary_page().and_then(|page| match page {
            PageTab::Chat(id) if active => Some(id.share()),
            _ => None,
        });
        let (mut running, mut awaiting) = (BTreeSet::new(), BTreeSet::new());
        let mut news = Vec::new();
        let catalog = &mut self.navigation_catalog;
        for session in &self.client.state.sessions {
            let id = &session.session_id;
            let shown = visible.as_ref() == Some(id);
            if shown {
                self.shell.recaps.read(session);
            }
            let previous_attention = catalog
                .attention
                .insert(id.share(), session.activity.attention)
                .unwrap_or(0);
            if catalog.tracking && session.activity.attention > previous_attention && !shown {
                news.push((
                    id.share(),
                    format!("{} has a question.", session_title(session)),
                ));
            }
            let news_for = match session.activity.state {
                SessionActivityState::Running => {
                    running.insert(id.share());
                    None
                }
                SessionActivityState::AwaitingApproval => {
                    awaiting.insert(id.share());
                    (!catalog.awaiting.contains(id)).then_some("needs approval.")
                }
                SessionActivityState::Idle => {
                    let finished = catalog.running.contains(id) || catalog.awaiting.contains(id);
                    if finished && !shown {
                        catalog.unread.insert(id.share());
                    }
                    finished.then_some("finished.")
                }
            };
            if let Some(what) = news_for.filter(|_| catalog.tracking && !shown) {
                news.push((id.share(), format!("{} {what}", session_title(session))));
            }
        }
        catalog.running = running;
        catalog.awaiting = awaiting;
        catalog.tracking = !self.client.state.sessions.is_empty();
        for (id, message) in news {
            if active {
                self.announce(Tone::Info, message);
            } else if let Err(error) = self.native.notify(&id, "Möbius", &message) {
                tracing::warn!(%error, "system notification not shown");
            }
        }
    }
}

/// A row's ⋯ button; while its menu is open the row keeps it shown under `key`.
fn more_button(
    id: String,
    label: &'static str,
    key: SharedString,
    cx: &Context<Workspace>,
    menu: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
) -> AnyElement {
    row_button(SharedString::from(id), label, "hi.dotsThree", cx)
        .dropdown_menu_with_anchor(Anchor::TopRight, menu)
        .on_open_change(cx.listener(move |this, open: &bool, _, cx| {
            let shown = &mut this.navigation_catalog.menu_open;
            if *open {
                *shown = Some(key.to_string()); // rules-allow: menu_open keeps an owned String, which the snapshot harness also writes
            } else if shown.as_deref() == Some(key.as_ref()) {
                *shown = None;
            }
            this.overlay_changed(*open, cx);
        }))
        .into_any_element()
}

/// Snapshot scenarios find rows to click by their group name.
#[cfg(feature = "snapshot")]
fn probe(group: &SharedString) -> impl IntoIterator<Item = AnyElement> {
    std::iter::once(crate::ui::snapshot::probe(group.to_string()).into_any_element())
}

#[cfg(not(feature = "snapshot"))]
fn probe(_: &SharedString) -> impl IntoIterator<Item = AnyElement> {
    std::iter::empty()
}

/// The overlay for a row's actions, over its reserved slot: shown while the row is
/// hovered or `revealed`.
fn row_actions(group: SharedString, revealed: bool, caret: bool) -> Div {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .right(row_actions_inset(caret))
        .flex()
        .items_center()
        .gap(space::XXS)
        .when(!revealed, |actions| {
            actions
                .invisible()
                .group_hover(group, gpui::Styled::visible)
        })
}

fn organize_items(
    mut menu: PopupMenu,
    view: &WeakEntity<Workspace>,
    current: Organization,
    filter: &BTreeSet<Arc<str>>,
    bots: &[(Arc<str>, SharedString, Hsla, BotShape)],
) -> PopupMenu {
    let update = |view: &WeakEntity<Workspace>,
                  change: fn(&mut catalog::CatalogState, &Arc<str>)| {
        let view = WeakEntity::clone(view);
        move |id: Arc<str>| {
            let view = WeakEntity::clone(&view);
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let changed = view.update(cx, |this, cx| {
                    change(&mut this.navigation_catalog, &id);
                    cx.notify();
                });
                if let Err(error) = changed {
                    tracing::debug!(%error, "chat filter change on a released workspace");
                }
            }
        }
    };
    menu = menu.check_side(Side::Right).label("Organize");
    for organization in Organization::ALL {
        let view = WeakEntity::clone(view);
        menu = menu.item(
            glyph_item(organization.title(), organization.glyph())
                .checked(organization == current)
                .on_click(move |_, _, cx| {
                    let changed = view.update(cx, |this, cx| {
                        this.navigation_catalog.organization = organization;
                        cx.notify();
                    });
                    if let Err(error) = changed {
                        tracing::debug!(%error, "chat organization change on a released workspace");
                    }
                }),
        );
    }
    let clear = update(view, |catalog, _| catalog.bot_filter.clear());
    menu = menu.separator().label("Filter").item(
        glyph_item("All", "bot.face")
            .checked(filter.is_empty())
            .on_click(clear("".into())),
    );
    let toggle = update(view, |catalog, id| {
        if !catalog.bot_filter.remove(id) {
            catalog.bot_filter.insert(Arc::clone(id));
        }
    });
    for (id, label, tint, shape) in bots {
        menu = menu.item(
            BotFace::new(*tint, *shape, glyph::INLINE)
                .menu_item(SharedString::clone(label))
                .checked(filter.contains(id))
                .on_click(toggle(Arc::clone(id))),
        );
    }
    menu
}

/// An organize menu row led by the inline glyph `name`.
fn glyph_item(label: &'static str, name: &'static str) -> PopupMenuItem {
    leading_item(label, move |cx| {
        icon(name, &palette(cx))
            .size(glyph::INLINE)
            .into_any_element()
    })
}

/// A yellow capsule holding the attention cursor and how many things wait on the reader.
fn attention_pill(count: u32, p: &Palette) -> Div {
    let ink = gpui::black();
    div()
        .flex()
        .items_center()
        .gap(space::XXS)
        .px(space::XS)
        .pill_corners()
        .bg(p.warning)
        .typography(Type::Label)
        .text_color(ink)
        .child(
            icon("hi.mousePointerClick", p)
                .size(glyph::MARK)
                .text_color(ink),
        )
        .child(count.to_string())
}
