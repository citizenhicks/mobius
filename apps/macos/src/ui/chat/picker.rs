//! Cards above the composer that wait for a decision: tool approvals (ComposerView.swift
//! ApprovalView) and frontend pickers (FrontendWidgetView.swift FrontendPickerView).

use super::*;
use mobius::SessionId;
use mobius::protocol::ReviewDecision;

/// The card shared by approvals and pickers: tinted fill and hairline over the card fill, at
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
    fn deny_approval(&self, id: Arc<str>, window: &mut Window, cx: &mut Context<Self>) {
        let session = self
            .client
            .state
            .selected_session_id
            .as_ref()
            .map(SessionId::share);
        self.edit_text(
            TextEdit::new("Deny approval", "")
                .ok_label("Deny")
                .kind(EditKind::Multiline { min: 3, max: 10 }),
            window,
            cx,
            move |this, rejection, _, cx| {
                if this.client.state.selected_session_id != session || rejection.trim().is_empty() {
                    return;
                }
                this.dispatch_widget_operation(
                    Op::ExecApproval {
                        id: Arc::clone(&id),
                        decision: ReviewDecision::Denied { rejection },
                    },
                    false,
                    cx,
                );
            },
        );
    }

    /// The selected pending approval with its calls and four decisions.
    pub(super) fn approval_card(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let approvals = &self.client.state.session.as_ref()?.pending_approvals;
        let approval = approvals.iter().find(|approval| {
            self.chat_ui.attention_item.as_deref() == Some(approval.id.as_ref())
        })?;
        let p = self.palette;
        let in_flight = self
            .chat_ui
            .action_requests
            .contains_key(&format!("approval\0{}", approval.id));
        let decisions: [(&str, &str, ReviewDecision); 4] = [
            ("abort", "Abort", ReviewDecision::Abort),
            (
                "deny",
                "Deny",
                ReviewDecision::Denied {
                    rejection: "Denied in Möbius desktop".into(),
                },
            ),
            (
                "session",
                "Approve for session",
                ReviewDecision::ApprovedForSession,
            ),
            ("once", "Approve once", ReviewDecision::Approved),
        ];
        let actions = decisions.into_iter().map(|(key, label, decision)| {
            let button =
                button(SharedString::from(format!("approval-{key}")), label).disabled(in_flight);
            let button = match decision {
                ReviewDecision::Abort => button.danger(),
                ReviewDecision::Approved => button.primary(),
                _ => button.outline(),
            };
            let id = Arc::clone(&approval.id);
            button.on_click(cx.listener(move |this, _, window, cx| {
                if matches!(decision, ReviewDecision::Denied { .. }) {
                    this.deny_approval(Arc::clone(&id), window, cx);
                    return;
                }
                this.dispatch_widget_operation(
                    Op::ExecApproval {
                        id: Arc::clone(&id),
                        decision: decision.clone(), // rules-allow: every click sends its own decision
                    },
                    false,
                    cx,
                );
            }))
        });
        let actions: Vec<_> = actions.collect();
        let calls: Vec<_> = approval
            .calls
            .iter()
            .map(|call| self.approval_call(&approval.id, call, cx))
            .collect();
        Some(
            decision_card(&p, p.warning)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::S)
                        .text_color(p.warning)
                        .child(icon("hi.userRoundCheck", &palette(cx)).size(glyph::LEAD))
                        .child(heading("Approval required").flex_1())
                        .when(approvals.len() > 1, |title| {
                            title.child(
                                div()
                                    .typography(Type::Caption)
                                    .text_color(p.muted)
                                    .child(format!("1 of {}", approvals.len())),
                            )
                        }),
                )
                .child(div().typography(Type::Body).whitespace_normal().child(
                    if approval.reason.is_empty() {
                        "Möbius needs permission to continue.".to_owned()
                    } else {
                        approval.reason.clone() // rules-allow: the card owns its text while the approval stays in session state
                    },
                ))
                .child(
                    div()
                        .id("approval-calls")
                        .max_h(height::LIST_MAX)
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap(space::S)
                        .children(calls),
                )
                .child(button_row().flex_wrap().children(actions))
                .into_any_element(),
        )
    }

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

impl Workspace {
    /// One call awaiting approval as its tool renders it, or its name when the tool
    /// renders nothing.
    fn approval_call(
        &self,
        approval: &str,
        call: &mobius::protocol::ApprovalCall,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let Some(block) = &call.preview else {
            return div()
                .p(space::S)
                .control_corners()
                .bg(p.raised)
                .typography(Type::Subhead)
                .child(SharedString::from(&call.name))
                .into_any_element();
        };
        let document = (block.format == mobius::protocol::FrontendBlockFormat::UnifiedDiff)
            .then(|| crate::ui::diff_document::DiffDocument::parse(&block.text));
        let key = format!("{approval}:{}", call.call_id);
        let line = transcript::BlockLine {
            key: &key,
            owner: (*call.name).to_owned(),
            label: None,
            block,
            document: document.as_ref(),
        };
        self.block_line(line, None, "approval", cx)
    }
}
