mod chat_menu;
mod conversation;
pub(in crate::ui) use chat_menu::{MenuScope, session_menu};

use super::*;
use gpui_component::Disableable;
use gpui_component::menu::{DropdownMenu, PopupMenuItem};

impl Workspace {
    pub(super) fn chrome(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // A closed sidebar leaves room for New chat beside its toggle.
        let closed = !self.navigation_visible;
        let width =
            (width::RAIL + self.sidebar_width(cx)).max(px(if closed { 234. } else { 200. }));
        let split = self
            .pane_split(window, cx)
            .map(|full| self.pane_sliding(full).unwrap_or(full));
        let has_content = self.active_content().is_some() || self.chat_tab_selected();
        let sidebar_toggle = toolbar_button("toggle-menu", "Toggle sidebar · ⌃⌘S", "hi.menu", cx)
            .on_click(cx.listener(|this, _, _, cx| this.toggle_navigation(cx)));
        #[cfg(feature = "snapshot")]
        let sidebar_toggle =
            sidebar_toggle.child(crate::ui::snapshot::probe("motion:sidebar-toggle".into()));
        title_bar()
            .child(
                div()
                    .w(width)
                    .h_full()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .pl(width::TRAFFIC_LIGHTS)
                    .gap(space::XS)
                    .child(
                        toolbar_button("back", "Back · ⌘[", back_icon(cx), cx)
                            .disabled(!self.history.can_move(false))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.navigate_history(false, window, cx);
                            })),
                    )
                    .child(
                        toolbar_button("forward", "Forward · ⌘]", forward_icon(cx), cx)
                            .disabled(!self.history.can_move(true))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.navigate_history(true, window, cx);
                            })),
                    )
                    .child(sidebar_toggle)
                    .when(closed, |zone| {
                        zone.child(appear(
                            "titlebar-new-chat-appear",
                            toolbar_button(
                                "titlebar-new-chat",
                                "New chat · ⌘N",
                                "hi.notePencil",
                                cx,
                            )
                            .on_click(cx.listener(|this, _, window, cx| this.new_chat(window, cx))),
                            cx,
                        ))
                    }),
            )
            .when(!has_content || split.is_some(), |bar| {
                bar.child(self.page_header(window, cx))
            })
            .children(self.document_strip(split, window, cx))
            .into_any_element()
    }

    /// SwiftUI's chat subtitle: "{workspace} · {machine}", then the attached folder count.
    fn chat_subtitle(&self) -> Option<String> {
        let session = self
            .client
            .state
            .session
            .as_ref()
            .filter(|_| self.active_session_ready())?;
        let path = session.workspace.as_ref()?.path.display().to_string();
        let parts = [
            crate::ui::navigation::workspace_name(&path).to_owned(),
            self.gateway_name(),
        ];
        let mut subtitle = parts
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        if !session.attached_folders.is_empty() {
            subtitle = format!("{subtitle} · Folders: {}", session.attached_folders.len());
        }
        Some(subtitle)
    }

    /// The page's titlebar zone: the chat title with its subtitle and the chat menu, or
    /// the page's own actions, then the side-pane toggle.
    fn page_header(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let primary = self.primary_page();
        let actions = primary
            .map(|page| self.page_actions(page, cx))
            .unwrap_or_default();
        let conversation = self.active_conversation_bot().is_some();
        let title = primary
            .filter(|_| !conversation)
            .filter(|tab| !matches!(tab, PageTab::Draft))
            .map(|tab| self.page_title(tab))
            .unwrap_or_default();
        let chat = matches!(primary, Some(PageTab::Chat(_) | PageTab::Draft));
        let chat_title = chat && !conversation;
        // Running, finished unread, or waiting on an approval or question, as in the sidebar.
        let status = match primary {
            Some(PageTab::Chat(id)) if !conversation => self.chat_status(id),
            _ => None,
        };
        let subtitle = self.chat_subtitle().filter(|_| chat);
        let tooltip = self
            .client
            .state
            .session
            .as_ref()
            .filter(|_| subtitle.is_some())
            .and_then(|session| session.workspace.as_ref())
            .map(|workspace| {
                let path = workspace.path.display();
                format!("{path} on {}", self.gateway_name())
            });
        div()
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .h_full()
            .pr(space::S)
            .gap(space::XS)
            // A chat's rule continues the floating sidebar card's edge, a gap inside the zone.
            .when(chat_title, |bar| {
                bar.child(
                    divider(&self.palette)
                        .when(self.navigation_visible, |rule| rule.ml(-space::S))
                        .mr(space::XS),
                )
            })
            .when(!title.is_empty() && !chat_title, |bar| bar.pl(space::S))
            .children(status)
            .when(!conversation, |bar| {
                bar.child(title_stack(
                    "page-title",
                    SharedString::clone(&title),
                    subtitle,
                    tooltip,
                ))
            })
            .when(conversation, |bar| bar.child(div().flex_1()))
            .when(chat, |bar| {
                bar.child(self.chat_menu(window, cx))
                    .children(self.session_card_button(cx))
            })
            .when(!actions.is_empty(), |bar| {
                // Fades in again whenever the page, and so its actions, change.
                let key = SharedString::from(format!("page-actions:{title}"));
                bar.child(appear(
                    key,
                    div().flex().items_center().gap(space::XS).children(actions),
                    cx,
                ))
            })
            .when(
                (chat || matches!(primary, Some(PageTab::Draft)))
                    && self.active_content().is_none(),
                |bar| {
                    bar.when(chat, |bar| bar.child(divider(&self.palette).mx(space::XS)))
                        .child(appear(
                            "pane-toggle-appear",
                            div()
                                .flex()
                                .items_center()
                                .gap(space::XS)
                                .children(self.open_tabs_badge(cx))
                                .child(self.pane_toggle_button(cx)),
                            cx,
                        ))
                },
            )
            .into_any_element()
    }

    pub(super) fn overlay_changed(&mut self, open: bool, cx: &mut Context<Self>) {
        if open {
            self.expanded.insert("native-overlay".into());
            self.native.hide_panel();
        } else {
            self.expanded.remove("native-overlay");
        }
        cx.notify();
    }
}

