//! The document strip in the titlebar, the pane toggle and the open-tabs badge (spec §1.10).

use super::*;
use gpui_component::menu::{ContextMenuExt, DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_component::scroll::ScrollableMask;
use std::rc::Rc;
use std::sync::Arc;

const TAB_MIN: Pixels = px(112.);
const TAB_MAX: Pixels = px(240.);

impl ContentTab {
    pub(in crate::ui) fn glyph(&self) -> &'static str {
        match &self.content {
            TabContent::Code(doc) => doc.kind.glyph,
            TabContent::Diff(_) => "hi.plusMinus02",
            TabContent::Files(_) => "hi.folder",
            TabContent::Preview { preview, .. } => preview_glyph(preview.source.symbol.as_ref()),
            TabContent::Routine(_) => "hi.workflowSquare01",
            TabContent::Launcher => "hi.plus",
            TabContent::File(_) => FileKind::for_name(&self.title).glyph,
            TabContent::Terminal(_) => "hi.terminalWindow",
            TabContent::AgentBrowser => agent_browser::GLYPH,
            TabContent::Browser if self.id.starts_with("browser:chat:") => desktop::GLYPH,
            TabContent::Browser => browser::GLYPH,
            TabContent::Desktop => desktop::GLYPH,
            TabContent::Detail { glyph, .. } => glyph,
            TabContent::Loading { .. } | TabContent::Failed(_) => "hi.fileText",
        }
    }
}

impl Workspace {
    /// The titlebar strip over the pane: `split` is the pane width beside the chat, or
    /// `None` for the full view, which leads with the chat's own tab. `None` when no
    /// document is showing.
    pub(in crate::ui) fn document_strip(
        &self,
        split: Option<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let active = self.active_content().map(|tab| &*tab.id);
        if active.is_none() && !self.chat_tab_selected() {
            return None;
        }
        let p = self.palette;
        let mut tabs = self
            .page_tabs
            .iter()
            .enumerate()
            .filter_map(|(index, page)| match page {
                PageTab::Content(id) => self.content.tabs.get(id).map(|tab| (index, tab)),
                _ => None,
            })
            .map(|(index, tab)| {
                let (id, glyph) = (&*tab.id, tab.glyph());
                let title = SharedString::clone(&tab.title);
                let mention = self.tab_mention(id, cx);
                DocumentTab::new(id, glyph, title, active == Some(id), window, cx)
                    .when(glyph == desktop::GLYPH, |tab| {
                        let status = if self.client.link.is_loading() {
                            Spinner::new(glyph::INLINE)
                                .color(p.muted)
                                .into_any_element()
                        } else {
                            status_dot(self.client.link.tone().color(&p)).into_any_element()
                        };
                        tab.status(status)
                    })
                    .page(index)
                    .on_select(
                        cx.listener(move |this, _, window, cx| this.select_page(index, window, cx)),
                    )
                    .on_close(
                        cx.listener(move |this, _, window, cx| this.close_page(index, window, cx)),
                    )
                    .when_some(mention, |tab, mention| {
                        tab.menu(Self::file_menu(mention, cx))
                    })
            })
            .collect::<Vec<_>>();
        if let Some(primary) = split
            .is_none()
            .then(|| self.primary_tab(active.is_none(), window, cx))
            .flatten()
        {
            tabs.insert(0, primary);
        }
        Some(
            div()
                .flex()
                .items_center()
                .h_full()
                .min_w_0()
                .px(space::S)
                .gap(space::S)
                .map(|strip| match split {
                    Some(width) => strip
                        .flex_none()
                        .w(width)
                        .border_l_1()
                        .border_color(p.separator),
                    None => strip.flex_1(),
                })
                .child(document_tab_list(tabs, window, cx))
                .child(
                    toolbar_button("new-tab", "New tab · ⌘T", "hi.plus", cx)
                        .on_click(cx.listener(|this, _, _, cx| this.open_launcher(cx))),
                )
                .child(spacer())
                .child(self.expand_button(split, cx))
                .child(self.pane_toggle_button(cx))
                .into_any_element(),
        )
    }

    /// The full view with the chat's own tab selected: the strip stays and the chat fills
    /// the page like any other tab.
    pub(in crate::ui) fn chat_tab_selected(&self) -> bool {
        !self.split_content
            && self.visible_content().next().is_some()
            && matches!(
                self.page_tabs.get(self.active_page),
                Some(PageTab::Chat(_) | PageTab::Draft)
            )
    }

