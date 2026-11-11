mod actions;

use super::*;
use crate::numeric::ToFloat;
use mobius::protocol::{
    FrontendAction, FrontendListItemState, FrontendSlot, FrontendTone, FrontendWidget,
    FrontendWidgetContent, Op,
};
use mobius_gateway::wire::{BotShape, ClientMessage};
use std::rc::Rc;

/// The stable key of a contributed widget: its capability and its id.
pub(in crate::ui) fn widget_key(capability: &str, id: &str) -> String {
    format!("{capability}:{id}")
}

#[derive(Clone, Copy, PartialEq)]
enum Origin {
    Gateway,
    Live,
    Session,
}

/// Slot queries filter before resolving duplicates and keep the first matching widget.
/// Page queries keep the last gateway/live widget, with the first static session fallback.
/// Both retain gateway > live session > static session precedence and key order.
fn collect_widgets<'a>(
    sources: impl Iterator<Item = (&'a str, &'a Arc<FrontendWidget>, Origin)>,
    slot: Option<FrontendSlot>,
) -> BTreeMap<String, (&'a Arc<FrontendWidget>, bool)> {
    let mut items = BTreeMap::new();
    for (capability, widget, origin) in sources {
        if slot.is_some_and(|slot| widget.slot != slot) {
            continue;
        }
        let global = origin == Origin::Gateway;
        let entry = items.entry(widget_key(capability, &widget.id));
        match entry {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((widget, global));
            }
            std::collections::btree_map::Entry::Occupied(mut entry)
                if slot.is_none() && origin != Origin::Session && (global || !entry.get().1) =>
            {
                entry.insert((widget, global));
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
    }
    items
}

impl Workspace {
    pub(in crate::ui) fn widget_items(
        &self,
        slot: Option<FrontendSlot>,
    ) -> BTreeMap<String, (&Arc<FrontendWidget>, bool)> {
        let global = self.client.state.gateway.iter().flat_map(|gateway| {
            gateway
                .agent_catalog
                .contributions
                .iter()
                .flat_map(|contribution| {
                    contribution
                        .widgets
                        .iter()
                        .map(|widget| (contribution.capability.as_ref(), widget, Origin::Gateway))
                })
        });
        let live = self.client.state.session.iter().flat_map(|session| {
            session
                .widgets
                .iter()
                .map(|widget| (widget.capability.as_ref(), &widget.item, Origin::Live))
        });
        let local = self.client.state.session.iter().flat_map(|session| {
            session.contributions.iter().flat_map(|contribution| {
                contribution
                    .widgets
                    .iter()
                    .map(|widget| (contribution.capability.as_ref(), widget, Origin::Session))
            })
        });
        collect_widgets(global.chain(live).chain(local), slot)
    }

    pub(in crate::ui) fn is_navigation_contribution(&self, key: &str) -> bool {
        self.widget_items(None)
            .get(key)
            .is_some_and(|(widget, _)| widget.slot == FrontendSlot::Navigation)
    }

    pub(in crate::ui) fn contribution_title(&self, key: &str) -> String {
        self.widget_items(None)
            .get(key)
            .map_or_else(|| "Contribution".into(), |(widget, _)| widget.text.clone()) // rules-allow: callers take an owned title, the widget stays in gateway state
    }

    pub(in crate::ui) fn navigation_rail_widgets(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        self.widget_buttons(FrontendSlot::Navigation, true, cx)
    }

    pub(in crate::ui) fn slot_widgets(
        &self,
        slot: FrontendSlot,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        self.widget_buttons(slot, false, cx)
    }

    fn widget_buttons(
        &self,
        slot: FrontendSlot,
        rail: bool,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        self.widget_items(None)
            .into_iter()
            .filter(|(_, (widget, _))| widget.slot == slot)
            .map(|(key, (widget, global))| {
                // Like the rail sections: the primary page decides, not a side tab.
                let selected = matches!(self.primary_page(), Some(PageTab::Contribution(open)) if open == &key);
                let id = SharedString::from(&key);
                let action = widget.action.clone(); // rules-allow: the 'static listener owns its copy of the operation from gateway state
                let opens = widget.content.is_some() || slot == FrontendSlot::Navigation;
                self.widget_button(id, widget, rail, selected, cx)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(op) = &action {
                            let op = op.clone(); // rules-allow: every click sends its own operation
                            this.dispatch_widget_operation(op, global, cx);
                        }
                        if opens {
                            let key = key.clone(); // rules-allow: open_page takes an owned tab on every click
                            this.open_page(PageTab::Contribution(key), window, cx);
                        }
                    }))
                    .into_any_element()
            })
            .collect()
    }

    fn widget_button(
        &self,
        id: SharedString,
        widget: &FrontendWidget,
        rail: bool,
        selected: bool,
        cx: &App,
    ) -> Button {
        let p = self.palette;
        let color = p.tone(widget.tone);
        let title = match &widget.content {
            Some(FrontendWidgetContent::ActionList { title, .. }) => {
                SharedString::from(format!("{title} · {}", widget.text))
            }
            None => SharedString::from(&widget.text),
        };
        let glyph = symbol_icon_or(widget.symbol.as_ref(), "hi.squaresFour");
        if rail {
            return rail_section_button(
                id,
                title,
                RailGlyph::Pair(glyph, filled_glyph(glyph)),
                selected,
                cx,
            )
            .when(
                !selected && widget.tone != FrontendTone::Neutral,
                |button| button.text_color(color),
            );
        }
        let mut pill = pill(id);
        if let Some(progress) = widget.progress {
            pill = pill.progress(if progress.total == 0 {
                0.
            } else {
                progress.completed.to_f32() / progress.total.to_f32()
            });
        }
        if widget.symbol.is_some() {
            pill = pill.glyph(glyph);
        }
        if !widget.icon_only {
            pill = pill.label(&widget.text);
        }
        pill.button(cx)
            .tooltip(SharedString::clone(&title))
            .accessibility_label(title)
            .when(selected, |button| {
                button.bg(p.accent_soft).text_color(p.accent)
            })
    }

    pub(in crate::ui) fn render_contribution(&self, key: &str, cx: &Context<Self>) -> AnyElement {
        let Some((title, body)) = self.contribution_parts(key, cx) else {
            return page("contribution", PageHeader::new("Contribution"))
                .child(EmptyState::new(
                    "hi.setup01",
                    "This item is no longer available on the gateway.",
                ))
                .into_any_element();
        };
        page(
            SharedString::from(format!("contribution:{key}")),
            PageHeader::new(title),
        )
        .child(body)
        .into_any_element()
    }

    /// A contributed list's own actions, for the page header or a chat side tab's toolbar.
    pub(in crate::ui) fn contribution_actions(
        &self,
        key: &str,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let Some((widget, global)) = self.widget_items(None).remove(key) else {
            return Vec::new();
        };
        match &widget.content {
            Some(FrontendWidgetContent::ActionList { actions, .. }) => actions
                .iter()
                .map(|action| self.widget_header_action(action, global, cx))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// A contributed widget's title and content, without the page around them, so a chat
    /// side tab can show the same content.
    pub(in crate::ui) fn contribution_parts(
        &self,
        key: &str,
        cx: &Context<Self>,
    ) -> Option<(&str, AnyElement)> {
        let (widget, global) = self.widget_items(None).remove(key)?;
        let body = match &widget.content {
            Some(FrontendWidgetContent::ActionList { items, .. }) => {
                use FrontendListItemState::*;
                let faces = widget.symbol == Some(mobius::protocol::FrontendSymbol::Agent);
                let rows = |states: &[FrontendListItemState]| -> Vec<Row> {
                    items
                        .iter()
                        .enumerate()
                        .filter(|(_, item)| states.contains(&item.state))
                        .map(|(index, item)| self.action_row(index, item, global, faces, cx))
                        .collect()
                };
                let sections = [
                    ("Active", rows(&[InProgress, Pending])),
                    ("Failed", rows(&[Failed])),
                    ("Interrupted", rows(&[Interrupted])),
                    ("Done", rows(&[Completed])),
                ]
                .into_iter()
                .filter(|(_, rows)| !rows.is_empty())
                .map(|(title, rows)| {
                    Section::new(format!("{title} · {}", rows.len()))
                        .child(Group::new().children(rows))
                });
                let plain = rows(&[Plain]);
                div()
                    .flex()
                    .flex_col()
                    .gap(space::L)
                    .children(sections)
                    .when(!plain.is_empty(), |list| {
                        list.child(Group::new().children(plain))
                    })
                    .into_any_element()
            }
            None => EmptyState::new("hi.setup01", "Waiting for gateway content…")
                .loading()
                .into_any_element(),
        };
        Some((widget.text.as_str(), body))
    }
}

impl Workspace {
    /// One list item: a face for subagents, "Waiting" while it has not started, and the
    /// whole row opening it when it has a single plain action.
    fn action_row(
        &self,
        index: usize,
        item: &mobius::protocol::FrontendActionListItem,
        global: bool,
        faces: bool,
        cx: &Context<Self>,
    ) -> Row {
        let p = self.palette;
        let mut row = Row::new(("action-item", index), &item.text);
        if faces && let Some((tint, shape)) = stable_face(&item.id) {
            row = row.leading(Leading::Face(tint, shape));
        }
        if item.state == FrontendListItemState::Pending {
            row = row.control(inline_status(p.muted, "Waiting", &p));
        }
        match item.actions.as_slice() {
            [action] if action.editor.is_none() && !action.input_from_label => {
                let action = Rc::new(action.clone()); // rules-allow: the 'static click handler owns its copy of the action from gateway state
                let entity = cx.entity();
                row.disclosure().on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| {
                        this.activate_widget_action(Rc::clone(&action), global, window, cx);
                    });
                })
            }
            actions => actions.iter().fold(row, |row, action| {
                row.control(self.widget_action(action, global, cx))
            }),
        }
    }
}

/// A face picked from `id`, the same on every draw, for items without a Bot of their own.
fn stable_face(id: &str) -> Option<(Hsla, BotShape)> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    id.hash(&mut hasher);
    let hash = hasher.finish();
    let pick = |bits: u64, len: usize| usize::try_from(bits).ok().map(|bits| bits % len);
    let shape = BotShape::ALL.get(pick(hash >> 32, BotShape::ALL.len())?)?;
    let accent = Accent::ALL.get(pick(hash & u64::from(u32::MAX), Accent::ALL.len())?)?;
    Some((accent.color(), *shape))
}

/// "● Running" on the row's own line: a dot in `tone` and the word in caption ink.
fn inline_status(tone: Hsla, label: &'static str, p: &Palette) -> AnyElement {
    let dot = "●".len();
    div()
        .typography(Type::Caption)
        .text_color(p.muted)
        .child(StyledText::new(format!("● {label}")).with_highlights([(0..dot, tone.into())]))
        .into_any_element()
}
