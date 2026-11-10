//! The transcript's disclosures (TranscriptEventViews.swift): Worked for, event groups,
//! event lines with their detail, and reasoning lines.

use super::*;
use crate::gateway::RowChild;
use mobius::protocol::{FrontendBlock, FrontendBlockFormat, FrontendBlockRole};
use mobius_gateway::wire::ActivityCounts;

/// A disclosure summary row: glyph, one line of muted text and a trailing mark.
fn summary_row(
    glyph: &'static str,
    text: impl IntoElement,
    trailing: AnyElement,
    p: &Palette,
) -> Div {
    div()
        .min_w_0()
        .min_h(height::REGULAR)
        .flex()
        .items_center()
        .gap(space::S)
        .typography(Type::Body)
        .text_color(p.muted)
        .child(icon(glyph, p).size(glyph::INLINE))
        .child(div().min_w_0().truncate().child(text))
        .child(trailing)
}

impl Workspace {
    pub(in crate::ui) fn toggle_disclosure(
        &mut self,
        id: &str,
        scope: &str,
        cx: &mut Context<Self>,
    ) {
        self.stop_following_transcript(scope);
        if !self.expanded.remove(id) {
            self.expanded.insert(id.to_owned());
            self.chat_ui.opened.insert(id.to_owned());
        }
        cx.notify();
    }

    /// A native keyboard-accessible disclosure trigger; resource actions remain siblings.
    fn disclosure_button(
        &self,
        button_id: SharedString,
        id: &str,
        scope: &str,
        label: &str,
        heading: Div,
        cx: &Context<Self>,
    ) -> Button {
        let (toggle, scope) = (id.to_owned(), scope.to_owned());
        Button::new(button_id)
            .ghost()
            .px_0()
            .h_auto()
            .min_w_0()
            .accessibility_label(format!(
                "{label}, {}",
                if self.expanded.contains(id) {
                    "expanded"
                } else {
                    "collapsed"
                }
            ))
            .child(heading)
            .on_click(
                cx.listener(move |this, _, _, cx| this.toggle_disclosure(&toggle, &scope, cx)),
            )
    }

    /// Whether a disclosure's body shows: expanded, or still closing.
    fn disclosure_shows(&self, id: &str) -> bool {
        self.expanded.contains(id) || motion::visible(&format!("{id}:body"))
    }

    /// A header over a body that opens and closes to its measured height, fading with it.
    fn disclosure(
        &self,
        id: &str,
        header: impl IntoElement,
        body: Option<AnyElement>,
        spacing: Pixels,
        _: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = self.expanded.contains(id);
        let body_id = format!("{id}:body");
        let body = body
            .filter(|_| expanded || motion::visible(&body_id))
            .map(|body| {
                // The gap travels with the body, so a closed body leaves no space behind.
                let content = div()
                    .pt(spacing)
                    .opacity(motion::progress(&body_id))
                    .child(body);
                motion::smooth(body_id, content)
                    .open(expanded)
                    .grow_in(self.chat_ui.opened.contains(id))
                    .into_any_element()
            });
        div()
            .flex()
            .flex_col()
            // A row, so the header keeps its own width and its accessories follow the title.
            .child(div().flex().child(header))
            .children(body)
            .into_any_element()
    }