/// A title with an optional muted subtitle underneath (SwiftUI navigationTitle + navigationSubtitle):
/// BodyMedium over Caption, 36 high, so it centres in the 44 titlebar like a single line.
#[derive(IntoElement)]
pub(in crate::ui) struct TitleStack {
    id: ElementId,
    title: SharedString,
    subtitle: Option<SharedString>,
    tooltip: Option<SharedString>,
}

pub(in crate::ui) fn title_stack(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    subtitle: Option<impl Into<SharedString>>,
    tooltip: Option<impl Into<SharedString>>,
) -> TitleStack {
    TitleStack {
        id: id.into(),
        title: title.into(),
        subtitle: subtitle.map(Into::into),
        tooltip: tooltip.map(Into::into),
    }
}

impl RenderOnce for TitleStack {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let muted = cx.global::<Palette>().muted;
        div()
            .id(self.id)
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .justify_center()
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .typography(Type::BodyMedium)
                    .child(self.title),
            )
            .children(self.subtitle.map(|subtitle| {
                div()
                    .min_w_0()
                    .truncate()
                    .typography(Type::Caption)
                    .text_color(muted)
                    .child(subtitle)
            }))
            .when_some(self.tooltip, |stack, tooltip| {
                stack.tooltip(text_tooltip(tooltip))
            })
    }
}

/// A titlebar control fading in as it appears, so controls never pop in; instant under
/// Reduce Motion. `id` restarts the fade whenever it changes.
fn appear(id: impl Into<ElementId>, child: impl IntoElement, cx: &App) -> AnyElement {
    let wrapper = div().flex().items_center().gap(space::XS).child(child);
    motion::fade_in(wrapper, id, motion::APPEAR, cx)
}

/// A short vertical rule between titlebar groups.
fn divider(p: &Palette) -> Div {
    div()
        .w(inset::HAIRLINE)
        .h(px(16.))
        .flex_shrink_0()
        .bg(p.separator)
}

/// The titlebar band: it drags the window and zooms on a double click, except where a
/// button took the press (buttons prevent the default on mouse down).
fn title_bar() -> Div {
    div()
        .flex()
        .items_center()
        .h(height::TITLEBAR)
        .flex_shrink_0()
        .window_control_area(WindowControlArea::Drag)
        .on_mouse_down(MouseButton::Left, |event, window, _| {
            if window.default_prevented() {
                return;
            }
            if event.click_count == 2 {
                window.titlebar_double_click();
            } else {
                window.start_window_move();
            }
        })
}