    /// Between the chat beside its documents and the full view, where the chat is a tab.
    /// Leaving the full view from the chat's tab brings the last document back beside it.
    pub(in crate::ui) fn toggle_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.split_content = !self.split_content;
        if self.split_content && self.active_content().is_none() {
            self.toggle_pane(window, cx);
        } else if self.split_content {
            self.close_session_card();
        }
        cx.notify();
    }

    /// The full view's first tab: the page the documents belong to, never closable.
    fn primary_tab(
        &self,
        active: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<DocumentTab> {
        let (index, page) = self
            .page_tabs
            .iter()
            .enumerate()
            .find(|(_, page)| !matches!(page, PageTab::Content(_)))?;
        let title = match self.page_title(page) {
            title if title.is_empty() => "New chat".into(),
            title => title,
        };
        // The chat keeps the Chats rail glyph; its sidebar status (running, unread, waiting)
        // trails the title.
        let status = match page {
            PageTab::Chat(id) => self.chat_status(id),
            _ => None,
        };
        Some(
            DocumentTab::new("primary", "hi.note01", title, active, window, cx)
                .when_some(status, DocumentTab::trailing)
                .page(index)
                // Selecting it hides the document the way the pane toggle does, so the
                // toggle and collapsing the full view bring that document back.
                .on_select(cx.listener(|this, _, window, cx| {
                    if this.active_content().is_some() {
                        this.toggle_pane(window, cx);
                    }
                })),
        )
    }

    fn expand_button(&self, split: Option<Pixels>, cx: &Context<Self>) -> AnyElement {
        let button = if split.is_some() {
            toolbar_button("expand-document", "Expand document", "hi.expand", cx)
        } else if self.split_content {
            toolbar_button(
                "expand-document",
                "Widen the window to show the chat beside",
                "hi.collapse",
                cx,
            )
            .disabled(true)
        } else {
            toolbar_button("expand-document", "Show chat beside", "hi.collapse", cx)
        };
        button
            .on_click(cx.listener(|this, _, window, cx| this.toggle_split(window, cx)))
            .into_any_element()
    }

    /// Shows or hides the pane; with no documents open it opens the new-tab launcher.
    pub(in crate::ui) fn pane_toggle_button(&self, cx: &Context<Self>) -> AnyElement {
        let shown = self.active_content().is_some();
        let label = if shown {
            "Hide side panel · ⌥⌘B"
        } else {
            "Show side panel · ⌥⌘B"
        };
        toolbar_button("pane-toggle", label, "hi.menu", cx)
            .toggle(shown, cx)
            .on_click(cx.listener(|this, _, window, cx| this.toggle_pane(window, cx)))
            .into_any_element()
    }

    /// The chat header badge counting open documents while the pane is hidden.
    pub(in crate::ui) fn open_tabs_badge(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.visible_content().next().is_none() || self.active_content().is_some() {
            return None;
        }
        let p = self.palette;
        let view = cx.entity().downgrade();
        let tabs: Vec<_> = self
            .visible_content()
            .map(|tab| {
                (
                    Arc::clone(&tab.id),
                    SharedString::clone(&tab.title),
                    tab.glyph(),
                )
            })
            .collect();
        Some(
            Button::new("open-tabs")
                .ghost()
                .size(height::MINI)
                .p_0()
                .pill_corners()
                .border_1()
                .border_color(p.separator)
                .tooltip("Open tabs")
                .accessibility_label("Open tabs")
                .child(
                    div()
                        .typography(Type::Label)
                        .child(tabs.len().to_string()),
                )
                .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, cx| {
                    let show = WeakEntity::clone(&view);
                    menu = menu
                        .item(PopupMenuItem::new("Show side panel · ⌥⌘B").on_click(
                            move |_, window, cx| {
                                if let Err(error) =
                                    show.update(cx, |this, cx| this.toggle_pane(window, cx))
                                {
                                    tracing::debug!(%error, "the workspace closed before the side panel showed");
                                }
                            },
                        ))
                        .separator()
                        .label("Open tabs");
                    for (id, title, glyph) in &tabs {
                        let id = Arc::clone(id);
                        let run = move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                            this.open_page(PageTab::Content(Arc::clone(&id)), window, cx);
                        };
                        let title = SharedString::clone(title);
                        menu = menu.item(workspace_item(title, glyph, &view, run, &palette(cx)));
                    }
                    menu
                })
                .into_any_element(),
        )
    }

    /// Hides the pane back to its page, or shows the last document again.
    pub(in crate::ui) fn toggle_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_content().is_some() {
            self.content
                .navigation
                .remember(&self.page_tabs, self.active_page);
            if let Some(index) = self
                .page_tabs
                .iter()
                .position(|page| !matches!(page, PageTab::Content(_)))
            {
                self.select_page(index, window, cx);
            }
            return;
        }
        let id = self.content.navigation.last(&TabOwner::of(&self.page_tabs));
        match id.map(Arc::clone) {
            Some(id) => self.open_page(PageTab::Content(id), window, cx),
            None => self.open_launcher(cx),
        }
    }

    /// Opens (or re-selects) the tab for one chat detail; ids are `{session}:{key}`.
    /// Opens `detail` as a pane tab, reusing the tab already showing it.
    pub(in crate::ui) fn open_session_tab(
        &mut self,
        detail: chat::ChatDetail,
        title: impl Into<SharedString>,
        glyph: &'static str,
        cx: &mut Context<Self>,
    ) {
        self.open_content(
            ContentTab {
                id: format!("{:?}:detail:{detail:?}", TabOwner::of(&self.page_tabs)).into(),
                title: title.into(),
                content: TabContent::Detail { detail, glyph },
            },
            cx,
        );
    }
}

