//! The new-tab launcher: tools, this chat's transcripts and its files.

use super::{
    AnyElement, ContentTab, Context, Div, ElementId, FluentBuilder, InteractiveElement,
    IntoElement, Palette, ParentElement, Shape, SharedString, Stateful, StatefulInteractiveElement,
    Styled, TabContent, Type, Typography, Workspace, desktop, div, document, glyph, heading,
    height, icon, preview_glyph, space, text_tooltip,
};
use mobius_gateway::wire::GitDiffScope;
use std::rc::Rc;

impl Workspace {
    /// A new tab: the tools and this chat's transcripts, to open one in its place.
    pub(in crate::ui) fn open_launcher(&mut self, cx: &mut Context<Self>) {
        self.load_chat_sources();
        self.open_content(
            ContentTab {
                id: format!("launcher:{}", crate::gateway::request_id()).into(),
                title: "New tab".into(),
                content: TabContent::Launcher,
            },
            cx,
        );
    }

    pub(super) fn launcher_document(&self, cx: &Context<Self>) -> AnyElement {
        document(None, self.launcher(cx), cx)
    }

    /// The New tab body (Codex img 7): tools, then this chat's transcripts.
    fn launcher(&self, cx: &Context<Self>) -> AnyElement {
        let files = self.chat_files_section(cx);
        let tools = self.launcher_tools(cx);
        let session = self.client.state.selected_session_id.as_ref();
        let transcripts = self.client.state.previews.values().filter_map(|preview| {
            let (session, preview) = (session?.share(), Rc::clone(preview));
            let glyph = preview_glyph(preview.source.symbol.as_ref());
            Some(
                launcher_card(
                    SharedString::from(format!("launch-preview:{}", preview.source.id)),
                    glyph,
                    &preview.source.title,
                    true,
                    &self.palette,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_preview_tab(&session, Rc::clone(&preview), cx);
                })),
            )
        });
        let transcripts: Vec<_> = transcripts.collect();
        div()
            .id("launcher")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(space::L)
            .flex()
            .flex_col()
            .gap(space::S)
            .child(heading("Tools"))
            .child(div().grid().grid_cols(2).gap(space::S).children(tools))
            .when(!transcripts.is_empty(), |body| {
                body.child(div().pt(space::L).child(heading("Transcripts")))
                    .child(
                        div()
                            .grid()
                            .grid_cols(2)
                            .gap(space::S)
                            .children(transcripts),
                    )
            })
            .children(files)
            .into_any_element()
    }

    fn launcher_tools(&self, cx: &Context<Self>) -> Vec<Stateful<Div>> {
        let workspace =
            self.active_session_ready() && self.client.state.workspace_session_id().is_some();
        let terminal = self.terminal_availability().err();
        let computer = self.computer_title();
        [
            ("launch-files", "hi.folder", "Files", workspace, 0),
            ("launch-changes", "hi.plusMinus02", "Changes", workspace, 1),
            (
                "launch-terminal",
                "hi.terminalWindow",
                "Terminal",
                terminal.is_none(),
                2,
            ),
            ("launch-open", "hi.folderOpen", "Open local file…", true, 3),
            (
                "launch-computer",
                desktop::GLYPH,
                computer.as_str(),
                self.computer_available(),
                6,
            ),
        ]
        .into_iter()
        .map(|(id, glyph, label, enabled, action)| {
            // Why a disabled tool is unavailable, shown on hover.
            let unavailable = match action {
                0 | 1 => Some("This conversation has no project workspace"),
                2 => terminal,
                _ => None,
            };
            launcher_card(id, glyph, label, enabled, &self.palette)
                .when_some(unavailable.filter(|_| !enabled), |card, reason| {
                    card.tooltip(text_tooltip(reason.into()))
                })
                .on_click(cx.listener(move |this, _, window, cx| match action {
                    0 if enabled => this.open_workspace_files(cx),
                    1 if enabled => this.open_git_diff(GitDiffScope::Unstaged, cx),
                    2 if enabled => this.open_terminal(window, cx),
                    6 if enabled => this.open_computer(window, cx),
                    0 | 1 | 2 | 6 => {}
                    _ => this.choose_file(window, cx),
                }))
        })
        .collect()
    }

    /// What was sent to the Bot and what it sent back: a click opens one, a right click
    /// adds it to the message.
    fn chat_files_section(&self, cx: &Context<Self>) -> Option<Div> {
        let session = self.client.state.selected_session_id.as_ref()?;
        let chat_files = self.chat_files();
        let files = self.file_buttons(
            chat_files.into_iter().map(|(file, _)| file),
            Some(session),
            cx,
        );
        (!files.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap(space::S)
                .pt(space::L)
                .child(heading("Chat files"))
                .child(div().flex().flex_wrap().gap(space::S).children(files))
        })
    }
}

fn launcher_card(
    id: impl Into<ElementId>,
    glyph: &'static str,
    label: &str,
    enabled: bool,
    p: &Palette,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(space::S)
        .h(height::TOUCH)
        .px(space::S)
        .control_corners()
        .bg(p.control)
        .border_1()
        .border_color(p.separator)
        .typography(Type::Body)
        .map(|card| {
            if enabled {
                card.cursor_pointer()
                    .hover(|style| style.bg(p.control.blend(p.hover)))
            } else {
                card.opacity(0.5)
            }
        })
        .child(icon(glyph, p).size(glyph::INLINE))
        .child(div().min_w_0().truncate().child(label.to_owned()))
}
