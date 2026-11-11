//! The card above the composer for a frontend picker (FrontendWidgetView.swift
//! FrontendPickerView); approvals and questions are decision cards in `attention`.

use super::*;

/// The picker's card: tinted fill and hairline over the card fill, at
/// the strengths of SwiftUI's approval card.
pub(super) fn decision_card(p: &Palette, tone: Hsla) -> Div {
    div()
        .p(inset::CARD)
        .flex()
        .flex_col()
        .gap(space::S)
        .tile_corners()
        .bg(p.card.blend(tone.opacity(0.09)))
        .border_1()
        .border_color(tone.opacity(0.55))
}

impl Workspace {
    /// A frontend picker: title, dismiss and one row per option; cleared when the turn ends.
    pub(super) fn picker_card(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let (title, options) = self.client.state.picker.as_ref()?;
        let p = self.palette;
        let rows = options.iter().enumerate().map(|(index, option)| {
            let op = option.op.clone(); // rules-allow: the 'static click listener owns its copy of the option's Op
            let mut row = Row::new(("picker-option", index), option.label.clone()) // rules-allow: the row owns its label
                .leading(
                    known_symbol_icon(option.symbol.as_ref())
                        .map_or(Leading::Gutter, Leading::glyph),
                )
                .dense()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.client.state.picker = None;
                    this.submit_op(op.clone(), cx); // rules-allow: every click submits its own Op
                    cx.notify();
                }));
            let detail = &option.description;
            if !detail.is_empty() {
                row = row.subtitle(detail.clone()); // rules-allow: the row owns its subtitle
            }
            row
        });
        Some(
            decision_card(&p, p.accent)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::S)
                        .child(heading(title.clone()).flex_1()) // rules-allow: the heading owns its text while the picker stays in state
                        .child(
                            inline_button("picker-dismiss", "Dismiss", "hi.x", cx).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.client.state.picker = None;
                                    cx.notify();
                                }),
                            ),
                        ),
                )
                .child(
                    div()
                        .id("picker-options")
                        .max_h(height::LIST_MAX)
                        .overflow_y_scroll()
                        .child(Group::new().children(rows)),
                )
                .into_any_element(),
        )
    }
}
