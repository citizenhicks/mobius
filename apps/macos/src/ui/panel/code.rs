//! Code documents: highlighted source, and rendered Markdown with a source toggle.

use super::*;
use crate::gateway::PreviewRequest;
use gpui_component::input::{Editor, EditorState};
use gpui_component::text::{TextView, TextViewStyle};
use mobius::SessionId;
use std::path::PathBuf;

const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone)]
pub(in crate::ui) struct CodeDoc {
    /// Workspace-relative for gateway files, absolute for local ones.
    pub path: String,
    pub text: SharedString,
    pub lines: usize,
    pub kind: FileKind,
    /// Markdown shows its source instead of the rendered page.
    pub source: bool,
    pub origin: Origin,
    pub(super) external_file: Option<FileDoc>,
    editor: Option<Entity<EditorState>>,
}

/// Where a code document reloads from.
#[derive(Clone)]
pub(in crate::ui) enum Origin {
    Local(PathBuf),
    Workspace(SessionId),
}

/// A local file read for the pane: text as a document, anything else for QuickLook.
pub(super) enum LocalPreview {
    Text(Box<CodeDoc>),
    Native(PathBuf),
}

pub(super) fn text_preview(path: &str, bytes: &[u8], origin: Origin) -> Option<CodeDoc> {
    text_of(path, bytes).map(|text| code_doc(path, text, origin))
}

