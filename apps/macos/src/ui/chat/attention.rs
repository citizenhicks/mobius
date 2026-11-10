//! Approval and contributed attention rows share the existing list and decision form.
use super::*;
use mobius::protocol::{FrontendAction, FrontendListItemState, FrontendWidgetContent};

impl Workspace {
    pub(super) fn attention_count(&self) -> usize {
        self.client
            .state
            .session
            .as_ref()
            .map_or(0, |session| session.pending_approvals.len())
            + self
                .slot_widget_items(FrontendSlot::Attention)
                .values()
                .flat_map(|(widget, _)| widget.pending_attention_items())
                .count()
    }

    pub(super) fn attention_has_items(&self) -> bool {
        self.client.state.session.as_ref().is_some_and(|session| !session.pending_approvals.is_empty()) || self.slot_widget_items(FrontendSlot::Attention).values().any(|(widget, _)| {
            matches!(&widget.content, Some(FrontendWidgetContent::ActionList { items, .. }) if !items.is_empty())
        })
    }

    pub(super) fn attention_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let mut rows = Vec::new();
        if let Some(session) = &self.client.state.session {
            for approval in &session.pending_approvals {
                let id = Arc::clone(&approval.id);
                let open = self.chat_ui.attention_item.as_ref() == Some(&approval.id);
                rows.push(
                    Row::new(
                        SharedString::from(format!("approval:{id}")),
                        "Approval required",
                    )
                    .leading(Leading::glyph("hi.userRoundCheck"))
                    .status(p.warning, "Waiting for you")
                    .subtitle(approval.reason.clone()) // rules-allow: the row owns its text while the approval stays in session state
                    .control(attention_caret(&p, open))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_attention_item(Arc::clone(&id), cx);
                    }))
                    .into_any_element(),
                );
                if open {
                    rows.extend(self.approval_card(cx));
                }
            }
        }
        let widgets = self.slot_widget_items(FrontendSlot::Attention);
        let mut items: Vec<_> = widgets
            .iter()
            .flat_map(|(key, (widget, global))| {
                let items = match &widget.content {
                    Some(FrontendWidgetContent::ActionList { items, .. }) => items.as_slice(),
                    _ => &[],
                };
                items.iter().map(move |item| (key, item, *global))
            })
            .collect();
        items.sort_by_key(|(_, item, _)| item.state != FrontendListItemState::Pending);
        for (key, item, global) in items {
            let id: Arc<str> = format!("{key}:{}", item.id).into();
            let pending = item.state == FrontendListItemState::Pending;
            let selected = self.chat_ui.attention_item.as_ref() == Some(&id);
            // An answered item reads "question\noutcome": the outcome becomes its status.
            let (question, outcome) = item.text.split_once('\n').unwrap_or((&item.text, ""));
            let row = Row::new(SharedString::from(&id), question.to_owned())
                .leading(Leading::glyph("hi.userQuestion01"));
            let row = if pending {
                let id = Arc::clone(&id);
                row.status(p.warning, "Waiting for you")
                    .control(attention_caret(&p, selected))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_attention_item(Arc::clone(&id), cx);
                    }))
            } else {
                row.status(
                    p.signal,
                    if outcome.is_empty() { "Done" } else { outcome }.to_owned(),
                )
            };
            // The options join their question's row, so the list draws no line between them.
            let options = (pending && selected)
                .then(|| self.attention_options(key, &id, &item.actions, global, cx));
            rows.push(div().child(row).children(options).into_any_element());
        }
        div()
            .id("attention-list")
            .p(inset::CARD)
            .overflow_y_scroll()
            .child(Group::new().children(rows))
            .into_any_element()
    }

    fn open_attention_item(&mut self, id: Arc<str>, cx: &mut Context<Self>) {
        self.chat_ui.attention_editor = None;
        self.chat_ui.attention_item = Some(id);
        cx.notify();
    }

    /// A question's answers under its row: choices or a written reply, then anything else,
    /// such as Skip, as a quiet trailing button.
    fn attention_options(
        &self,
        key: &str,
        id: &str,
        actions: &[FrontendAction],
        global: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let (choices, others): (Vec<_>, Vec<_>) = actions
            .iter()
            .partition(|action| action.input_from_label || action.editor.is_some());
        // Questions list the recommended answer first.
        let recommended = choices
            .iter()
            .find(|action| action.input_from_label)
            .map(|action| &action.id);
        let others = others.into_iter().map(|action| {
            let action = action.clone(); // rules-allow: the 'static click listener owns its copy of the action
            button(
                SharedString::from(format!("attention-other:{key}:{}", action.id)),
                action.label.clone(), // rules-allow: the button owns its label while the listener keeps the action
            )
            .ghost()
            .text_color(p.muted)
            .disabled(self.widget_action_blocked(&action))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.activate_widget_action(action.clone(), global, window, cx); // rules-allow: every click activates its own copy
            }))
        });
        let others: Vec<_> = others.collect();
        motion::rise_in(
            div()
                .flex()
                .flex_col()
                .gap(space::XS)
                .pl(space::L + glyph::GUTTER + space::S)
                .pr(space::L)
                .pb(space::S)
                .children(choices.iter().map(|action| {
                    self.attention_action(key, action, recommended == Some(&action.id), global, cx)
                }))
                .when(!others.is_empty(), |options| {
                    options.child(button_row().children(others))
                }),
            SharedString::from(format!("attention-options:{id}")),
            motion::STANDARD,
            px(4.),
            cx,
        )
    }

    /// One answer to a question: a left-aligned choice, the recommended one tagged.
    fn attention_option(
        &self,
        id: impl Into<ElementId>,
        glyph_name: &str,
        action: &FrontendAction,
        recommended: bool,
        blocked: bool,
    ) -> Stateful<Div> {
        let p = self.palette;
        let label = action
            .label
            .strip_suffix(" (Recommended)")
            .unwrap_or(&action.label);
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(space::S)
            .min_h(height::REGULAR)
            .py(space::XS)
            .px(space::S)
            .control_corners()
            .border_1()
            .border_color(p.separator)
            .child(icon(glyph_name, &p).size(glyph::INLINE).text_color(p.muted))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .typography(Type::Body)
                    .text_color(p.tone(action.tone))
                    .child(label.to_owned()),
            )
            .when(recommended, |option| {
                option.child(
                    div()
                        .px(space::S)
                        .pill_corners()
                        .bg(p.accent.opacity(alpha::TINT))
                        .typography(Type::Caption)
                        .text_color(p.accent)
                        .child("Recommended"),
                )
            })
            .when(blocked, |option| option.opacity(alpha::DIM))
            .when(!blocked, |option| {
                option
                    .cursor_pointer()
                    .hover(|style| style.bg(p.hover).border_color(p.accent.opacity(alpha::DIM)))
            })
    }

    fn attention_action(
        &self,
        widget_key: &str,
        action: &FrontendAction,
        recommended: bool,
        global: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let blocked = self.widget_action_blocked(action);
        let Some(editor) = &action.editor else {
            let action = action.clone(); // rules-allow: the 'static click listener owns its copy of the action
            return self
                .attention_option(
                    SharedString::from(format!("attention-option:{widget_key}:{}", action.id)),
                    "hi.circle",
                    &action,
                    recommended,
                    blocked,
                )
                .when(!blocked, |option| {
                    option.on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_widget_action(action.clone(), global, window, cx); // rules-allow: every click activates its own copy
                    }))
                })
                .into_any_element();
        };
        let key: Arc<str> = format!(
            "{widget_key}:{}",
            match &action.op {
                Op::CapabilityCommand { arguments, .. } => arguments.as_str(),
                _ => action.id.as_str(),
            }
        )
        .into();
        if let Some((active, input, _)) = &self.chat_ui.attention_editor
            && active == &key
        {
            let text = input.read(cx).value().trim().to_owned();
            let empty = text.is_empty();
            let mut operation = action.op.clone(); // rules-allow: the answer goes out as its own Op carrying the typed text
            if let Op::CapabilityCommand { input, .. } = &mut operation {
                *input = Some(text);
            }
            return div()
                .flex()
                .items_center()
                .gap(space::S)
                .child(div().flex_1().child(text_field(input)))
                .child(
                    button("send-attention-answer", editor.submit_label.clone()) // rules-allow: the button owns its label
                        .primary()
                        .disabled(empty || !self.client.link.is_ready())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this
                                .dispatch_widget_operation(operation.clone(), global, cx) // rules-allow: every click dispatches its own Op
                                .is_some()
                            {
                                this.chat_ui.attention_editor = None;
                            }
                            cx.notify();
                        })),
                )
                .into_any_element();
        }
        let label = SharedString::from(editor.label.clone()); // rules-allow: the 'static listener owns the field's placeholder
        self.attention_option(
            SharedString::from(format!("attention-option:{key}")),
            "hi.pencilSimple",
            action,
            false,
            false,
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            let input =
                cx.new(|cx| InputState::new(window, cx).placeholder(SharedString::clone(&label)));
            let subscription = cx.subscribe(&input, |_, _, _: &InputEvent, cx| cx.notify());
            input.update(cx, |input, cx| input.focus(window, cx));
            this.chat_ui.attention_editor = Some((Arc::clone(&key), input, subscription));
            cx.notify();
        }))
        .into_any_element()
    }
}

fn attention_caret(p: &Palette, open: bool) -> gpui_component::Icon {
    chevron(open, p).text_color(p.muted)
}