    /// A turn's work behind "Working m:ss" while it runs, then "Worked for m:ss"
    /// (WorkedForGroupView).
    pub(super) fn worked_row(
        &self,
        transcript: &TranscriptStore,
        row: &TranscriptRow,
        (counts, started_at_ms, duration_ms): (ActivityCounts, i64, Option<u64>),
        session_id: Option<&SessionId>,
        scope: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = format!("{scope}:{}", row.id);
        let caret = chevron(self.expanded.contains(&id), &self.palette).into_any_element();
        let label = match duration_ms {
            Some(duration) => format!("Worked for {}", clock_duration(duration)),
            None => format!(
                "Working {}",
                clock_duration(
                    chat::now_ms()
                        .saturating_sub(started_at_ms)
                        .max(0)
                        .cast_unsigned()
                )
            ),
        };
        let text = if duration_ms.is_none() {
            Self::running_summary(label.clone()) // rules-allow: the shimmer and the accessibility label each own the text
        } else {
            SharedString::from(label.clone()).into_any_element() // rules-allow: the row and the accessibility label each own the text
        };
        let header = summary_row("hi.combine", text, caret, &self.palette);
        let summary = activity_summary(counts);
        let header = self.disclosure_button(
            format!("{id}:heading").into(),
            &id,
            scope,
            &if summary.is_empty() {
                label
            } else {
                format!("{label}, {summary}")
            },
            header,
            cx,
        );
        let body = self.disclosure_shows(&id).then(|| {
            div()
                .flex()
                .flex_col()
                .gap(space::S)
                .children(transcript.row_children(row).map(|child| match child {
                    RowChild::Item(entry) => self.work_item(entry, session_id, scope, cx),
                    RowChild::Row(run) => {
                        self.transcript_row(transcript, run, session_id, scope, cx)
                    }
                }))
                .into_any_element()
        });
        // The files the work produced are the turn's deliverables: SwiftUI pins them to the
        // answer that follows, so they stay in view while the work is folded. Unfolded, each
        // inner row shows its own files, so they are not repeated here.
        let files = body
            .is_none()
            .then(|| self.file_row(row_files(transcript, row), session_id, false, cx))
            .flatten();
        div()
            .flex()
            .flex_col()
            .gap(space::S)
            .child(self.disclosure(&id, header, body, space::S, cx))
            .children(files)
            .into_any_element()
    }

    /// One item of a turn's work or an event group, as its own line.
    fn work_item(
        &self,
        entry: &TranscriptEntry,
        session_id: Option<&SessionId>,
        scope: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match &entry.content {
            TranscriptItemContent::Assistant {
                phase: ModelStepContentPhase::Reasoning,
                text,
                ..
            } => self.reasoning_line(&entry.id, text, session_id, scope, cx),
            TranscriptItemContent::Block { block, .. }
                if block.role == FrontendBlockRole::Notice =>
            {
                self.notice_row(entry)
            }
            TranscriptItemContent::Block { .. } => self.event_line(entry, session_id, scope, cx),
            _ => self.transcript_entry(entry, None, session_id, scope, cx),
        }
    }

    /// A run of events outside a turn behind one summary line (EventGroupView); files stay
    /// outside it.
    pub(super) fn event_group(
        &self,
        transcript: &TranscriptStore,
        row: &TranscriptRow,
        counts: ActivityCounts,
        session_id: Option<&SessionId>,
        scope: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let id = format!("{scope}:{}", row.id);
        let mark = chevron(self.expanded.contains(&id), &palette(cx)).into_any_element();
        let last = transcript.row_items(row).next_back();
        let label = activity_summary(counts);
        let heading = summary_row(
            last.map_or("hi.loader", activity_glyph),
            SharedString::from(label.clone()), // rules-allow: the row and the accessibility label each own the text
            div().into_any_element(),
            &p,
        );
        let trigger = self
            .disclosure_button(
                format!("{id}:heading").into(),
                &id,
                scope,
                &label,
                heading,
                cx,
            )
            .min_w_0();
        let arrow = self.disclosure_button(
            format!("{id}:caret").into(),
            &id,
            scope,
            "Show or hide the steps",
            div()
                .min_h(height::COMPACT)
                .flex()
                .items_center()
                .child(mark),
            cx,
        );
        let header = div()
            .w_full()
            .flex()
            .items_center()
            .gap(space::S)
            .child(trigger)
            .child(arrow);
        let body = self.disclosure_shows(&id).then(|| {
            div()
                .flex()
                .flex_col()
                .gap(space::XXS)
                .children(
                    transcript
                        .row_items(row)
                        .map(|entry| self.work_item(entry, session_id, scope, cx)),
                )
                .into_any_element()
        });
        div()
            .flex()
            .flex_col()
            .gap(space::S)
            .children(self.file_row(row_files(transcript, row), session_id, false, cx))
            .child(self.disclosure(&id, header, body, space::S, cx))
            .into_any_element()
    }

