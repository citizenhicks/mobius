//! Git diff documents and transcript patches, drawn by gpui-component's `Diff` with the
//! app's own file headers.

use super::*;
use crate::gateway::PreviewRequest;
use crate::numeric::ToFloat;
use crate::ui::diff_document::DiffDocument;
use gpui_component::ActiveTheme as _;
use gpui_component::diff::{Diff, DiffFile, DiffHunkSeparator, DiffState};
use gpui_component::menu::ContextMenuExt;
use mobius::SessionId;
use mobius_gateway::wire::GitDiffScope;
use std::cell::Cell;
use std::rc::Rc;

#[cfg(all(test, feature = "snapshot"))]
#[path = "diff_tests.rs"]
mod interaction_tests;

/// The scope of a turn's own changes, which the transcript's turn card opens.
const TURN: &str = "This turn";
const SCOPES: [(&str, GitDiffScope); 3] = [
    ("Unstaged", GitDiffScope::Unstaged),
    ("Staged", GitDiffScope::Staged),
    ("Last commit", GitDiffScope::Committed),
];
/// Files start expanded unless the diff is this long.
const EXPANDED_ROWS: usize = 2_000;
/// A transcript patch scrolls inside this height.
const INLINE_HEIGHT: Pixels = px(360.);

#[derive(Clone)]
pub(in crate::ui) struct DiffDoc {
    session_id: SessionId,
    /// The Git scope shown, or `None` for a turn's changes.
    scope: Option<GitDiffScope>,
    empty_source: bool,
    truncated: bool,
    totals: (usize, usize),
    state: Entity<DiffState>,
    pub(super) explorer: Box<FilesDoc>,
    selected: Option<String>,
    /// A tree selection waiting to be revealed on the next render.
    reveal: Rc<Cell<bool>>,
}

impl DiffDoc {
    pub fn new<T: 'static>(
        session_id: SessionId,
        scope: GitDiffScope,
        text: &str,
        cx: &mut Context<T>,
    ) -> Self {
        Self::with_document(session_id, Some(scope), text, DiffDocument::parse(text), cx)
    }

    /// A turn's changes, with paths inside the chat's folder `root` relative to it.
    fn turn(session_id: SessionId, text: &str, root: &str, cx: &mut Context<Workspace>) -> Self {
        let document = DiffDocument::parse_in(text, root);
        Self::with_document(session_id, None, text, document, cx)
    }

    fn with_document<T: 'static>(
        session_id: SessionId,
        scope: Option<GitDiffScope>,
        text: &str,
        document: DiffDocument,
        cx: &mut Context<T>,
    ) -> Self {
        let files = document
            .files
            .iter()
            .map(|file| mobius_gateway::wire::WorkspaceFileRecord {
                path: file.path().to_string(),
                size: 0,
            })
            .collect();
        let explorer = FilesDoc::new(None, files, document.truncated, cx);
        let totals = (document.added(), document.removed());
        let collapse = text.lines().count() > EXPANDED_ROWS;
        let DiffDocument { files, truncated } = document;
        let state = cx.new(|cx| {
            let mut state = DiffState::new(files, cx);
            if collapse {
                set_all_collapsed(&mut state, true, cx);
            }
            state
        });
        Self {
            session_id,
            scope,
            empty_source: text.trim().is_empty(),
            truncated,
            totals,
            state,
            explorer: Box::new(explorer),
            selected: None,
            reveal: Rc::default(),
        }
    }

    #[cfg(all(test, feature = "snapshot"))]
    pub(super) fn selected_path(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// A tree selection reveals its diff, keeping the Changes tab open.
    pub(super) fn select_path(&mut self, path: &str) {
        let path = path.trim_start_matches("./");
        self.selected = Some(path.to_owned());
        self.reveal.set(true);
        self.explorer.reveal(path);
    }

    /// Expands and scrolls to the selected file once the tree picked it.
    fn reveal_selected(&self, cx: &mut App) {
        let Some(selected) = self.selected.as_deref().filter(|_| self.reveal.take()) else {
            return;
        };
        self.state.update(cx, |state, cx| {
            // The state is borrowed mutably below, so the path is held by its own handle.
            let Some(path) = state
                .files()
                .iter()
                .map(DiffFile::path)
                .find(|path| path.trim_start_matches("./") == selected)
            else {
                return;
            };
            let path = SharedString::clone(path);
            state.set_file_collapsed(&path, false, cx);
            state.scroll_to_file(&path, cx);
        });
    }

    fn scope_label(&self) -> &'static str {
        let Some(current) = self.scope else {
            return TURN;
        };
        SCOPES
            .iter()
            .find(|(_, scope)| *scope == current)
            .map_or("Unstaged", |(label, _)| label)
    }
}

