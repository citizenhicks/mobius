use super::*;
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::text::{SelectionFormat, TextView, TextViewState};
use std::rc::Rc;

pub(in crate::ui) struct StreamingMarkdown {
    state: Entity<TextViewState>,
    /// The entry generation and text length `state` holds. Compared instead of the text, so
    /// a frame or a chunk costs nothing for the text streamed so far.
    generation: u64,
    length: usize,
    /// `]:` appeared in this text: a definition may change blocks it is not in.
    defines: bool,
    /// The text length at the last parse of everything.
    parsed: usize,
    /// The finished text for the message's actions, built when first asked for.
    shared: Option<SharedString>,
    pub(in crate::ui) session: Option<SessionId>,
}

/// Every link reference and footnote definition holds `]:`. Prose and code may too; that only
/// costs them one more parse when they finish.
fn holds_definition(bytes: &[u8]) -> bool {
    bytes.windows(2).any(|pair| pair == b"]:")
}

impl StreamingMarkdown {
    fn new(generation: u64, text: &str, session: Option<&SessionId>, cx: &mut App) -> Self {
        Self {
            state: cx.new(|cx| TextViewState::markdown(text, cx)),
            generation,
            length: text.len(),
            defines: holds_definition(text.as_bytes()),
            parsed: text.len(),
            shared: None,
            session: session.map(SessionId::share),
        }
    }

    /// Brings the view up to `text`. `set_text` appends what extends the text and parses
    /// anything else again, reparsing only the last block with a chunk: enough for brackets
    /// and tags split across chunks, but definitions in other blocks stay out of its sight,
    /// so a text holding one is parsed whole when it finishes.
    fn catch_up(&mut self, generation: u64, text: &str, streaming: bool, cx: &mut App) {
        let grew = self.generation == generation;
        if !grew || self.length != text.len() {
            // One byte back: the last chunk may have ended between `]` and `:`.
            let unseen = if grew {
                self.length.saturating_sub(1)
            } else {
                0
            };
            let bytes = text.as_bytes().get(unseen..).unwrap_or_default();
            self.defines = (grew && self.defines) || holds_definition(bytes);
            self.state.update(cx, |state, cx| state.set_text(text, cx));
            self.generation = generation;
            self.length = text.len();
            self.shared = None;
        }
        if !streaming && self.defines && self.parsed != text.len() {
            // `set_text` cannot read a shorter text as an extension, so this parses it all,
            // keeping the old document on screen while a long text parses in the background.
            self.state.update(cx, |state, cx| {
                state.push_str("\n", cx);
                state.set_text(text, cx);
            });
            self.parsed = text.len();
        }
    }
}

impl Workspace {
    /// The view of a growing or finished `text`, and the text as actions keep it once it is
    /// `finished`.
    pub(super) fn streaming_transcript_markdown(
        &self,
        id: SharedString,
        generation: u64,
        text: &str,
        (streaming, finished): (bool, bool),
        session: Option<&SessionId>,
        cx: &mut Context<Self>,
    ) -> (TextView, Option<SharedString>) {
        let mut views = self.chat_ui.markdown.borrow_mut();
        let view = views
            .entry(id)
            .or_insert_with(|| StreamingMarkdown::new(generation, text, session, cx));
        view.catch_up(generation, text, streaming, cx);
        let shared = finished.then(|| {
            SharedString::clone(
                view.shared
                    .get_or_insert_with(|| SharedString::from(text.to_owned())),
            )
        });
        let entity = cx.entity().downgrade();
        let session = session.map(SessionId::share);
        let text_view = TextView::new(&view.state)
            .style(markdown_style(self.palette.button_accent))
            .text_color(self.palette.ink)
            .selectable(true)
            .selection_format(SelectionFormat::SourceAcrossStyles)
            .on_link_click(move |href, _, _, cx| {
                let opened = entity.update(cx, |this, cx| {
                    this.open_transcript_link(href, session.as_ref(), cx);
                });
                if let Err(error) = opened {
                    tracing::debug!(%error, "transcript link on a released workspace");
                }
            });
        (text_view, shared)
    }
    pub(super) fn transcript_markdown(
        &self,
        id: SharedString,
        text: impl Into<SharedString>,
        session: Option<&SessionId>,
        cx: &Context<Self>,
    ) -> TextView {
        let entity = cx.entity().downgrade();
        let session = session.map(SessionId::share);
        TextView::markdown(id, text)
            .style(markdown_style(self.palette.button_accent))
            .text_color(self.palette.ink)
            .selectable(true)
            .selection_format(SelectionFormat::SourceAcrossStyles)
            .on_link_click(move |href, _, _, cx| {
                let opened = entity.update(cx, |this, cx| {
                    this.open_transcript_link(href, session.as_ref(), cx);
                });
                if let Err(error) = opened {
                    tracing::debug!(%error, "transcript link on a released workspace");
                }
            })
    }

