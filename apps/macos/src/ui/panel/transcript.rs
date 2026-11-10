//! Transcript documents: subagent and voice previews, and routine runs.

use super::*;
use crate::gateway::{Preview, TranscriptStore};
use crate::ui::transcript::HISTORY;
use mobius::SessionId;
use mobius::protocol::Op;
use mobius_gateway::wire::RoutineRunPreview;
use std::rc::Rc;

/// A routine run's transcript; its source keeps the run and the cursor of older pages.
pub(in crate::ui) struct RoutineDoc {
    pub(in crate::ui) source: RoutineRunPreview,
    transcript: TranscriptStore,
}

impl RoutineDoc {
    /// The run's page, below what `previous` already loaded of it.
    fn new(mut source: RoutineRunPreview, previous: Option<&Self>) -> Self {
        let page = std::mem::take(&mut source.page);
        let transcript = match previous {
            Some(previous) => {
                let mut transcript = TranscriptStore::from(previous.transcript.page());
                transcript.prepend(page);
                transcript
            }
            None => TranscriptStore::from(page),
        };
        Self { source, transcript }
    }
}

impl Workspace {
    /// Refresh only the visible document. Session switching preserves the other open
    /// snapshots through preserve_previews before the selected-session map is cleared.
    pub(super) fn refresh_active_preview(&mut self) {
        let Some(PageTab::Content(id)) = self.page_tabs.get(self.active_page) else {
            return;
        };
        if let Some(ContentTab {
            content:
                TabContent::Preview {
                    session_id,
                    preview,
                },
            ..
        }) = self.content.tabs.get_mut(id)
            && self.client.state.selected_session_id.as_ref() == Some(session_id)
            && let Some(current) = self.client.state.previews.get(&preview.source.id)
        {
            *preview = Rc::clone(current);
        }
    }

    pub(in crate::ui) fn open_routine_preview(
        &mut self,
        preview: RoutineRunPreview,
        cx: &mut Context<Self>,
    ) {
        let id: Arc<str> = format!("routine:{}", preview.run.id).into();
        let previous = match self.content.tabs.get(&id) {
            Some(ContentTab {
                content: TabContent::Routine(previous),
                ..
            }) => Some(previous.as_ref()),
            _ => None,
        };
        let document = RoutineDoc::new(preview, previous);
        self.open_content_for(
            TabOwner::Global,
            ContentTab {
                id,
                title: document
                    .source
                    .routine
                    .instructions
                    .lines()
                    .next()
                    .unwrap_or("Routine run")
                    .chars()
                    .take(60)
                    .collect::<String>()
                    .into(),
                content: TabContent::Routine(Rc::new(document)),
            },
            cx,
        );
    }

    /// Asks the chat's gateway for a transcript, such as a subagent's or the voice call's,
    /// and opens it beside the chat, waiting in place until it arrives.
    pub(in crate::ui) fn open_preview_command(
        &mut self,
        title: SharedString,
        op: Op,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self
            .client
            .state
            .selected_session_id
            .as_ref()
            .map(SessionId::share)
        else {
            return;
        };
        let id: Arc<str> = match &op {
            Op::CapabilityCommand {
                capability,
                command,
                arguments,
                ..
            } => format!("{session}:command:{capability}:{command}:{arguments}"),
            _ => format!("{session}:command"),
        }
        .into();
        let Some(request) = self.dispatch_widget_operation(op, false, cx) else {
            return;
        };
        if let Some(ContentTab {
            content: TabContent::Preview { .. },
            ..
        }) = self.content.tabs.get(&id)
        {
            self.activate_tab(PageTab::Content(id));
            cx.notify();
            return;
        }
        let content = TabContent::Loading {
            request,
            session_id: Some(session),
        };
        self.open_content(ContentTab { id, title, content }, cx);
    }

