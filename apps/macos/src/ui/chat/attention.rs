//! Questions and approvals shown in the transcript and the Attention panel.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui::AnyElement;
use gpui::AppContext;
use gpui::Context;
use gpui::Entity;
use gpui::Hsla;
use gpui::InteractiveElement;
use gpui::IntoElement;
use gpui::ParentElement;
use gpui::SharedString;
use gpui::StatefulInteractiveElement;
use gpui::Styled;
use gpui::Subscription;
use gpui::Window;
use gpui::div;
use gpui_component::Disableable;
use gpui_component::carousel::{Carousel, CarouselContent, CarouselItem, CarouselState};
use gpui_component::input::InputEvent;
use gpui_component::input::TextareaState;
use mobius::protocol::ApprovalCall;
use mobius::protocol::ContentPart;
use mobius::protocol::ExecApprovalRequestEvent;
use mobius::protocol::FrontendActionListItem;
use mobius::protocol::FrontendBlock;
use mobius::protocol::FrontendBlockFormat;
use mobius::protocol::FrontendBlockRole;
use mobius::protocol::FrontendListItemState;
use mobius::protocol::FrontendSlot;
use mobius::protocol::FrontendTone;
use mobius::protocol::FrontendWidget;
use mobius::protocol::FrontendWidgetContent;
use mobius::protocol::Op;
use mobius::protocol::ReviewDecision;
use mobius_gateway::wire::TranscriptItem;
use mobius_gateway::wire::TranscriptItemContent;
use mobius_gateway::wire::TranscriptItemState;
use mobius_gateway::wire::TranscriptRow;
use mobius_gateway::wire::TranscriptRowKind;

use super::Workspace;
use crate::ui::diff_document::DiffDocument;
use crate::ui::motion;
use crate::ui::primitives::Shape;
use crate::ui::primitives::Type;
use crate::ui::primitives::Typography;
use crate::ui::primitives::choice_row;
use crate::ui::primitives::heading;
use crate::ui::primitives::icon;
use crate::ui::primitives::inline_button;
use crate::ui::primitives::inline_card;
use crate::ui::primitives::primary_button;
use crate::ui::primitives::textarea;
use crate::ui::transcript;
use crate::ui::{glyph, inset, space};

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum DecisionTarget {
    Question { widget: Arc<str>, item: Arc<str> },
    Approval { request: Arc<str> },
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct DecisionKey {
    session: String,
    target: DecisionTarget,
}

pub(super) struct DecisionDraft {
    selected: SharedString,
    editor: Option<DecisionEditor>,
}

struct DecisionEditor {
    placement: DecisionPlacement,
    input: Entity<TextareaState>,
    _subscription: Subscription,
}

struct Choice {
    id: SharedString,
    label: SharedString,
    prompt: Option<SharedString>,
}

impl Choice {
    fn approval(id: &'static str, label: &'static str, prompt: Option<&'static str>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            prompt: prompt.map(SharedString::from),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DecisionPlacement {
    Inline,
    Panel,
    Tail,
}

impl DecisionPlacement {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Panel => "panel",
            Self::Tail => "tail",
        }
    }
}

struct DecisionCard {
    key: Option<Rc<DecisionKey>>,
    title: SharedString,
    glyph: &'static str,
    tint: Option<Hsla>,
    detail: Vec<AnyElement>,
    outcome: Option<SharedString>,
    choices: Vec<Choice>,
    enabled: bool,
}

#[derive(Clone, Copy)]
enum Decision<'a> {
    Question {
        widget: &'a AttentionWidget,
        item: &'a FrontendActionListItem,
    },
    Pending(&'a ExecApprovalRequestEvent),
    Closed {
        request: &'a str,
        block: &'a FrontendBlock,
    },
}

impl Decision<'_> {
    fn id(self) -> String {
        match self {
            Self::Question { widget, item } => format!("question:{}:{}", widget.key, item.id),
            Self::Pending(approval) => format!("approval:{}", approval.id),
            Self::Closed { request, .. } => format!("approval:{request}"),
        }
    }

    fn is_pending(&self) -> bool {
        match self {
            Self::Pending(_) => true,
            Self::Question { item, .. } => item.state == FrontendListItemState::Pending,
            Self::Closed { .. } => false,
        }
    }
}

struct AttentionWidget {
    key: Arc<str>,
    widget: Arc<FrontendWidget>,
    global: bool,
}

