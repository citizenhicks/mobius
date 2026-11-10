use super::*;

/// The sidebar's and side pane's reveals, made on the first frame so whatever shows at
/// launch appears without sliding; any later way of opening or closing them animates.
#[derive(Default)]
pub(super) struct PanelMotion {
    sidebar: Option<motion::Reveal>,
    pub(in crate::ui) pane: Option<motion::Reveal>,
    /// The tab the pane last showed and its width, so a hidden pane slides out with it.
    pub(in crate::ui) pane_shown: Option<(String, Pixels)>,
    /// Whether the pane slid when this frame began; native views paint later in the frame
    /// and must agree with the layout, which was built from the same moment.
    pane_moving: bool,
}

impl Workspace {
    pub(super) fn primary_page(&self) -> Option<&PageTab> {
        self.page_tabs
            .iter()
            .find(|tab| !matches!(tab, PageTab::Content(_)))
    }

    pub(super) fn draft_selected(&self) -> bool {
        matches!(self.primary_page(), Some(PageTab::Draft))
            && (matches!(self.page_tabs.get(self.active_page), Some(PageTab::Draft))
                || (self.active_content().is_some() && self.split_content))
    }

    pub(super) fn sidebar_width(&self, cx: &App) -> Pixels {
        let full = self.full_sidebar_width(cx);
        match &self.motion.sidebar {
            Some(reveal) => reveal
                .sliding(full, motion::now())
                .unwrap_or_else(|| self.settled_sidebar_width(cx)),
            None => self.settled_sidebar_width(cx),
        }
    }

    fn settled_sidebar_width(&self, cx: &App) -> Pixels {
        if self.navigation_visible {
            self.full_sidebar_width(cx)
        } else {
            px(0.)
        }
    }

    fn full_sidebar_width(&self, cx: &App) -> Pixels {
        self.sidebar_resize
            .read(cx)
            .sizes()
            .first()
            .copied()
            .unwrap_or(self.shell.sidebar_width)
            .clamp(width::SIDEBAR_MIN, width::SIDEBAR_MAX)
    }

    /// Heads the sidebar's and side pane's reveals toward what shows and keeps frames
    /// coming while one moves. Reduce Motion keeps them instant.
    pub(super) fn track_panel_motion(&mut self, window: &Window, cx: &mut Context<Self>) {
        let still = self.native.reduce_motion();
        #[cfg(feature = "snapshot")]
        let still = still || snapshot::reduce_motion();
        cx.set_reduce_motion(still);
        let now = motion::now();
        let split = self.pane_split(window, cx);
        if let Some(width) = split
            && let Some(id) = self.active_content().map(|tab| &*tab.id)
        {
            let same = matches!(&self.motion.pane_shown, Some((shown, _)) if shown == id);
            let fresh = (!same).then(|| id.to_owned());
            match (fresh, &mut self.motion.pane_shown) {
                (Some(id), shown) => *shown = Some((id, width)),
                (None, Some((_, shown_width))) => *shown_width = width,
                (None, None) => {}
            }
        }
        let pane = split.is_some();
        let visible = self.navigation_visible;
        let panels = &mut self.motion;
        for (reveal, open) in [(&mut panels.sidebar, visible), (&mut panels.pane, pane)] {
            reveal
                .get_or_insert_with(|| motion::Reveal::new(open))
                .set(open, still, now);
        }
        panels.pane_moving = panels
            .pane
            .as_ref()
            .is_some_and(|reveal| reveal.moving(now));
        if [&panels.sidebar, &panels.pane]
            .into_iter()
            .flatten()
            .any(|reveal| reveal.moving(now))
        {
            window.request_animation_frame();
        }
    }

    /// The side pane's width while it slides, given its `full` width.
    pub(super) fn pane_sliding(&self, full: Pixels) -> Option<Pixels> {
        self.motion.pane.as_ref()?.sliding(full, motion::now())
    }

    /// Native views ignore GPUI clipping, so they wait out the pane's slide.
    pub(super) fn pane_moving(&self) -> bool {
        self.motion.pane_moving
    }

    /// While a hidden pane slides out: the tab it showed, its full and shown widths.
    pub(super) fn pane_closing(&self) -> Option<(&str, Pixels, Pixels)> {
        let (id, full) = self.motion.pane_shown.as_ref()?;
        Some((id, *full, self.pane_sliding(*full)?))
    }