    pub(super) fn open_transcript_link(
        &mut self,
        href: &str,
        session: Option<&SessionId>,
        cx: &mut Context<Self>,
    ) {
        match link_target(href) {
            Some(LinkTarget::Browser(url)) => cx.open_url(&url),
            Some(LinkTarget::File(path)) => {
                let owner = self.client.state.session.as_ref().and_then(|open| {
                    open.workspace
                        .as_ref()
                        .map(|_| open.session.session_id.as_ref())
                });
                if let Some(session) = session.filter(|session| {
                    file_link_available(
                        session,
                        self.client.state.selected_session_id.as_deref(),
                        owner,
                        self.client.link.is_ready(),
                        &path,
                    )
                }) {
                    self.open_workspace_file(session.share(), path, cx);
                } else {
                    self.report("This file is not available in this chat.");
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    pub(super) fn transcript_links<'a>(
        &self,
        links: impl IntoIterator<Item = &'a mobius::protocol::FrontendLink>,
        session: Option<&SessionId>,
        scope: &str,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let mut seen = std::collections::BTreeSet::new();
        let (files, mut links): (Vec<_>, Vec<_>) = links
            .into_iter()
            .filter(|link| seen.insert(link.href.as_str()))
            .partition(|link| matches!(link_target(&link.href), Some(LinkTarget::File(_))));
        let mut result = Vec::new();
        if files.len() > 1 {
            result.push(self.transcript_file_menu(&files, session, scope, cx));
        } else {
            links.extend(files);
        }
        result.extend(links.iter().enumerate().map(|(index, link)| {
            let href = link.href.clone(); // rules-allow: the 'static click handler owns its link
            let session = session.map(SessionId::share);
            Button::new(SharedString::from(format!("{scope}:link:{index}")))
                .ghost()
                .small()
                .px_0()
                .min_w_0()
                .accessibility_label(format!("Open {}", link.label))
                .text_color(self.palette.button_accent)
                .child(div().truncate().underline().child(link.label.clone())) // rules-allow: the element owns its label while the block keeps the link
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.open_transcript_link(&href, session.as_ref(), cx);
                }))
                .into_any_element()
        }));
        result
    }

    fn transcript_file_menu(
        &self,
        links: &[&mobius::protocol::FrontendLink],
        session: Option<&SessionId>,
        scope: &str,
        cx: &Context<Self>,
    ) -> AnyElement {
        let items: Vec<(SharedString, Rc<str>)> = links
            .iter()
            .map(|link| (link.label.clone().into(), link.href.as_str().into())) // rules-allow: the 'static menu owns its labels while the block keeps the links
            .collect();
        let view = cx.entity().downgrade();
        let session = session.map(SessionId::share);
        let count = items.len();
        Button::new(SharedString::from(format!("{scope}:files")))
            .ghost()
            .small()
            .px_0()
            .min_w_0()
            .accessibility_label(format!("Open {count} changed files"))
            .tooltip("Opens the file viewer")
            .text_color(self.palette.button_accent)
            .child(div().truncate().underline().child(format!("{count} files")))
            .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, _| {
                for (label, href) in &items {
                    let (href, session, view) = (
                        Rc::clone(href),
                        session.as_ref().map(SessionId::share),
                        WeakEntity::clone(&view),
                    );
                    let item = PopupMenuItem::new(SharedString::clone(label));
                    menu = menu.item(item.on_click(move |_, _, cx| {
                        let opened = view.update(cx, |this, cx| {
                            this.open_transcript_link(&href, session.as_ref(), cx);
                        });
                        if let Err(error) = opened {
                            tracing::debug!(%error, "file link on a released workspace");
                        }
                    }));
                }
                menu
            })
            .into_any_element()
    }
}