impl AttentionWidget {
    fn items(&self) -> &[FrontendActionListItem] {
        match &self.widget.content {
            Some(FrontendWidgetContent::ActionList { items, .. }) => items,
            _ => &[],
        }
    }
}

struct QuestionAnchor {
    widget: usize,
    item: usize,
}

#[derive(Default)]
pub(super) struct AttentionIndex {
    widgets: Vec<AttentionWidget>,
    questions: BTreeMap<String, BTreeMap<Arc<str>, QuestionAnchor>>,
    pending: Vec<Arc<str>>,
    deck: Option<Entity<CarouselState>>,
    deck_ids: Vec<String>,
}

impl AttentionIndex {
    fn question(&self, entry: &TranscriptItem) -> Option<Decision<'_>> {
        if entry.inherited {
            return None;
        }
        let TranscriptItemContent::Block {
            capability, block, ..
        } = &entry.content
        else {
            return None;
        };
        let call = block
            .id
            .as_deref()?
            .strip_prefix(entry.turn_id.as_deref()?)?
            .strip_prefix('/')?;
        let anchor = self.questions.get(capability.as_ref())?.get(call)?;
        let widget = self.widgets.get(anchor.widget)?;
        Some(Decision::Question {
            widget,
            item: widget.items().get(anchor.item)?,
        })
    }

    fn questions(&self) -> impl Iterator<Item = Decision<'_>> {
        self.widgets.iter().flat_map(|widget| {
            widget
                .items()
                .iter()
                .map(move |item| Decision::Question { widget, item })
        })
    }
}

struct ApprovalAnchor<'a> {
    request: &'a str,
    block: &'a FrontendBlock,
}

fn approval_anchor(item: &TranscriptItem) -> Option<ApprovalAnchor<'_>> {
    let TranscriptItemContent::Block { block, .. } = &item.content else {
        return None;
    };
    if block.role != FrontendBlockRole::Approval {
        return None;
    }
    let (_, request) = item.id.rsplit_once("/approval:")?;
    Some(ApprovalAnchor { request, block })
}

impl Workspace {
    pub(super) fn attention_count(&self) -> usize {
        self.pending_approvals().len()
            + self.chat_ui.attention.questions().filter(|question| {
                matches!(question, Decision::Question { item, .. } if item.state == FrontendListItemState::Pending)
            }).count()
    }

    pub(super) fn attention_has_items(&self) -> bool {
        !self.pending_approvals().is_empty() || self.chat_ui.attention.questions().next().is_some()
    }

    pub(super) fn pending_approvals(&self) -> &[ExecApprovalRequestEvent] {
        self.client
            .state
            .session
            .as_ref()
            .map_or(&[], |session| session.pending_approvals.as_slice())
    }

    fn decision_session(&self) -> String {
        self.client.draft_key(
            self.client
                .state
                .selected_session_id
                .as_deref()
                .unwrap_or_default(),
        )
    }

    fn refresh_attention_widgets(&mut self) -> bool {
        let sources = self.slot_widget_items(FrontendSlot::Attention);
        let attention = &self.chat_ui.attention;
        let widgets_changed = sources.len() != attention.widgets.len()
            || !sources.iter().zip(&attention.widgets).all(
                |((key, (widget, global)), previous)| {
                    key == &*previous.key
                        && *global == previous.global
                        && Arc::ptr_eq(widget, &previous.widget)
                },
            );
        if widgets_changed {
            let widgets: Vec<_> = sources
                .into_iter()
                .map(|(key, (widget, global))| AttentionWidget {
                    key: key.into(),
                    widget: Arc::clone(widget),
                    global,
                })
                .collect();
            let mut questions: BTreeMap<String, BTreeMap<Arc<str>, QuestionAnchor>> =
                BTreeMap::new();
            for (widget, source) in widgets.iter().enumerate() {
                let Some((capability, _)) = source.key.split_once(':') else {
                    continue;
                };
                let anchors = questions.entry(capability.to_owned()).or_default();
                for (item, question) in source.items().iter().enumerate() {
                    anchors
                        .entry(Arc::clone(&question.id))
                        .or_insert(QuestionAnchor { widget, item });
                }
            }
            self.chat_ui.attention.widgets = widgets;
            self.chat_ui.attention.questions = questions;
        }
        widgets_changed
    }