    pub(super) fn workspace_body(&self, body: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let content = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(body);
        let full = self.full_sidebar_width(cx);
        let sliding = self
            .motion
            .sidebar
            .as_ref()
            .and_then(|reveal| reveal.sliding(full, motion::now()))
            .map(|shown| (shown, full));
        let main = if let Some((shown, full)) = sliding {
            // While it slides, the sidebar keeps its full width and is revealed by a
            // clipping frame, so its rows never reflow.
            div()
                .flex()
                .size_full()
                .child(
                    div()
                        .relative()
                        .w(shown)
                        .h_full()
                        .flex_shrink_0()
                        .overflow_hidden()
                        .child(
                            div()
                                .w(full)
                                .min_w(full)
                                .h_full()
                                .flex_shrink_0()
                                .child(self.sidebar_card(cx)),
                        )
                        .map(|view| {
                            #[cfg(feature = "snapshot")]
                            let view = view.child(snapshot::track("motion:sidebar"));
                            view
                        }),
                )
                .child(content)
                .into_any_element()
        } else if self.navigation_visible {
            gpui_component::h_resizable("workspace-panes")
                .with_state(&self.sidebar_resize)
                // The floating card is the sidebar's edge; the handle keeps only its drag.
                .with_handle_appearance(std::rc::Rc::new(|_, _, _| Some(div().into_any_element())))
                .child(
                    gpui_component::resizable_panel()
                        .size(self.shell.sidebar_width)
                        .size_range(width::SIDEBAR_MIN..width::SIDEBAR_MAX)
                        .flex_none()
                        .fixed()
                        .child(
                            div()
                                .relative()
                                .size_full()
                                .child(self.sidebar_card(cx))
                                .map(|view| {
                                    #[cfg(feature = "snapshot")]
                                    let view = view.child(snapshot::track("motion:sidebar"));
                                    view
                                }),
                        ),
                )
                .child(
                    gpui_component::resizable_panel()
                        .size_range(width::CHAT_MIN..Pixels::MAX)
                        .child(content),
                )
                .into_any_element()
        } else {
            content.into_any_element()
        };
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(self.navigation_rail(cx))
            // The rail and title bar are one L-shaped frame holding the page's sheet; the
            // sidebar floats over the sheet's edge while the page's content stays beside it.
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .bg(p.sheet)
                    .top_left_corner(radius::TILE)
                    .border_t_1()
                    .border_l_1()
                    .border_color(p.separator)
                    .overflow_hidden()
                    .children(backdrop::scanlines(cx))
                    .child(main),
            )
            .into_any_element()
    }

    /// The sidebar as a card floating on the sheet, a gap clear of every edge.
    fn sidebar_card(&self, cx: &mut Context<Self>) -> Div {
        let p = self.palette;
        // The frost is the card's first child, so the sidebar paints over it.
        let card = div()
            .relative()
            .size_full()
            .border_1()
            .border_color(p.separator)
            .shadow_sm()
            .overflow_hidden();
        div()
            .size_full()
            .p(space::S)
            .child(backdrop::floating(card, p.float, cx).child(self.navigation_context(cx)))
    }

    pub(super) fn close_page(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.page_tabs.get(index) else {
            return;
        };
        if !matches!(tab, PageTab::Content(_)) {
            self.new_chat(window, cx);
            return;
        }
        if let PageTab::Content(id) = self.page_tabs.remove(index) {
            self.content
                .navigation
                .detach(&panel::TabOwner::of(&self.page_tabs), &id);
            if self.content.navigation.owner(&id).is_none() {
                self.forget_content(&id, cx);
            }
        }
        self.prune_thumbnails();
        if self.page_tabs.is_empty() {
            self.new_chat(window, cx);
            return;
        }
        let selected = selected_after_close(self.active_page, index, self.page_tabs.len());
        if self.active_page == index {
            self.select_page(selected, window, cx);
        } else {
            self.active_page = selected;
            self.content
                .navigation
                .remember(&self.page_tabs, self.active_page);
            cx.notify();
        }
    }

    pub(super) fn forget_content(&mut self, id: &str, cx: &mut Context<Self>) {
        self.content.tabs.remove(id);
        self.content.navigation.remove(id);
        self.forget_agent_browser(id, cx);
        self.forget_browser(id);
        self.forget_desktop(id);
        self.native.close_panel(id);
        self.transcript_scrolls.remove(id);
    }

    pub(in crate::ui) fn forget_chat_tabs(&mut self, session: &SessionId, cx: &mut Context<Self>) {
        self.content
            .navigation
            .remember(&self.page_tabs, self.active_page);
        let visible =
            panel::TabOwner::of(&self.page_tabs) == panel::TabOwner::Chat(session.share());
        for id in self.content.navigation.remove_chat(session) {
            if visible {
                self.page_tabs
                    .retain(|page| !matches!(page, PageTab::Content(tab) if *tab == id));
            }
            if self.content.navigation.owner(&id).is_none() {
                self.forget_content(&id, cx);
            }
        }
        self.active_page = self.active_page.min(self.page_tabs.len().saturating_sub(1));
    }
}