#[cfg(all(test, feature = "snapshot"))]
mod streaming_tests {
    use super::*;
    use gpui::TestAppContext;

    fn streamed(chunks: &[&str], cx: &TestAppContext) -> (StreamingMarkdown, String) {
        cx.update(gpui_component::init);
        let mut text = String::new();
        let mut view = cx.update(|cx| StreamingMarkdown::new(1, &text, None, cx));
        for chunk in chunks {
            text.push_str(chunk);
            cx.update(|cx| view.catch_up(1, &text, true, cx));
            cx.run_until_parked();
        }
        (view, text)
    }

    fn rendered(state: &Entity<TextViewState>, cx: &TestAppContext) -> String {
        state.read_with(cx, |state, _| state.rendered_text().as_str().to_owned())
    }

    fn fresh_parse(text: &str, cx: &TestAppContext) -> String {
        let fresh = cx.update(|cx| StreamingMarkdown::new(0, text, None, cx));
        cx.run_until_parked();
        rendered(&fresh.state, cx)
    }

    #[gpui::test]
    fn a_finished_answer_with_definitions_renders_as_a_fresh_parse(cx: &TestAppContext) {
        for chunks in [
            // A definition after its reference, with its marker split between two chunks.
            &[
                "See [a][id] now\n\nmore\n\n[id]",
                ": http://x.example\n\nend",
            ][..],
            &["[id]: http://x.example\n\nfirst\n\n", "See [a][id] now"],
            &["word[^1] here\n\nnext\n\n", "[^1]: the note"],
        ] {
            let (mut view, text) = streamed(chunks, cx);
            cx.update(|cx| view.catch_up(1, &text, false, cx));
            cx.run_until_parked();
            assert_eq!(rendered(&view.state, cx), fresh_parse(&text, cx), "{text}");
        }
    }

    #[gpui::test]
    fn brackets_and_tags_keep_appending_and_replaced_text_parses_again(cx: &mut TestAppContext) {
        let (mut view, text) = streamed(
            &[
                "[x] see <b>bo",
                "ld</b> and [link",
                "](http://x.example)\n\n",
            ],
            cx,
        );
        assert_eq!(rendered(&view.state, cx), fresh_parse(&text, cx));
        // An append keeps a selection, a parse of everything does not.
        view.state.update(cx, TextViewState::select_all);
        let more = format!("{text}plain words");
        cx.update(|cx| view.catch_up(1, &more, true, cx));
        cx.run_until_parked();
        cx.update(|cx| view.catch_up(1, &more, false, cx));
        cx.run_until_parked();
        let selected = |view: &StreamingMarkdown, cx: &mut TestAppContext| {
            view.state.read_with(cx, |state, _| state.selected_text())
        };
        assert!(selected(&view, cx).contains("plain words"));
        assert!(!view.defines);

        cx.update(|cx| view.catch_up(2, "corrected", true, cx));
        cx.run_until_parked();
        assert_eq!(rendered(&view.state, cx), fresh_parse("corrected", cx));
        assert!(selected(&view, cx).is_empty());
    }
}

enum LinkTarget {
    Browser(String),
    File(String),
}

fn file_link_available(
    session: &str,
    selected: Option<&str>,
    workspace_owner: Option<&str>,
    ready: bool,
    path: &str,
) -> bool {
    !session.is_empty()
        && selected == Some(session)
        && workspace_owner == Some(session)
        && ready
        && !path.is_empty()
        && path.len() <= 4_096
        && !path.chars().any(char::is_control)
        && !path.split('/').any(|part| part == "..")
}

