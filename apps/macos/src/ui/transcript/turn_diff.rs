//! The changes a finished turn made, under its answer (TurnDiffCard): "N files changed",
//! the turn's totals, on demand its first files, and a row that opens the whole diff in
//! the Changes tab as "This turn".

use super::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Shape, SharedString,
    StatefulInteractiveElement, Styled, Type, Typography, Workspace, chevron, counted, div, glyph,
    height, icon, palette, space,
};

/// Files shown before "View more" (TurnDiffCard's `prefix(3)`).
const LISTED_FILES: usize = 3;

impl Workspace {
    /// The card under a turn's answer; one click lists its first files, the last row
    /// opens every change beside the chat (SwiftUI presents WorkspaceDiffView).
    pub(super) fn turn_diff_card(
        &self,
        key: &str,
        transcript: &crate::gateway::TranscriptStore,
        changes: &mobius_gateway::wire::TurnChanges,
        scope: &str,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let diffs: Vec<_> = transcript.diffs(&changes.diffs).collect();
        let files: Vec<_> = diffs
            .iter()
            .filter_map(|entry| crate::ui::diff_document::DiffDocument::of(entry))
            .flat_map(|document| &document.files)
            .collect();
        if files.is_empty() {
            return None;
        }
        let p = self.palette;
        let id = SharedString::from(format!("{key}:turn-diff"));
        let expanded = self.expanded.contains(id.as_ref());
        let (added, removed) = (changes.totals.additions, changes.totals.deletions);
        let count = format!("{} changed", counted(files.len(), "file", "files"));
        let totals = |added: &dyn std::fmt::Display, removed: &dyn std::fmt::Display| {
            div()
                .flex()
                .flex_shrink_0()
                .gap(space::S)
                .child(div().text_color(p.signal).child(format!("+{added}")))
                .child(div().text_color(p.danger).child(format!("−{removed}")))
        };
        let row = || {
            div()
                .min_h(height::REGULAR)
                .px(space::L)
                .flex()
                .items_center()
                .gap(space::S)
        };
        let (toggle, toggle_scope) = (SharedString::clone(&id), scope.to_owned());
        let header = row()
            .id(SharedString::clone(&id))
            .cursor_pointer()
            .role(gpui::accesskit::Role::Button)
            .aria_label(format!("{count}, {added} additions, {removed} removals"))
            .typography(Type::Label)
            .child(div().text_color(p.text).child(count))
            .child(totals(&added, &removed))
            .child(div().flex_1())
            .child(chevron(expanded, &self.palette))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_disclosure(&toggle, &toggle_scope, cx);
            }));
        let details = match files.len().saturating_sub(LISTED_FILES) {
            0 => "View all changes".to_owned(),
            more => format!("View {}", counted(more, "more file", "more files")),
        };
        // The Changes tab reads the turn as one patch.
        let diff = diffs
            .iter()
            .filter_map(|entry| entry.block())
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let body = expanded.then(|| {
            div()
                .flex()
                .flex_col()
                .children(files.iter().take(LISTED_FILES).map(|file| {
                    row()
                        .typography(Type::Mono)
                        .text_color(p.muted)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(file.path().to_string()),
                        )
                        .child(totals(&file.additions(), &file.deletions()))
                }))
                .child(
                    row()
                        .id(SharedString::from(format!("{id}:open")))
                        .cursor_pointer()
                        .role(gpui::accesskit::Role::Button)
                        .aria_label("Opens the full diff")
                        .typography(Type::Label)
                        .text_color(p.muted)
                        .hover(move |row| row.text_color(p.text))
                        .child(div().flex_1().child(details))
                        .child(icon("hi.caretRight", &palette(cx)).size(glyph::INLINE))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.open_turn_changes(&diff, cx)),
                        ),
                )
        });
        Some(
            div()
                .mt(space::S)
                .tile_corners()
                // The page itself is card-coloured, so the summary takes the panel step.
                .bg(p.panel)
                .flex()
                .flex_col()
                .child(header)
                .children(body)
                .into_any_element(),
        )
    }
}