fn set_all_collapsed(state: &mut DiffState, collapsed: bool, cx: &mut Context<DiffState>) {
    for file in 0..state.files().len() {
        // `set_file_collapsed` borrows the state mutably, so each path is its own handle.
        let Some(path) = state
            .files()
            .get(file)
            .map(|file| SharedString::clone(file.path()))
        else {
            continue;
        };
        state.set_file_collapsed(&path, collapsed, cx);
    }
}

/// The app's diff surface: our palette's card, no stock border, numbered lines and hunk
/// headings.
fn diff_view(state: &Entity<DiffState>, p: &Palette) -> Diff {
    Diff::new(state)
        .line_number(true)
        .hunk_separator(DiffHunkSeparator::Metadata)
        .bg(p.card)
        .border_0()
}

impl Workspace {
    pub(in crate::ui) fn diff_document(
        &self,
        id: &str,
        doc: &DiffDoc,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        doc.reveal_selected(cx);
        let turn = self.turn_changes(&doc.session_id).is_some().then(|| {
            let session = doc.session_id.share();
            MenuChoice::new(
                TURN,
                doc.scope.is_none(),
                cx.listener(move |this, _, _, cx| this.show_turn_changes(session.share(), cx)),
            )
        });
        let git = SCOPES.iter().map(|(label, scope)| {
            let (id, session, scope) = (Arc::<str>::from(id), doc.session_id.share(), *scope);
            MenuChoice::new(
                *label,
                Some(scope) == doc.scope,
                cx.listener(move |this, _, _, cx| {
                    let request = PreviewRequest::Diff(scope);
                    this.request_preview(
                        Arc::clone(&id),
                        "Changes".into(),
                        session.share(),
                        request,
                        cx,
                    );
                }),
            )
        });
        let choices = turn.into_iter().chain(git).collect();
        let (id_refresh, session) = (Arc::<str>::from(id), doc.session_id.share());
        let has_files = !doc.state.read(cx).files().is_empty();
        let bar = toolbar()
            .child(menu_select(
                "diff-scope",
                doc.scope_label(),
                None,
                choices,
                cx,
            ))
            .child(totals(doc.totals.0, doc.totals.1, cx))
            .child(spacer())
            .children(has_files.then(|| Self::fold_all_button(doc, cx)))
            // A turn's changes are what the turn reported; only Git's can be read again.
            .children(doc.scope.map(|scope| {
                toolbar_button("refresh-diff", "Refresh", "hi.refresh03", cx).on_click(cx.listener(
                    move |this, _, _, cx| {
                        let request = PreviewRequest::Diff(scope);
                        this.request_preview(
                            Arc::clone(&id_refresh),
                            "Changes".into(),
                            session.share(),
                            request,
                            cx,
                        );
                    },
                ))
            }));
        let body = if has_files {
            let workspace = cx.weak_entity();
            let session = doc.session_id.share();
            div()
                .flex()
                .flex_col()
                .size_full()
                .child(diff_view(&doc.state, &self.palette).flex_1().render_header(
                    move |file, _, cx| {
                        workspace
                            .update(cx, |this, cx| this.diff_file_header(&session, file, cx))
                            .unwrap_or_else(|_| div().into_any_element())
                    },
                ))
                .children(
                    doc.truncated
                        .then(|| truncation_warning(TRUNCATED, &self.palette)),
                )
                .into_any_element()
        } else {
            let title = if doc.scope.is_none() {
                "No changes in this turn".into()
            } else if doc.empty_source {
                format!("No {} changes", doc.scope_label().to_lowercase())
            } else {
                "No displayable changes".into()
            };
            EmptyState::new("hi.plusMinus02", title).into_any_element()
        };
        let bar = bar.child(Self::rail_toggle(id, doc.explorer.rail_shown(), cx));
        let body = self.explorer_body(id, &doc.explorer, body, cx);
        document(bar, body, cx)
    }