    pub(super) fn refresh_decisions(&mut self, cx: &mut Context<Self>) {
        let widgets_changed = self.refresh_attention_widgets();
        let pending_changed = !self
            .pending_approvals()
            .iter()
            .map(|approval| &approval.id)
            .eq(&self.chat_ui.attention.pending);
        if pending_changed {
            self.chat_ui.attention.pending = self
                .pending_approvals()
                .iter()
                .map(|approval| Arc::clone(&approval.id))
                .collect();
        }
        if widgets_changed || pending_changed {
            let ids: Vec<String> = self.pending_decisions().map(Decision::id).collect();
            let deck = self
                .chat_ui
                .attention
                .deck
                .get_or_insert_with(|| cx.new(|_| CarouselState::new(0)));
            let selection = deck
                .read(cx)
                .selected_index()
                .and_then(|index| self.chat_ui.attention.deck_ids.get(index))
                .and_then(|id| ids.iter().position(|candidate| candidate == id));
            deck.update(cx, |deck, cx| {
                deck.set_item_count(ids.len(), cx);
                if let Some(index) = selection {
                    deck.set_selected_index(index, cx);
                }
            });
            self.chat_ui.attention.deck_ids = ids;
        }
        if (widgets_changed || pending_changed) && self.active_session_ready() {
            let scope = self.decision_session();
            let attention = &self.chat_ui.attention;
            let requests = &mut self.chat_ui.action_requests;
            self.chat_ui.decision_drafts.retain(|key, _| {
                if key.session != scope {
                    return true;
                }
                match &key.target {
                    DecisionTarget::Approval { request } => {
                        let pending = attention.pending.contains(request);
                        if !pending {
                            requests.remove(&format!("approval\0{request}"));
                        }
                        pending
                    }
                    DecisionTarget::Question { widget, item } => attention
                        .widgets
                        .iter()
                        .find(|source| &source.key == widget)
                        .and_then(|source| {
                            source.items().iter().find(|question| &question.id == item)
                        })
                        .is_none_or(|question| question.state == FrontendListItemState::Pending),
                }
            });
        }
    }

    fn transcript_items(&self) -> impl DoubleEndedIterator<Item = &TranscriptItem> {
        let transcript = &self.client.state.transcript;
        transcript
            .rows()
            .flat_map(move |row| transcript.row_items(row))
            .map(|entry| &**entry)
    }

    fn approval_at<'a>(&'a self, item: &'a TranscriptItem) -> Option<Decision<'a>> {
        let anchor = approval_anchor(item)?;
        if item.state == TranscriptItemState::Complete {
            return Some(Decision::Closed {
                request: if item.inherited {
                    &item.id
                } else {
                    anchor.request
                },
                block: anchor.block,
            });
        }
        if item.inherited {
            return None;
        }
        self.pending_approvals()
            .iter()
            .find(|approval| &*approval.id == anchor.request)
            .map(Decision::Pending)
    }