    /// Reasoning's first line, expanding in place to the full muted markdown. Both states
    /// hang from the top with the same inset, so the glyph and the first line stay put
    /// and only the lines after them come and go.
    pub(super) fn reasoning_line(
        &self,
        entry_id: &str,
        text: &str,
        session_id: Option<&SessionId>,
        scope: &str,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let id = SharedString::from(format!("{scope}:{entry_id}:reasoning"));
        let expanded = self.expanded.contains(id.as_str());
        let (entry, toggle_scope) = (entry_id.to_owned(), scope.to_owned());
        let line_height = Type::Body.line_height();
        let text = if expanded {
            // Scoped under the line's own id, so a fixed name stays unique.
            self.transcript_markdown("reasoning-text".into(), text.to_owned(), session_id, cx)
                .text_color(p.muted)
                .into_any_element()
        } else {
            let (summary, strong) = summary_line(text);
            let bold = HighlightStyle {
                font_weight: Some(FontWeight::BOLD),
                ..HighlightStyle::default()
            };
            let runs = strong.into_iter().map(|range| (range, bold));
            div()
                .truncate()
                .child(StyledText::new(summary).with_highlights(runs))
                .into_any_element()
        };
        let line = div()
            .min_h(height::COMPACT)
            .py((height::COMPACT - line_height) / 2.)
            .flex()
            .items_start()
            .gap(space::S)
            .typography(Type::Body)
            .text_color(p.muted)
            .cursor_pointer()
            .child(first_line(
                Type::Body,
                icon("hi.loader", &p).size(glyph::INLINE),
            ))
            .child(div().flex_1().min_w_0().child(text));
        #[cfg(feature = "snapshot")]
        let line = line.child(crate::ui::snapshot::track(&id));
        line.id(id)
            .on_click(cx.listener(move |this, _, _, cx| {
                let toggle = format!("{toggle_scope}:{entry}:reasoning");
                this.toggle_disclosure(&toggle, &toggle_scope, cx);
            }))
            .into_any_element()
    }

    /// A notice in plain muted words; an error in a softly tinted message box
    /// (TranscriptNoticeView).
    pub(super) fn notice_row(&self, entry: &TranscriptEntry) -> AnyElement {
        let p = self.palette;
        let Some(block) = entry.block() else {
            return div().into_any_element();
        };
        if block.tone == FrontendTone::Error {
            return Notice::new(
                Tone::Error,
                if block.text.is_empty() {
                    block.title.as_str()
                } else {
                    block.text.as_str()
                },
            )
            .into_any_element();
        }
        let text = if block.text.is_empty() {
            block.title.clone() // rules-allow: the line owns its text while the block keeps its title
        } else {
            format!("{} · {}", block.title, block.text)
        };
        let line = div()
            .w_full()
            .flex()
            .items_center()
            .typography(Type::Caption)
            .text_color(p.muted);
        if entry.is_pending() {
            line.child(Self::running_summary(text)).into_any_element()
        } else {
            line.child(text).into_any_element()
        }
    }

    /// One typed event on one line (EventLine): glyph, owner · headline, and its detail.
    pub(super) fn event_line(
        &self,
        entry: &TranscriptEntry,
        session: Option<&SessionId>,
        scope: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let TranscriptItemContent::Block {
            capability, block, ..
        } = &entry.content
        else {
            return div().into_any_element();
        };
        let line = BlockLine {
            key: &entry.id,
            owner: self.capability_label(capability),
            label: activity_line(entry),
            block,
            document: crate::ui::diff_document::DiffDocument::of(entry),
        };
        self.block_line(line, session, scope, cx)
    }