    fn fold_all_button(doc: &DiffDoc, cx: &Context<Self>) -> Button {
        let state = doc.state.read(cx);
        let expand = state
            .files()
            .iter()
            .all(|file| state.is_file_collapsed(file.path()));
        let (label, glyph) = if expand {
            ("Expand all files", "hi.unfoldVertical")
        } else {
            ("Collapse all files", "hi.foldVertical")
        };
        let state = doc.state.downgrade();
        toolbar_button("fold-all-diff", label, glyph, cx).on_click(move |_, _, cx| {
            if let Err(error) = state.update(cx, |state, cx| set_all_collapsed(state, !expand, cx))
            {
                tracing::debug!(%error, "the diff closed before its files could fold");
            }
        })
    }

    /// A Changes file header: the shared label, then Open file and Add to message.
    fn diff_file_header(
        &self,
        session: &SessionId,
        file: &DiffFile,
        cx: &Context<Self>,
    ) -> AnyElement {
        let (open_session, open_path) = (session.share(), file.path().to_string());
        let open = toolbar_button("diff-open", "Open file", "hi.arrowUpRight01", cx).on_click(
            cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.open_workspace_file_tab(open_session.share(), open_path.clone(), cx); // rules-allow: every click opens a tab that owns the path
            }),
        );
        let mention = file.path().to_string();
        let add = toolbar_button("diff-mention", "Add to message", "hi.link", cx).on_click(
            cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.add_mention(chat::Mention::Path(mention.clone()), window, cx); // rules-allow: every click adds a mention that owns the path
            }),
        );
        #[cfg(feature = "snapshot")]
        let (open, add) = {
            let index = self
                .content
                .tabs
                .values()
                .find_map(|tab| match &tab.content {
                    TabContent::Diff(doc) => doc
                        .state
                        .read(cx)
                        .files()
                        .iter()
                        .position(|entry| entry.path() == file.path()),
                    _ => None,
                })
                .unwrap_or_default();
            (
                open.child(crate::ui::snapshot::probe(format!("changes:open:{index}"))),
                add.child(crate::ui::snapshot::probe(format!("changes:add:{index}"))),
            )
        };
        file_label(file, &self.palette)
            .id("diff-header")
            .child(open)
            .child(add)
            .context_menu(Self::file_menu(
                chat::Mention::Path(file.path().to_string()),
                cx,
            ))
            .into_any_element()
    }
}

impl Workspace {
    /// Shows the chat's last opened turn changes in its Changes tab, as "This turn".
    pub(in crate::ui) fn show_turn_changes(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(text) = self.turn_changes(&session) else {
            return;
        };
        let root = self
            .client
            .state
            .session
            .as_ref()
            .filter(|open| *open.session.session_id == *session)
            .and_then(|open| open.workspace.as_ref())
            .map_or_default(|workspace| workspace.path.to_string_lossy().into_owned());
        self.open_content_for(
            TabOwner::Chat(session.share()),
            ContentTab {
                id: format!("{session}:diff").into(),
                title: "Changes".into(),
                content: TabContent::Diff(DiffDoc::turn(session, &text, &root, cx)),
            },
            cx,
        );
    }

    /// A patch or diff inside the transcript, drawn as the Changes view draws it inside a
    /// bounded, scrolling card. Its state hangs off `key`; `_scope` is the disclosure scope
    /// the component's own fold state replaces.
    pub(in crate::ui) fn inline_diff(
        &self,
        key: &str,
        text: &str,
        _scope: &str,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        let root = self
            .client
            .state
            .session
            .as_ref()
            .and_then(|session| session.workspace.as_ref())
            .map_or_default(|workspace| workspace.path.to_string_lossy().into_owned());
        InlineDiff {
            // A patch still streaming gets fresh state as its text grows.
            key: format!("{key}:diff:{}", text.len()).into(),
            text: text.to_owned(),
            root,
        }
        .into_any_element()
    }
}

