//! The changes a finished turn made, under its answer (TurnDiffCard): "N files changed",
//! the turn's totals, on demand its first files, and a row that opens the whole diff in
//! the Changes tab as "This turn".

use crate::ui::Workspace;
use crate::ui::primitives::{
    Shape as _, Type, Typography as _, chevron, counted, glyph, height, icon, palette, space,
};

use gpui::{
    AnyElement, Context, Div, InteractiveElement as _, IntoElement as _, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div,
};
use std::rc::Rc;

/// Files shown before "View more" (TurnDiffCard's `prefix(3)`).
const LISTED_FILES: usize = 3;

fn diff_row() -> Div {
    div()
        .min_h(height::REGULAR)
        .px(space::L)
        .flex()
        .items_center()
        .gap(space::S)
}

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
        let (toggle, toggle_scope) = (SharedString::clone(&id), scope.to_owned());
        let header = diff_row()
            .id(SharedString::clone(&id))
            .cursor_pointer()
            .role(gpui::accesskit::Role::Button)
            .aria_label(format!("{count}, {added} additions, {removed} removals"))
            .typography(Type::Label)
            .child(div().text_color(p.text).child(count))
            .child(div().flex().flex_shrink_0().gap(space::S).children(
                crate::ui::primitives::diff_count_labels(added, removed, "", &p),
            ))
            .child(div().flex_1())
            .child(chevron(expanded, &self.palette))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_disclosure(&toggle, &toggle_scope, cx);
            }));
        let body = expanded.then(|| self.turn_diff_body(&id, &diffs, &files, cx));
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

    fn turn_diff_body(
        &self,
        id: &SharedString,
        diffs: &[&crate::gateway::TranscriptEntry],
        files: &[&gpui_component::diff::DiffFile],
        cx: &Context<Self>,
    ) -> Div {
        let p = self.palette;
        let details = match files.len().saturating_sub(LISTED_FILES) {
            0 => "View all changes".to_owned(),
            more => format!("View {}", counted(more, "more file", "more files")),
        };
        // The Changes tab reads the turn as one patch; join only while expanded.
        let diff: Rc<str> = Rc::from(
            diffs
                .iter()
                .filter_map(|entry| entry.block())
                .map(|block| block.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        div()
            .flex()
            .flex_col()
            .children(
                files
                    .iter()
                    .take(LISTED_FILES)
                    .enumerate()
                    .map(|(index, file)| {
                        let file_diff = Rc::clone(&diff);
                        let path = file.path().to_owned();
                        diff_row()
                            .id(SharedString::from(format!("{id}:file:{index}")))
                            .cursor_pointer()
                            .role(gpui::accesskit::Role::Button)
                            .aria_label(format!("Open diff for {path}"))
                            .typography(Type::Mono)
                            .text_color(p.muted)
                            .hover(move |row| row.text_color(p.text))
                            .child(div().flex_1().min_w_0().truncate().child(path))
                            .child(div().flex().flex_shrink_0().gap(space::S).children(
                                crate::ui::primitives::diff_count_labels(
                                    file.additions(),
                                    file.deletions(),
                                    "",
                                    &p,
                                ),
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_turn_changes(&file_diff, cx);
                            }))
                    }),
            )
            .child(
                diff_row()
                    .id(SharedString::from(format!("{id}:open")))
                    .cursor_pointer()
                    .role(gpui::accesskit::Role::Button)
                    .aria_label("Opens the full diff")
                    .typography(Type::Label)
                    .text_color(p.muted)
                    .hover(move |row| row.text_color(p.text))
                    .child(div().flex_1().child(details))
                    .child(icon("hi.caretRight", &palette(cx)).size(glyph::INLINE))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_turn_changes(&diff, cx))),
            )
    }
}