fn link_target(href: &str) -> Option<LinkTarget> {
    if href.chars().any(char::is_control) {
        return None;
    }
    let web = href.split_once(':').is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    });
    if !web && !safe_file_href(href) {
        return None;
    }
    if let Ok(url) = gpui::http_client::Url::parse(href) {
        return match url.scheme() {
            "http" | "https" => Some(LinkTarget::Browser(url.to_string())),
            "file" => url
                .to_file_path()
                .ok()
                .map(|path| LinkTarget::File(path.to_string_lossy().into_owned())),
            _ => None,
        };
    }
    let path = href.split('#').next()?.split('?').next()?;
    if path.is_empty() || path.contains(':') {
        return None;
    }
    let uri = gpui::http_client::Url::parse("file:///")
        .ok()?
        .join(path)
        .ok()?;
    let decoded = uri.to_file_path().ok()?;
    let decoded = if path.starts_with('/') {
        decoded.as_path()
    } else {
        decoded.strip_prefix("/").ok()?
    };
    Some(LinkTarget::File(decoded.to_string_lossy().into_owned()))
}

/// Check the URI before URL resolution can erase traversal segments or control bytes.
fn safe_file_href(href: &str) -> bool {
    let mut bytes = href.split(['#', '?']).next().unwrap_or_default().bytes();
    let (mut dots, mut only_dots) = (0usize, true);
    while let Some(mut byte) = bytes.next() {
        if byte == b'%' {
            let Some(high) = bytes.next().and_then(|byte| char::from(byte).to_digit(16)) else {
                return false;
            };
            let Some(low) = bytes.next().and_then(|byte| char::from(byte).to_digit(16)) else {
                return false;
            };
            let Ok(decoded) = u8::try_from(high * 16 + low) else {
                return false;
            };
            byte = decoded;
        }
        if byte.is_ascii_control() {
            return false;
        }
        if matches!(byte, b'/' | b'\\') {
            if only_dots && dots == 2 {
                return false;
            }
            (dots, only_dots) = (0, true);
        } else if byte == b'.' {
            dots += 1;
        } else {
            only_dots = false;
        }
    }
    !(only_dots && dots == 2)
}

#[cfg(test)]
mod tests {
    use super::{LinkTarget, file_link_available, link_target};

    #[test]
    fn transcript_file_links_require_their_ready_workspace_and_bounded_path() {
        let available = |selected, owner, ready, path| {
            file_link_available("chat", selected, owner, ready, path)
        };
        assert!(available(Some("chat"), Some("chat"), true, "src/main.rs"));
        assert!(!available(Some("other"), Some("chat"), true, "src/main.rs"));
        assert!(!available(Some("chat"), Some("other"), true, "src/main.rs"));
        assert!(!available(Some("chat"), None, true, "src/main.rs"));
        assert!(!available(Some("chat"), Some("chat"), false, "src/main.rs"));
        for path in ["", "src/../secret", "src/line\n.rs", &"a".repeat(4_097)] {
            assert!(!available(Some("chat"), Some("chat"), true, path));
        }
        assert!(link_target("src/line\n.rs").is_none());
        for href in [
            "src/line%0A.rs",
            "../secret",
            "src/../secret",
            "src/%2e%2E/secret",
            "src%2F..%2Fsecret",
            "file:///tmp/%2E%2E/secret",
            "src%5C..%5Csecret",
        ] {
            assert!(link_target(href).is_none(), "{href}");
        }
    }
    #[test]
    fn transcript_links_keep_remote_paths_in_the_gateway_and_reject_other_schemes() {
        assert!(
            matches!(link_target("src%2Fa+b.rs"), Some(LinkTarget::File(path)) if path == "src/a+b.rs")
        );
        assert!(
            matches!(link_target("src/main.rs#L20"),Some(LinkTarget::File(path)) if path=="src/main.rs")
        );
        assert!(
            matches!(link_target("file:///tmp/hello%20world.txt"),Some(LinkTarget::File(path)) if path=="/tmp/hello world.txt")
        );
        assert!(matches!(
            link_target("https://example.com/path"),
            Some(LinkTarget::Browser(_))
        ));
        assert!(link_target("javascript:alert(1)").is_none());
        assert!(link_target("#heading").is_none());
        assert!(
            matches!(link_target("src%2Ftest%20%23%C3%A9.rs"), Some(LinkTarget::File(path)) if path == "src/test #é.rs")
        );
        assert!(
            matches!(link_target("https%3A%2F%2Fexample.com"), Some(LinkTarget::File(path)) if path == "https://example.com")
        );
    }
}