    pub(super) fn attention_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.refresh_decisions(cx);
        let mut questions: Vec<_> = self.chat_ui.attention.questions().collect();
        questions.sort_by_key(|decision| {
            matches!(decision, Decision::Question { item, .. } if item.state != FrontendListItemState::Pending)
        });
        let cards: Vec<_> = self
            .pending_approvals()
            .iter()
            .map(Decision::Pending)
            .chain(questions)
            .chain(
                self.transcript_items()
                    .filter_map(|item| self.approval_at(item))
                    .filter(|decision| matches!(decision, Decision::Closed { .. })),
            )
            .map(|decision| self.decision(decision, DecisionPlacement::Panel, cx))
            .collect();
        div()
            .id("attention-list")
            .p(inset::CARD)
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(space::S)
            .children(cards)
            .into_any_element()
    }

    /// Render a row's decisions and omit its source only if every item has a replacement.
    pub(super) fn row_decisions(
        &self,
        row: &TranscriptRow,
        cx: &mut Context<Self>,
    ) -> RowDecisions {
        let mut has_source = !matches!(row.kind, TranscriptRowKind::Activity { .. });
        let mut cards = Vec::new();
        for entry in self.client.state.transcript.row_items(row) {
            if let Some(decision) = self
                .approval_at(entry)
                .or_else(|| self.chat_ui.attention.question(entry))
                .filter(|decision| !decision.is_pending())
            {
                cards.push(self.decision(decision, DecisionPlacement::Inline, cx));
            } else {
                has_source = true;
            }
        }
        RowDecisions {
            show_source: has_source || cards.is_empty(),
            cards,
        }
    }

    fn pending_decisions(&self) -> impl Iterator<Item = Decision<'_>> {
        self.pending_approvals()
            .iter()
            .map(Decision::Pending)
            .chain(
                self.chat_ui
                    .attention
                    .questions()
                    .filter(Decision::is_pending),
            )
    }

    pub(super) fn has_pending_decisions(&self) -> bool {
        !self.chat_ui.attention.deck_ids.is_empty()
    }

    pub(super) fn decision_deck(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.chat_ui.attention.deck.as_ref()?;
        let cards: Vec<_> = self
            .pending_decisions()
            .enumerate()
            .map(|(index, decision)| {
                CarouselItem::new(decision.id(), index, state).child(self.decision(
                    decision,
                    DecisionPlacement::Tail,
                    cx,
                ))
            })
            .collect();
        let mut deck = Carousel::new("pending-decisions", state)
            .accessibility_label("Pending decisions")
            .w_full()
            .gap(space::S)
            .child(CarouselContent::new(state).children(cards));
        if self.chat_ui.attention.deck_ids.len() > 1 {
            let previous = Entity::clone(state);
            let next = Entity::clone(state);
            deck = deck.child(
                div()
                    .flex()
                    .justify_center()
                    .gap(space::S)
                    .child(
                        inline_button("decision-previous", "Previous", "hi.arrowLeft", cx)
                            .disabled(!state.read(cx).has_previous())
                            .on_click(move |_, _, cx| {
                                previous.update(cx, |state, cx| {
                                    state.select_previous(cx);
                                });
                            }),
                    )
                    .child(
                        inline_button("decision-next", "Next", "hi.arrowRight", cx)
                            .disabled(!state.read(cx).has_next())
                            .on_click(move |_, _, cx| {
                                next.update(cx, |state, cx| {
                                    state.select_next(cx);
                                });
                            }),
                    ),
            );
        }
        Some(
            div()
                .mx_auto()
                .w_full()
                .max_w(crate::ui::width::TRANSCRIPT)
                .child(deck)
                .into_any_element(),
        )
    }

    fn decision(
        &self,
        decision: Decision<'_>,
        placement: DecisionPlacement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let identity = decision.id();
        let card = match decision {
            Decision::Question { widget, item } => self.question_card(widget, item),
            Decision::Pending(approval) => self.approval_decision(approval, cx),
            Decision::Closed { block, .. } => self.closed_approval(block),
        };
        let id = format!("{}:{identity}", placement.as_str());
        let card = self.decision_card(card, &id, placement, cx);
        motion::rise_in(
            div().w_full().child(card),
            id,
            motion::REVEAL,
            space::XS,
            cx,
        )
    }

    fn question_card(
        &self,
        widget: &AttentionWidget,
        item: &FrontendActionListItem,
    ) -> DecisionCard {
        let key = self.decision_key(DecisionTarget::Question {
            widget: Arc::clone(&widget.key),
            item: Arc::clone(&item.id),
        });
        let outcome = item
            .outcome
            .as_deref()
            .map(SharedString::from)
            .or_else(|| item.actions.is_empty().then(|| "Done".into()));
        let choices = item
            .actions
            .iter()
            .map(|action| Choice {
                id: SharedString::from(action.id.as_str()),
                label: SharedString::from(
                    action
                        .label
                        .strip_suffix(" (Recommended)")
                        .unwrap_or(&action.label),
                ),
                prompt: action
                    .editor
                    .as_ref()
                    .map(|editor| SharedString::from(editor.label.as_str())),
            })
            .collect();
        DecisionCard {
            key: Some(key),
            title: SharedString::from(item.text.as_str()),
            glyph: "hi.userQuestion01",
            tint: None,
            detail: Vec::new(),
            outcome,
            choices,
            enabled: self.client.link.is_ready()
                && !item
                    .actions
                    .iter()
                    .any(|action| self.widget_action_blocked(action)),
        }
    }

    fn approval_decision(
        &self,
        approval: &ExecApprovalRequestEvent,
        cx: &mut Context<Self>,
    ) -> DecisionCard {
        let key = self.decision_key(DecisionTarget::Approval {
            request: Arc::clone(&approval.id),
        });
        let in_flight = self
            .chat_ui
            .action_requests
            .contains_key(&format!("approval\0{}", approval.id));
        let mut detail = self.decision_detail(&approval.reason);
        detail.extend(
            approval
                .calls
                .iter()
                .map(|call| self.approval_call(&approval.id, call, cx)),
        );
        DecisionCard {
            key: Some(key),
            title: "Allow this command?".into(),
            glyph: "hi.shieldCheck",
            tint: Some(self.palette.warning),
            detail,
            outcome: None,
            choices: vec![
                Choice::approval("once", "Allow once", None),
                Choice::approval("session", "Allow for this chat", None),
                Choice::approval(
                    "deny",
                    "Deny and tell möbius why",
                    Some("Why not, or what to do instead"),
                ),
                Choice::approval("abort", "Stop this turn", None),
            ],
            enabled: !in_flight && self.client.link.is_ready(),
        }
    }

    fn closed_approval(&self, block: &FrontendBlock) -> DecisionCard {
        let outcome = match block.content.0.first() {
            Some(ContentPart::Text { text }) => format!("{}: {text}", block.title).into(),
            _ => SharedString::from(block.title.as_str()),
        };
        DecisionCard {
            key: None,
            title: "Allow this command?".into(),
            glyph: "hi.shieldCheck",
            tint: None,
            detail: self.decision_detail(&block.text),
            outcome: Some(outcome),
            choices: Vec::new(),
            enabled: false,
        }
    }

    fn decision_detail(&self, text: &str) -> Vec<AnyElement> {
        if text.is_empty() {
            return Vec::new();
        }
        vec![
            div()
                .typography(Type::Body)
                .text_color(self.palette.muted)
                .whitespace_normal()
                .child(SharedString::from(text))
                .into_any_element(),
        ]
    }

    fn decision_key(&self, target: DecisionTarget) -> Rc<DecisionKey> {
        Rc::new(DecisionKey {
            session: self.decision_session(),
            target,
        })
    }

    fn decision_card(
        &self,
        card: DecisionCard,
        id: &str,
        placement: DecisionPlacement,
        cx: &Context<Self>,
    ) -> AnyElement {
        let DecisionCard {
            key,
            title,
            glyph: glyph_name,
            tint,
            detail,
            outcome,
            choices,
            enabled,
        } = card;
        let p = self.palette;
        let title = heading(title).whitespace_normal().into_any_element();
        let mut body = detail;
        if let Some(outcome) = outcome {
            body.push(motion::fade_in(
                div()
                    .flex()
                    .items_start()
                    .gap(space::S)
                    .typography(Type::Body)
                    .text_color(p.muted)
                    .child(
                        icon("hi.check", &p)
                            .size(glyph::INLINE)
                            .text_color(p.signal),
                    )
                    .child(div().flex_1().whitespace_normal().child(outcome)),
                SharedString::from(format!("{id}:outcome")),
                motion::CROSSFADE,
                cx,
            ));
            return inline_card(Some(title), body, glyph_name, None, None, &p).into_any_element();
        }
        let Some(key) = key else {
            return inline_card(Some(title), body, glyph_name, tint, None, &p).into_any_element();
        };
        let draft = self.chat_ui.decision_drafts.get(&key);
        let selected =
            draft.and_then(|draft| choices.iter().find(|choice| choice.id == draft.selected));
        let editor = draft
            .and_then(|draft| draft.editor.as_ref())
            .filter(|editor| editor.placement == placement);
        let needs_text = selected.is_some_and(|choice| choice.prompt.is_some());
        let can_send = enabled
            && selected.is_some_and(|choice| {
                choice.prompt.is_none()
                    || editor.is_some_and(|editor| !editor.input.read(cx).value().trim().is_empty())
            });
        for (index, choice) in choices.into_iter().enumerate() {
            let selected = draft.is_some_and(|draft| draft.selected == choice.id);
            let key = Rc::clone(&key);
            body.push(
                choice_row(
                    SharedString::from(format!("{id}:choice:{index}")),
                    choice.label,
                    selected,
                    enabled,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.choose(
                        Rc::clone(&key),
                        SharedString::clone(&choice.id),
                        choice.prompt.as_ref(),
                        placement,
                        window,
                        cx,
                    );
                }))
                .into_any_element(),
            );
        }
        if needs_text && let Some(editor) = editor {
            body.push(motion::rise_in(
                div().w_full().child(textarea(&editor.input).w_full()),
                SharedString::from(format!("{id}:field")),
                motion::STANDARD,
                space::XS,
                cx,
            ));
        }
        let send = primary_button(
            SharedString::from(format!("{id}:send")),
            "Send",
            "hi.arrowUp02",
            cx,
        )
        .disabled(!can_send)
        .on_click(cx.listener(move |this, _, window, cx| this.send_decision(&key, window, cx)));
        inline_card(
            Some(title),
            body,
            glyph_name,
            tint,
            Some(send.into_any_element()),
            &p,
        )
        .into_any_element()
    }

    fn choose(
        &mut self,
        key: Rc<DecisionKey>,
        selected: SharedString,
        prompt: Option<&SharedString>,
        placement: DecisionPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if key.session != self.decision_session() {
            return;
        }
        let draft = self
            .chat_ui
            .decision_drafts
            .entry(key)
            .or_insert_with(|| DecisionDraft {
                selected: SharedString::clone(&selected),
                editor: None,
            });
        draft.selected = selected;
        if let Some(prompt) = prompt {
            let editor = draft.editor.get_or_insert_with(|| {
                let prompt = SharedString::clone(prompt);
                let input = cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .placeholder(prompt)
                        .auto_grow(2, 5)
                });
                let subscription = cx.subscribe(&input, |_, _, _: &InputEvent, cx| cx.notify());
                DecisionEditor {
                    placement,
                    input,
                    _subscription: subscription,
                }
            });
            editor.placement = placement;
            editor.input.update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
    }

    fn send_decision(&mut self, key: &DecisionKey, window: &mut Window, cx: &mut Context<Self>) {
        if key.session != self.decision_session() {
            return;
        }
        let Some(draft) = self.chat_ui.decision_drafts.get(key) else {
            return;
        };
        let selected = SharedString::clone(&draft.selected);
        let text = draft
            .editor
            .as_ref()
            .map_or_default(|editor| editor.input.read(cx).value().trim().to_owned());
        match &key.target {
            DecisionTarget::Approval { request } => {
                if !self
                    .pending_approvals()
                    .iter()
                    .any(|approval| &approval.id == request)
                {
                    return;
                }
                let decision = match selected.as_ref() {
                    "once" => ReviewDecision::Approved,
                    "session" => ReviewDecision::ApprovedForSession,
                    "deny" if !text.is_empty() => ReviewDecision::Denied { rejection: text },
                    "abort" => ReviewDecision::Abort,
                    _ => return,
                };
                self.dispatch_widget_operation(
                    Op::ExecApproval {
                        id: Arc::clone(request),
                        decision,
                    },
                    false,
                    cx,
                );
            }
            DecisionTarget::Question { widget, item } => {
                let widgets = self.slot_widget_items(FrontendSlot::Attention);
                let Some((widget, global)) = widgets.get(widget.as_ref()) else {
                    return;
                };
                let Some(FrontendWidgetContent::ActionList { items, .. }) = &widget.content else {
                    return;
                };
                let Some(action) =
                    items
                        .iter()
                        .find(|question| &question.id == item)
                        .and_then(|question| {
                            question
                                .actions
                                .iter()
                                .find(|action| action.id == selected.as_ref())
                        })
                else {
                    return;
                };
                let typed = action.editor.is_some();
                if typed && text.is_empty() {
                    return;
                }
                let global = *global;
                if !typed && !action.input_from_label && action.tone == FrontendTone::Error {
                    let action = action.clone(); // rules-allow: a confirmation owns the action after dispatch
                    self.activate_widget_action(action, global, window, cx);
                    return;
                }
                let mut op = action.op.clone(); // rules-allow: submitting owns its operation while the widget remains mounted
                if let Op::CapabilityCommand { input, .. } = &mut op {
                    if typed {
                        *input = Some(text);
                    } else if action.input_from_label {
                        *input = Some(action.label.clone()); // rules-allow: the submitted operation owns its answer
                    }
                }
                self.dispatch_widget_operation(op, global, cx);
            }
        }
    }

    fn approval_call(
        &self,
        approval: &str,
        call: &ApprovalCall,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(block) = &call.preview else {
            return div()
                .p(space::S)
                .control_corners()
                .bg(self.palette.inset)
                .typography(Type::Mono)
                .child(SharedString::from(&call.name))
                .into_any_element();
        };
        let document = (block.format == FrontendBlockFormat::UnifiedDiff)
            .then(|| DiffDocument::parse(&block.text));
        let key = format!("{approval}:{}", call.call_id);
        self.block_line(
            transcript::BlockLine {
                key: &key,
                owner: (*call.name).to_owned(),
                label: None,
                block,
                document: document.as_ref(),
            },
            None,
            "approval",
            cx,
        )
    }
}

pub(super) struct RowDecisions {
    pub(super) show_source: bool,
    pub(super) cards: Vec<AnyElement>,
}
