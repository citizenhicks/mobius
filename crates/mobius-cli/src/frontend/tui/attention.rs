//! Session approvals use the same attention list as contributed decisions.

use std::sync::Arc;

use super::{TuiState, input::UiAction};
use mobius::protocol::{
    FrontendAction, FrontendActionListItem, FrontendEditor, FrontendEvent, FrontendListItemState,
    FrontendSlot, FrontendSymbol, FrontendTone, FrontendWidget, FrontendWidgetContent, Op,
    ReviewDecision,
};

impl TuiState {
    pub(super) fn sync_attention_approvals(&mut self) {
        let Some(overlay) = &mut self.capability_overlay else {
            return;
        };
        let items = self
            .approvals
            .iter()
            .map(|approval| FrontendActionListItem {
                id: Arc::clone(&approval.id),
                text: approval.reason.clone(), // rules-allow: the pending approval stays queued while the overlay owns its row
                state: FrontendListItemState::Pending,
                actions: [
                    ("Approve", ReviewDecision::Approved),
                    ("Approve for session", ReviewDecision::ApprovedForSession),
                    (
                        "Deny",
                        ReviewDecision::Denied {
                            rejection: String::new(),
                        },
                    ),
                    ("Abort", ReviewDecision::Abort),
                ]
                .into_iter()
                .map(|(label, decision)| {
                    let editor =
                        matches!(decision, ReviewDecision::Denied { .. }).then(|| FrontendEditor {
                            title: "Deny approval".into(),
                            label: "Reason".into(),
                            description: "Explain why this request is denied.".into(),
                            submit_label: "Deny".into(),
                        });
                    FrontendAction {
                        id: label.into(),
                        label: label.into(),
                        symbol: FrontendSymbol::ShieldCheck,
                        tone: FrontendTone::Neutral,
                        op: Op::ExecApproval {
                            id: Arc::clone(&approval.id),
                            decision,
                        },
                        input_from_label: false,
                        editor,
                    }
                })
                .collect(),
            })
            .collect::<Vec<_>>();
        let event = if items.is_empty() {
            FrontendEvent::RemoveWidget {
                capability: "approval".into(),
                id: "pending".into(),
            }
        } else {
            FrontendEvent::Widget {
                capability: "approval".into(),
                item: Arc::new(FrontendWidget {
                    id: "pending".into(),
                    count: None,
                    priority: mobius::protocol::FrontendPriority::Primary,
                    slot: FrontendSlot::Attention,
                    text: "Approvals".into(),
                    tone: FrontendTone::Warning,
                    symbol: Some(FrontendSymbol::ShieldCheck),
                    icon_only: false,
                    progress: None,
                    action: None,
                    content: Some(FrontendWidgetContent::ActionList {
                        title: "Approvals".into(),
                        items,
                        actions: Vec::new(),
                    }),
                }),
            }
        };
        overlay.apply(event);
    }

    pub(super) fn submit_attention_operation(&mut self, op: Op) -> UiAction {
        if let Op::ExecApproval { id, .. } = &op {
            self.approvals.retain(|approval| &approval.id != id);
            self.restore_draft();
            self.sync_attention_approvals();
        }
        UiAction::submit(op)
    }
}