    /// A block on one line: glyph, owner · headline, its resources and its detail.
    pub(in crate::ui) fn block_line(
        &self,
        line: BlockLine<'_>,
        session: Option<&SessionId>,
        scope: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let block = line.block;
        let id = format!("{scope}:{}:block", line.key);
        let expanded = self.expanded.contains(&id);
        let diff = shows_diff(block);
        let detail = event_detail(block);
        let headline_color = if block.tone == FrontendTone::Neutral {
            p.text
        } else {
            p.tone(block.tone)
        };
        let trailing = if diff || !detail.is_empty() {
            chevron(expanded, &p).into_any_element()
        } else {
            div().into_any_element()
        };
        let label = SharedString::from(
            line.label.unwrap_or_else(|| block.title.clone()), // rules-allow: the line owns its label while the block keeps its title
        );
        let headline = div()
            .min_w_0()
            .truncate()
            .text_color(headline_color)
            .child(SharedString::clone(&label))
            .into_any_element();
        let heading = div()
            .min_w_0()
            .min_h(height::COMPACT)
            .flex()
            .items_center()
            .gap(space::S)
            .typography(Type::Body)
            .child(icon(event_glyph(block), &palette(cx)).size(glyph::INLINE))
            .child(div().text_color(p.accent).flex_shrink_0().child(line.owner))
            .when(!label.is_empty(), |heading| {
                heading
                    .child(div().text_color(p.muted).child("·"))
                    .child(headline)
            });
        let interactive = !detail.is_empty() || diff;
        let heading = if interactive {
            self.disclosure_button(
                format!("{id}:heading").into(),
                &id,
                scope,
                &label,
                heading,
                cx,
            )
            .min_w_0()
            .into_any_element()
        } else {
            heading.into_any_element()
        };
        let resources = self.event_resources(block, line.document, session, &id, cx);
        let line = div()
            .w_full()
            .flex()
            .items_center()
            .gap(space::S)
            .child(heading)
            .children(resources)
            .when(interactive, |line| {
                line.child(
                    self.disclosure_button(
                        format!("{id}:caret").into(),
                        &id,
                        scope,
                        "Show or hide the details",
                        div()
                            .min_h(height::COMPACT)
                            .flex()
                            .items_center()
                            .child(trailing),
                        cx,
                    ),
                )
            });
        if detail.is_empty() && !diff {
            return line.into_any_element();
        }
        let body = self.disclosure_shows(&id).then(|| {
            if diff {
                self.inline_diff(&id, &block.text, scope, cx)
            } else {
                output_body(detail, &p)
            }
        });
        self.disclosure(&id, line, body, space::XS, cx)
    }

    /// Resource actions for one event, separate from its disclosure trigger.
    fn event_resources(
        &self,
        block: &FrontendBlock,
        document: Option<&crate::ui::diff_document::DiffDocument>,
        session: Option<&SessionId>,
        scope: &str,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let document = document.filter(|document| !document.files.is_empty());
        if block.links.is_empty() && document.is_none() {
            return None;
        }
        let p = self.palette;
        let resources = div()
            .flex()
            .items_center()
            .gap(space::XS)
            .children(self.transcript_links(block.links.iter(), session, scope, cx));
        Some(
            resources
                .children(document.map(|document| {
                    let (added, removed) = (document.added(), document.removed());
                    let bound = if document.truncated { "≥" } else { "" };
                    div()
                        .id(SharedString::from(format!("{scope}:totals")))
                        .role(gpui::accesskit::Role::Group)
                        .aria_label(format!(
                            "{}{added} additions, {removed} removals",
                            if document.truncated { "At least " } else { "" }
                        ))
                        .flex()
                        .flex_shrink_0()
                        .gap(space::XS)
                        .typography(Type::Caption)
                        .child(div().text_color(p.signal).child(format!("{bound}+{added}")))
                        .child(
                            div()
                                .text_color(p.danger)
                                .child(format!("{bound}−{removed}")),
                        )
                }))
                .into_any_element(),
        )
    }

    fn capability_label(&self, capability: &str) -> String {
        self.client
            .state
            .gateway
            .as_ref()
            .and_then(|gateway| {
                gateway
                    .agent_catalog
                    .middleware_features
                    .iter()
                    .find(|feature| feature.id == capability)
            })
            .map_or_else(
                || capability.replace('_', " "),
                |feature| feature.label.clone(), // rules-allow: the line owns its label while the gateway keeps the feature
            )
    }
}

/// What one block line shows.
pub(in crate::ui) struct BlockLine<'a> {
    /// Unique within the transcript scope, for the line's disclosure.
    pub(in crate::ui) key: &'a str,
    /// Who rendered the block.
    pub(in crate::ui) owner: String,
    /// The headline in place of the block's title.
    pub(in crate::ui) label: Option<String>,
    pub(in crate::ui) block: &'a FrontendBlock,
    pub(in crate::ui) document: Option<&'a crate::ui::diff_document::DiffDocument>,
}

/// What the line hides: the block text and any observations, never markdown.
fn event_detail(block: &FrontendBlock) -> String {
    let mut text = block.text.clone(); // rules-allow: the detail grows into its own text while the block keeps its own
    let observations = block.content.text();
    if !observations.is_empty() && observations != text {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&observations);
    }
    text
}

/// A code change.
fn shows_diff(block: &FrontendBlock) -> bool {
    block.format == FrontendBlockFormat::UnifiedDiff
}