#[derive(IntoElement)]
struct InlineDiff {
    key: SharedString,
    text: String,
    root: String,
}

struct InlineState {
    state: Entity<DiffState>,
    truncated: bool,
    lines: usize,
}

impl RenderOnce for InlineDiff {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        let Self { key, text, root } = self;
        let inline = window.use_keyed_state(key, cx, |_, cx| {
            let document = DiffDocument::parse_in(&text, &root);
            InlineState {
                truncated: document.truncated,
                lines: text.lines().count(),
                state: cx.new(|cx| DiffState::new(document.files, cx)),
            }
        });
        let inline = inline.read(cx);
        if inline.state.read(cx).files().is_empty() {
            return div()
                .px(space::S)
                .py(space::S)
                .control_corners()
                .bg(p.panel)
                .typography(Type::Code)
                .text_color(p.muted)
                .whitespace_normal()
                .child(text)
                .into_any_element();
        }
        // The component's row height: its mono size at 1.6 lines.
        let row = window.rem_size() * (f32::from(cx.theme().mono_font_size) / 16.) * 1.6;
        let height = (row * inline.lines.to_f32()).min(INLINE_HEIGHT);
        div()
            .flex()
            .flex_col()
            .gap(space::S)
            .child(
                div()
                    .h(height)
                    .overflow_hidden()
                    .tile_corners()
                    .border_1()
                    .border_color(p.separator)
                    .child(
                        diff_view(&inline.state, &p)
                            .size_full()
                            .render_header(move |file, _, _| file_label(file, &p)),
                    ),
            )
            .children(inline.truncated.then(|| truncation_warning(TRUNCATED, &p)))
            .into_any_element()
    }
}

/// A file's header: kind glyph, name over its folder, and its totals.
fn file_label(file: &DiffFile, p: &Palette) -> Div {
    let path = file.path().as_str();
    let (parent, name) = path
        .rsplit_once('/')
        .map_or((None, path), |(parent, name)| (Some(parent), name));
    div()
        .flex()
        .items_center()
        .gap(space::S)
        .min_h(height::TOUCH)
        .w_full()
        .min_w_0()
        .child(icon(FileKind::for_name(path).glyph, p).size(glyph::LEAD))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(space::XXS)
                .flex_1()
                .min_w_0()
                .typography(Type::Mono)
                .child(div().truncate().child(name.to_owned()))
                .children(parent.map(|parent| {
                    div()
                        .truncate()
                        .text_color(p.muted)
                        .child(parent.to_owned())
                })),
        )
        .child(
            div()
                .flex()
                .flex_shrink_0()
                .gap(space::XS)
                .typography(Type::Mono)
                .when(file.additions() > 0, |el| {
                    el.child(
                        div()
                            .text_color(p.signal)
                            .child(format!("+{}", file.additions())),
                    )
                })
                .when(file.deletions() > 0, |el| {
                    el.child(
                        div()
                            .text_color(p.danger)
                            .child(format!("−{}", file.deletions())),
                    )
                }),
        )
}

const TRUNCATED: &str = "Diff truncated at the safe transfer limit";

/// A muted warning line for a listing cut short.
pub(super) fn truncation_warning(text: &'static str, p: &Palette) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(space::S)
        .px(space::S)
        .py(space::S)
        .typography(Type::Caption)
        .text_color(p.muted)
        .child(icon("hi.warning", p).size(glyph::INLINE))
        .child(text)
        .into_any_element()
}

#[cfg(feature = "snapshot")]
impl Workspace {
    /// Shows `text` as the Changes tab without asking the gateway.
    pub(in crate::ui) fn open_diff_text(
        &mut self,
        text: &str,
        expanded: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.client.state.selected_session_id.as_ref() else {
            return;
        };
        let doc = DiffDoc::new(session.share(), GitDiffScope::Committed, text, cx);
        doc.state
            .update(cx, |state, cx| set_all_collapsed(state, !expanded, cx));
        self.open_content(
            ContentTab {
                id: "snapshot:diff".into(),
                title: "Changes".into(),
                content: TabContent::Diff(doc),
            },
            cx,
        );
    }
}