    /// Shows a command's answer in the tab waiting for it, the tab already showing that
    /// transcript, or a new one.
    pub(super) fn present_preview(
        &mut self,
        session: &SessionId,
        request: &str,
        preview: Rc<Preview>,
        cx: &mut Context<Self>,
    ) {
        let existing = self
            .content
            .tabs
            .values_mut()
            .find(|tab| match &tab.content {
                TabContent::Loading {
                    request: pending,
                    session_id,
                } => **pending == *request && session_id.as_ref() == Some(session),
                TabContent::Preview {
                    session_id,
                    preview: shown,
                } => session_id == session && shown.source.id == preview.source.id,
                _ => false,
            });
        let Some(tab) = existing else {
            return self.open_preview_tab(session, preview, cx);
        };
        let waiting = matches!(tab.content, TabContent::Loading { .. });
        tab.title = Arc::clone(&preview.source.title).into();
        tab.content = TabContent::Preview {
            session_id: session.share(),
            preview,
        };
        let route = waiting.then(|| Arc::clone(&tab.id));
        if let Some(id) = route {
            self.activate_tab(PageTab::Content(id));
        }
        cx.notify();
    }

    pub(in crate::ui) fn open_preview_tab(
        &mut self,
        session_id: &SessionId,
        preview: Rc<Preview>,
        cx: &mut Context<Self>,
    ) {
        self.open_content(
            ContentTab {
                id: format!("{session_id}:preview:{}", preview.source.id).into(),
                title: Arc::clone(&preview.source.title).into(),
                content: TabContent::Preview {
                    session_id: session_id.share(),
                    preview,
                },
            },
            cx,
        );
    }

    pub(super) fn preview_document(
        &mut self,
        tab: &ContentTab,
        preview: &Preview,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let current = &preview.source;
        // A subagent's header reads as SwiftUI's: "name · status", with
        // the spawn context behind a glyph rather than in the subtitle.
        let bar =
            if current.kind == mobius::protocol::FrontendPreviewKind::Agent {
                let title = self.subagent_status(&current.id).map_or_else(
                    || current.title.to_string(),
                    |status| format!("{} · {status}", current.title),
                );
                transcript_toolbar(tab.glyph(), &title, "", current.started_at_ms.is_some(), cx)
                    .when(!current.subtitle.is_empty(), |bar| {
                        bar.child(spacer())
                            .child(spawn_context(&current.subtitle, cx))
                    })
            } else {
                transcript_toolbar(
                    tab.glyph(),
                    &current.title,
                    &current.subtitle,
                    current.started_at_ms.is_some(),
                    cx,
                )
            };
        let body = self.document_list(
            &tab.id,
            &preview.transcript,
            current.next.is_some(),
            window,
            cx,
        );
        document(bar, body, cx)
    }

    pub(super) fn routine_document(
        &mut self,
        tab: &ContentTab,
        doc: &RoutineDoc,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let live = doc.source.run.finished_at.is_none();
        let bar = transcript_toolbar(tab.glyph(), &tab.title, "Routine run", live, cx);
        let body = self.document_list(
            &tab.id,
            &doc.transcript,
            doc.source.next.is_some(),
            window,
            cx,
        );
        document(bar, body, cx)
    }

    /// A preview's or routine run's transcript, its Load more row first while it has one.
    fn document_list(
        &mut self,
        scope: &Arc<str>,
        transcript: &TranscriptStore,
        more: bool,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let ids = more
            .then(|| HISTORY.into())
            .into_iter()
            .chain(transcript.rows().map(|row| Arc::clone(&row.id)))
            .collect();
        let list = self.prepare_chat_list(scope, ids, true, window, cx);
        let entity = cx.entity().downgrade();
        let tab = Arc::clone(scope);
        let render = move |index: usize, _: &mut Window, cx: &mut App| {
            entity
                .update(cx, |this, cx| this.document_row(&tab, index, cx))
                .unwrap_or_else(|_| div().into_any_element())
        };
        self.chat_list(scope, list, None, render, cx)
    }