/// The files a grouped row's blocks carry.
fn row_files<'a>(
    transcript: &'a TranscriptStore,
    row: &'a TranscriptRow,
) -> impl Iterator<Item = &'a SessionFileReference> {
    transcript
        .row_items(row)
        .filter_map(TranscriptEntry::block)
        .flat_map(|block| block.files.iter().chain(block.content.files()))
}

/// What a grouped row holds: "2 thoughts · 3 tool calls".
pub(in crate::ui) fn activity_summary(counts: ActivityCounts) -> String {
    [
        (counts.thoughts, "thought", "thoughts"),
        (counts.tool_calls, "tool call", "tool calls"),
        (counts.web_searches, "web search", "web searches"),
        (counts.approvals, "approval", "approvals"),
        (counts.artifacts, "artifact", "artifacts"),
        (counts.events, "event", "events"),
        (counts.errors, "error", "errors"),
    ]
    .into_iter()
    .filter(|(count, ..)| *count > 0)
    .map(|(count, one, other)| counted(count, one, other))
    .collect::<Vec<_>>()
    .join(" · ")
}

/// The mark a step carries in its own line and, while it is the newest, in its run's header.
fn activity_glyph(entry: &TranscriptEntry) -> &'static str {
    entry.block().map_or("hi.loader", event_glyph)
}

/// What a step says it is doing: a reasoning title, or the block's title with the block
/// it is about.
pub(in crate::ui) fn activity_line(entry: &TranscriptEntry) -> Option<String> {
    match &entry.content {
        TranscriptItemContent::Assistant { text, .. } => chat::reasoning_title(text),
        TranscriptItemContent::Block { block, .. } if !block.title.is_empty() => {
            Some(match entry.group_title() {
                Some(about) => format!("{} · {about}", block.title),
                None => block.title.clone(), // rules-allow: the activity line owns its text while the block keeps its title
            })
        }
        _ => None,
    }
}

/// EventLine.glyph: error, diff, the block's own symbol, then its role.
fn event_glyph(block: &FrontendBlock) -> &'static str {
    if block.tone == FrontendTone::Error {
        return "hi.xCircle";
    }
    if shows_diff(block) {
        return "hi.fileMagnifyingGlass";
    }
    let fallback = match block.role {
        FrontendBlockRole::WebSearch => "hi.globe02",
        FrontendBlockRole::Artifact => "hi.fileMagnifyingGlass",
        FrontendBlockRole::Approval => "hi.checkCircle",
        _ => "hi.typeCursor",
    };
    symbol_icon_or(block.symbol.as_ref(), fallback)
}

/// Tool output: code type in a control card, scrolling past 320 points.
fn output_body(text: String, p: &Palette) -> AnyElement {
    div()
        .id("event-output")
        .max_h(height::LIST_MAX)
        .overflow_y_scroll()
        .px(space::S)
        .py(space::S)
        .control_corners()
        .bg(p.panel)
        .typography(Type::Code)
        .text_color(p.muted)
        .whitespace_normal()
        .child(text)
        .into_any_element()
}

/// The collapsed reasoning line (ReasoningLine.summary): the first line cut to 512
/// characters, with an ellipsis when anything was left out. A heading's marks go and
/// `**strong**` runs come back as byte ranges to embolden, as the expanded markdown
/// draws them, so the first line reads the same open or closed.
pub(super) fn summary_line(text: &str) -> (String, Vec<std::ops::Range<usize>>) {
    let first = text.lines().next().unwrap_or_default().trim();
    let plain = first.trim_start_matches('#').trim();
    let source: String = plain.chars().take(512).collect();
    let mut parts: Vec<String> = source.split("**").map(str::to_owned).collect();
    // An unmatched marker stays as written.
    if parts.len().is_multiple_of(2)
        && let Some(tail) = parts.pop()
        && let Some(last) = parts.last_mut()
    {
        last.push_str("**");
        last.push_str(&tail);
    }
    let mut summary = String::new();
    let mut strong = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        let start = summary.len();
        summary.push_str(part);
        if index % 2 == 1 && !part.is_empty() {
            strong.push(start..summary.len());
        }
    }
    if plain.chars().count() > 512 || text.trim_end().lines().count() > 1 {
        summary.push('…');
    }
    (summary, strong)
}