/// The text of a file shown as code, or `None` for media, binaries and large files.
fn text_of<'a>(path: &str, bytes: &'a [u8]) -> Option<&'a str> {
    let media = mime_guess::from_path(path).first_or_octet_stream();
    if matches!(media.type_().as_str(), "image" | "audio" | "video")
        || media.essence_str() == "application/pdf"
        || bytes.len() > MAX_TEXT_BYTES
        || bytes.contains(&0)
    {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

fn code_doc(path: &str, text: &str, origin: Origin) -> CodeDoc {
    CodeDoc {
        path: path.into(),
        lines: text.lines().count(),
        text: SharedString::from(text.to_owned()),
        kind: FileKind::for_name(path),
        source: false,
        external_file: match &origin {
            Origin::Local(path) => Some(FileDoc::local(path.clone())), // rules-allow: the reload origin and the file actions each own the local path
            Origin::Workspace(_) => None,
        },
        origin,
        editor: None,
    }
}

/// Reads a local file off the main thread: text as a document, `None` for QuickLook.
pub(super) fn read_local_preview(path: PathBuf) -> Result<LocalPreview, String> {
    let Some(bytes) = crate::gateway::local::read_capped(&path, MAX_TEXT_BYTES as u64)
        .map_err(|error| crate::gateway::describe_io(&error))?
    else {
        return Ok(LocalPreview::Native(path));
    };
    let name = path.to_string_lossy().into_owned();
    Ok(match text_of(&name, &bytes) {
        Some(text) => LocalPreview::Text(Box::new(code_doc(&name, text, Origin::Local(path)))),
        None => LocalPreview::Native(path),
    })
}

/// A link target relative to the document at `base`; `..` and `.` are resolved.
fn resolve_link(base: &str, href: &str) -> String {
    let href = href.split('#').next().unwrap_or_default();
    let absolute = href.starts_with('/') || base.starts_with('/');
    let mut parts: Vec<&str> = match (href.starts_with('/'), base.rsplit_once('/')) {
        (false, Some((parent, _))) => parent.split('/').filter(|p| !p.is_empty()).collect(),
        _ => Vec::new(),
    };
    for part in href.split('/') {
        match part {
            "" | "." => {}
            // Climbing above the workspace stays visible so the file opener can route it.
            ".." if parts.last().is_none_or(|last| *last == "..") => {
                if !absolute {
                    parts.push("..");
                }
            }
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

impl Workspace {
    /// The code document tab `id` shows: its own, or the file selected in a Files tab.
    pub(super) fn code_doc_mut(&mut self, id: &str) -> Option<&mut CodeDoc> {
        match &mut self.content.tabs.get_mut(id)?.content {
            TabContent::Code(doc) => Some(doc),
            TabContent::Files(files) => files.code_mut(),
            _ => None,
        }
    }

    pub(super) fn code_document(
        &mut self,
        tab: &ContentTab,
        doc: &CodeDoc,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let bar = Self::code_toolbar(&tab.id, doc, cx);
        let body = self.code_view(&tab.id, doc, window, cx);
        document(bar, body, cx)
    }

    /// Path, line count, the Markdown mode, copy and refresh.
    pub(super) fn code_toolbar(id: &str, doc: &CodeDoc, cx: &Context<Self>) -> Div {
        let markdown = doc.kind.is_markdown();
        let id = SharedString::from(id.to_owned());
        toolbar()
            .child(path_label(&doc.path, doc.kind.glyph, cx))
            .child(
                // The count gives way first when the toolbar runs out of room.
                div()
                    .flex_shrink(GIVE_WAY)
                    .min_w_0()
                    .overflow_hidden()
                    .child(meta(line_count(doc.lines), cx).whitespace_nowrap()),
            )
            .child(spacer())
            .when(markdown, |bar| {
                let id = SharedString::clone(&id);
                bar.child(segmented(
                    "markdown-mode",
                    ["Rendered", "Source"],
                    Some(usize::from(doc.source)),
                    cx.listener(move |this, index: &usize, _, cx| {
                        if let Some(doc) = this.code_doc_mut(&id) {
                            doc.source = *index == 1;
                        }
                        cx.notify();
                    }),
                ))
            })
            .when_some(doc.external_file.as_ref(), |bar, path| {
                bar.child(Self::file_actions(path, &doc.path, cx))
            })
            .child(
                toolbar_button("refresh", "Refresh", "hi.refresh03", cx)
                    .on_click(cx.listener(move |this, _, _, cx| this.refresh_code(&id, cx))),
            )
    }

    /// Rendered Markdown, or the highlighted source.
    pub(super) fn code_view(
        &mut self,
        id: &str,
        doc: &CodeDoc,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if doc.kind.is_markdown() && !doc.source {
            self.markdown_body(id, doc, cx)
        } else {
            self.code_body(id, doc, window, cx)
        }
    }

    fn code_body(
        &mut self,
        id: &str,
        doc: &CodeDoc,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let editor = if let Some(editor) = &doc.editor {
            Entity::clone(editor)
        } else {
            let editor = cx.new(|cx| {
                EditorState::new(window, cx)
                    .default_value(SharedString::clone(&doc.text))
                    .language(doc.kind.language)
                    .line_number(true)
                    .folding(false)
                    .searchable(true)
            });
            if let Some(stored) = self.code_doc_mut(id) {
                stored.editor = Some(Entity::clone(&editor));
            }
            editor
        };
        div()
            .flex_1()
            .min_h_0()
            .pt(space::S)
            .child(
                Editor::new(&editor)
                    .readonly(true)
                    .bordered(false)
                    .appearance(false)
                    .size_full(),
            )
            .into_any_element()
    }

    fn markdown_body(&self, id: &str, doc: &CodeDoc, cx: &Context<Self>) -> AnyElement {
        let entity = cx.entity().downgrade();
        let (path, origin) = (doc.path.clone(), doc.origin.clone()); // rules-allow: the 'static link listener owns the document's path and origin
        // Keyed by path too, so a Files tab showing another file starts fresh.
        let key = format!("{id}:{path}");
        div()
            .id(SharedString::from(format!("{key}:markdown")))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                div()
                    .max_w(width::TRANSCRIPT)
                    .mx_auto()
                    .px(space::L)
                    .py(space::L)
                    .child(
                        TextView::markdown(
                            SharedString::from(format!("{key}:text")),
                            SharedString::clone(&doc.text),
                        )
                        .selectable(true)
                        .inline_code_style(inline_code_ring(self.palette.button_accent))
                        .style({
                            let style = markdown_style(self.palette.button_accent);
                            TextViewStyle {
                                code_block: style
                                    .code_block
                                    .bg(self.palette.control)
                                    .border_1()
                                    .border_color(self.palette.separator)
                                    .tile_corners()
                                    .p(space::L),
                                ..style
                            }
                        })
                        .on_link_click(move |href, _, _, cx| {
                            if let Err(error) = entity.update(cx, |this, cx| {
                                this.open_document_link(href, &path, &origin, cx);
                            }) {
                                tracing::debug!(%error, "the workspace closed before a document link opened");
                            }
                        }),
                    ),
            )
            .into_any_element()
    }

    fn open_document_link(
        &mut self,
        href: &str,
        base: &str,
        origin: &Origin,
        cx: &mut Context<Self>,
    ) {
        if href.starts_with("http://") || href.starts_with("https://") {
            cx.open_url(href);
            return;
        }
        if href.starts_with('#') || href.contains(':') {
            return;
        }
        let target = resolve_link(base, href);
        match origin {
            Origin::Workspace(session) => self.open_workspace_file(session.share(), target, cx),
            Origin::Local(_) => {
                let path = PathBuf::from(&target);
                let title = file_title(&target);
                self.open_file_preview(target, title, path, cx);
            }
        }
    }

    /// Reloads a code document in place, keeping its tab.
    fn refresh_code(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(tab) = self.content.tabs.get(id) else {
            return;
        };
        match &tab.content {
            TabContent::Code(_) => {}
            TabContent::Files(files) => {
                if let Some(path) = files.selected_path().map(str::to_owned) {
                    self.load_workspace_selection(id, path, cx);
                }
                return;
            }
            _ => return,
        }
        // The reload opens a new tab under the same id, so the old one is taken apart.
        let Some(ContentTab {
            title,
            content: TabContent::Code(doc),
            ..
        }) = self.content.tabs.remove(id)
        else {
            return;
        };
        match doc.origin {
            Origin::Local(path) => self.open_file_preview(id, title, path, cx),
            Origin::Workspace(session) => {
                let request = PreviewRequest::File(doc.path);
                self.request_preview(id.into(), title, session, request, cx);
            }
        }
    }
}

pub(super) fn file_title(path: &str) -> String {
    crate::ui::navigation::workspace_name(path).to_owned()
}

#[cfg(feature = "snapshot")]
impl Workspace {
    /// Opens a local file synchronously, as snapshots need it drawn on the next frame.
    pub(in crate::ui) fn open_local_now(
        &mut self,
        path: PathBuf,
        source: bool,
        cx: &mut Context<Self>,
    ) {
        let id: Arc<str> = path.to_string_lossy().into();
        let title = file_title(&id).into();
        let id = self
            .content
            .navigation
            .register_file(&TabOwner::of(&self.page_tabs), id);
        let content = match read_local_preview(path) {
            Ok(LocalPreview::Text(doc)) => TabContent::Code(CodeDoc { source, ..*doc }),
            Ok(LocalPreview::Native(path)) => TabContent::File(FileDoc::local(path)),
            Err(error) => TabContent::Failed(error),
        };
        self.open_content(ContentTab { id, title, content }, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_link;

    #[test]
    fn markdown_links_resolve_against_the_document() {
        assert_eq!(resolve_link("docs/guide/a.md", "../b.md#top"), "docs/b.md");
        assert_eq!(resolve_link("README.md", "./src/lib.rs"), "src/lib.rs");
        assert_eq!(resolve_link("/Users/me/a.md", "b.md"), "/Users/me/b.md");
        assert_eq!(
            resolve_link("README.md", "/Users/me/.mobius/skills/x/SKILL.md"),
            "/Users/me/.mobius/skills/x/SKILL.md"
        );
        assert_eq!(
            resolve_link("docs/a.md", "../../skills/b.md"),
            "../skills/b.md"
        );
        assert_eq!(resolve_link("/a.md", "../../b.md"), "/b.md");
    }
}