    /// One item of a document's list, spaced from the next as a chat's items are.
    fn document_row(&self, scope: &Arc<str>, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let rows = self
            .transcript_scrolls
            .get(&**scope)
            .map_or(&[][..], |state| &state.rows[..]);
        let row = rows.get(index).and_then(|id| {
            let tab = self.content.tabs.get(&**scope)?;
            let (transcript, session) = match &tab.content {
                TabContent::Preview {
                    session_id,
                    preview,
                } => (&preview.transcript, Some(session_id)),
                TabContent::Routine(doc) => (&doc.transcript, doc.source.run.session_id.as_ref()),
                _ => return None,
            };
            if &**id == HISTORY {
                return self.load_more(scope, &tab.content, cx);
            }
            let row = transcript.row(id)?;
            Some(self.transcript_row_view(transcript, row, session, scope, cx))
        });
        div()
            .w_full()
            .when(index + 1 < rows.len(), |item| item.pb(space::S))
            .children(row)
            .into_any_element()
    }

    /// The button that asks for a document's earlier page, keeping the reader in place.
    fn load_more(
        &self,
        scope: &Arc<str>,
        content: &TabContent,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let scope = Arc::clone(scope);
        match content {
            TabContent::Preview {
                session_id,
                preview,
            } => {
                let op = preview.source.next.clone()?; // rules-allow: the 'static load-more listener owns the history request
                Some(
                    div()
                        .flex()
                        .justify_center()
                        .child(
                            button("preview-more", "Load more")
                                .ghost()
                                .disabled(
                                    self.client.state.selected_session_id.as_ref()
                                        != Some(session_id),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.begin_history_restore(&scope);
                                    this.submit_op(op.clone(), cx); // rules-allow: every click submits its own request
                                })),
                        )
                        .into_any_element(),
                )
            }
            TabContent::Routine(doc) => {
                let before = doc.source.next.clone()?; // rules-allow: the 'static load-more listener owns its cursor
                let run = Arc::clone(&doc.source.run.id);
                Some(
                    button("routine-more", "Load earlier activity")
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.begin_history_restore(&scope);
                            this.open_run(&run, Some(before.clone()), cx); // rules-allow: every click sends its own request
                        }))
                        .into_any_element(),
                )
            }
            _ => None,
        }
    }
}

/// How much of the parent conversation a subagent started with: SwiftUI's "Spawn
/// context" glyph, explained in its tooltip.
fn spawn_context(context: &str, cx: &App) -> AnyElement {
    let (glyph, detail) = spawn_context_detail(context);
    let tooltip = SharedString::from(format!("Spawn context: {context}. {detail}"));
    div()
        .id("spawn-context")
        .flex_shrink_0()
        .aria_label(SharedString::clone(&tooltip))
        .child(icon(glyph, &palette(cx)).size(glyph::INLINE))
        .tooltip(text_tooltip(tooltip))
        .into_any_element()
}

/// The glyph and sentence for a spawn context such as "No context", "Full context" or
/// "Last 2 turns" (PreviewTranscriptSheet.spawnContextGlyph and spawnContextDetail).
fn spawn_context_detail(context: &str) -> (&'static str, String) {
    let lowered = context.to_lowercase();
    if lowered.starts_with("no ") || lowered == "none" {
        (
            "hi.circle",
            "This agent started fresh with only its assigned task. It inherited none of the \
             parent conversation."
                .to_owned(),
        )
    } else if lowered.starts_with("full") {
        (
            "hi.circleDot",
            "This agent inherited the full parent conversation as its starting context.".to_owned(),
        )
    } else {
        (
            "hi.circleDotDashed",
            format!("This agent inherited {lowered} from the parent conversation."),
        )
    }
}

/// Symbol, title and subtitle, with a signal dot while the transcript is live.
fn transcript_toolbar(glyph: &str, title: &str, subtitle: &str, live: bool, cx: &App) -> Div {
    let p = palette(cx);
    toolbar()
        .child(icon(glyph, &palette(cx)).size(glyph::INLINE))
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .child(
                    div()
                        .truncate()
                        .typography(Type::BodyMedium)
                        .child(title.to_owned()),
                )
                .when(!subtitle.is_empty(), |text| {
                    text.child(
                        div()
                            .truncate()
                            .typography(Type::Caption)
                            .text_color(p.muted)
                            .child(subtitle.to_owned()),
                    )
                }),
        )
        .when(live, |bar| bar.child(status_dot(p.signal)))
}