/// Keeps the focused tab visible and moves selection with the tab-list keys.
fn document_tab_list(
    tabs: Vec<DocumentTab>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Div {
    let reveal = tabs
        .iter()
        .position(|tab| tab.focus.contains_focused(window, cx))
        .or_else(|| tabs.iter().position(|tab| tab.active));
    let scroll = window.use_keyed_state("document-tabs-scroll", cx, |_, _| {
        (ScrollHandle::new(), None::<SharedString>)
    });
    let scroll = scroll.update(cx, |(scroll, previous), _| {
        if let Some((position, tab)) =
            reveal.and_then(|position| tabs.get(position).map(|tab| (position, tab)))
        {
            let key = &tab.key;
            if previous.as_ref() != Some(key) {
                scroll.scroll_to_item(position);
                *previous = Some(SharedString::clone(key));
            }
        }
        ScrollHandle::clone(scroll)
    });
    let destinations: Rc<Vec<_>> = Rc::new(
        tabs.iter()
            .map(|tab| (tab.page, FocusHandle::clone(&tab.focus)))
            .collect(),
    );
    let tabs = tabs.into_iter().enumerate().map(|(position, tab)| {
        let destinations = Rc::clone(&destinations);
        tab.on_navigate(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            let Some(target) = tab_destination(position, destinations.len(), &event.keystroke.key)
            else {
                return;
            };
            if event.keystroke.modifiers.modified() {
                return;
            }
            let Some((page, focus)) = destinations.get(target) else {
                return;
            };
            focus.focus(window, cx);
            this.select_page(*page, window, cx);
            window.prevent_default();
            cx.stop_propagation();
        }))
    });
    div()
        .relative()
        .flex()
        .h_full()
        .min_w_0()
        .child(
            div()
                .id("document-tabs")
                .role(Role::TabList)
                .aria_label("Documents")
                .flex()
                .items_center()
                .size_full()
                .gap(space::XS)
                .overflow_x_scroll()
                .track_scroll(&scroll)
                .children(tabs),
        )
        .child(ScrollableMask::vertical_wheel(&scroll))
}

/// One floating pill: a leading glyph that identifies the tab, a Label title, an optional
/// trailing status and a close mark shown on hover or focus.
#[derive(IntoElement)]
struct DocumentTab {
    key: SharedString,
    glyph: &'static str,
    title: SharedString,
    /// After the title: the chat tab's activity status.
    trailing: Option<AnyElement>,
    active: bool,
    page: usize,
    focus: FocusHandle,
    status: Option<AnyElement>,
    on_select: Option<Handler<ClickEvent>>,
    on_close: Option<Handler<ClickEvent>>,
    on_navigate: Option<Handler<KeyDownEvent>>,
    /// The right-click menu for the tab's file or page: Add to message, Download.
    menu: Option<Rc<TabMenu>>,
}

type TabMenu = dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu;

impl DocumentTab {
    fn new(
        key: &str,
        glyph: &'static str,
        title: SharedString,
        active: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let key = SharedString::from(format!("doc:{key}"));
        let focus = FocusHandle::clone(
            window
                .use_keyed_state(SharedString::clone(&key), cx, |_, cx| cx.focus_handle())
                .read(cx),
        );
        Self {
            key,
            glyph,
            title: if title.is_empty() {
                "Untitled".into()
            } else {
                title
            },
            trailing: None,
            active,
            page: 0,
            focus,
            status: None,
            on_select: None,
            on_close: None,
            on_navigate: None,
            menu: None,
        }
    }

    fn page(mut self, page: usize) -> Self {
        self.page = page;
        self
    }

    fn trailing(mut self, trailing: AnyElement) -> Self {
        self.trailing = Some(trailing);
        self
    }

    fn status(mut self, status: AnyElement) -> Self {
        self.status = Some(status);
        self
    }

    fn on_navigate(
        mut self,
        handler: impl Fn(&KeyDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_navigate = Some(Rc::new(handler));
        self
    }

    fn on_select(mut self, handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_select = Some(std::rc::Rc::new(handler));
        self
    }

    fn on_close(mut self, handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_close = Some(std::rc::Rc::new(handler));
        self
    }

    fn menu(
        mut self,
        menu: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> Self {
        self.menu = Some(Rc::new(menu));
        self
    }
}

impl RenderOnce for DocumentTab {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        let group = SharedString::clone(&self.key);
        let active = self.active;
        let focused = self.focus.contains_focused(window, cx);
        let menu = self.menu;
        let focus = FocusHandle::clone(&self.focus);
        let title = self.title;
        let tab = div()
            .id(SharedString::clone(&self.key))
            .group(SharedString::clone(&group))
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(space::S)
            .h(height::REGULAR)
            .min_w(TAB_MIN)
            .max_w(TAB_MAX)
            .pl(space::S)
            .pr(space::XS)
            .control_corners()
            .border_1()
            .border_color(transparent_black())
            .role(Role::Tab)
            .aria_label(SharedString::clone(&title))
            .aria_selected(active)
            .track_focus(&self.focus)
            .tab_stop(true)
            .cursor_default()
            .focus_visible(|style| style.border_color(p.accent))
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                focus.focus(window, cx);
                window.prevent_default();
            })
            .text_color(if active { p.text } else { p.muted })
            .map(|tab| {
                if active {
                    tab.bg(p.card).border_color(p.separator.opacity(0.5))
                } else {
                    tab.hover(|style| style.bg(p.hover).text_color(p.text))
                }
            })
            .tooltip(text_tooltip(SharedString::from(&title)))
            .when_some(self.on_select, |tab, handler| {
                tab.on_click(move |event, window, cx| handler(event, window, cx))
            })
            .when_some(self.on_navigate, |tab, handler| {
                tab.on_key_down(move |event, window, cx| handler(event, window, cx))
            })
            .children(self.status)
            .child(icon(self.glyph, &p).size(glyph::INLINE).flex_shrink_0())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .typography(Type::Label)
                    .child(title),
            )
            .children(self.trailing)
            .when_some(self.on_close, |tab, handler| {
                tab.child(
                    div()
                        .flex_shrink_0()
                        .when(!focused, |close| {
                            close.invisible().group_hover(group, gpui::Styled::visible)
                        })
                        .child(mini_button("close", "Close tab", "hi.x", cx).on_click(
                            move |event, window, cx| {
                                cx.stop_propagation();
                                handler(event, window, cx);
                            },
                        )),
                )
            });
        match menu {
            Some(build) => tab
                .context_menu(move |menu, window, cx| build(menu, window, cx))
                .into_any_element(),
            None => tab.into_any_element(),
        }
    }
}

fn tab_destination(position: usize, count: usize, key: &str) -> Option<usize> {
    if count == 0 {
        return None;
    }
    match key {
        "left" => Some((position + count - 1) % count),
        "right" => Some((position + 1) % count),
        "home" => Some(0),
        "end" => Some(count - 1),
        _ => None,
    }
}
