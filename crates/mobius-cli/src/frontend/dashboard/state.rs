use std::sync::Arc;

use mobius::protocol::{
    FrontendAction, FrontendActionListItem, FrontendEvent, FrontendSlot, FrontendWidget,
    FrontendWidgetContent, Op,
};
use mobius_gateway::wire::{
    ClientStatus, ProfileSnapshot, ReadyPayload, SessionActivityState, SessionReadyPayload,
    SessionRecord,
};
use ratatui::widgets::ListState;

use crate::frontend::catalog::KeyedWidgets;
use crate::frontend::terminal::{CONTRIBUTED_WIDGET_BYTES, MAX_ENTRY_BYTES, bounded_widget};

pub(super) struct DashboardState {
    pub(super) endpoint: String,
    pub(super) gateway: ReadyPayload,
    pub(super) clients: Vec<ClientStatus>,
    pub(super) current_client_id: Option<String>,
    pub(super) devices: Selection<String>,
    pub(super) chats: Selection<mobius::SessionId>,
    pub(super) bots: Selection<Arc<str>>,
    pub(super) focus: DashboardFocus,
    pub(super) pending_unpair: Option<(String, String)>,
    pub(super) profile: Option<ProfileSnapshot>,
    /// The request id of the chat being opened for its capability overlay.
    pub(super) pending_open: Option<Arc<str>>,
    pub(super) overlay: Option<CapabilityOverlay>,
    pub(super) error: Option<String>,
}

impl DashboardState {
    pub(super) fn move_selection(&mut self, movement: RowMove) {
        self.update_selection(self.focus, movement);
    }

    /// Keeps the selected id when it is still listed, and otherwise selects the first row.
    pub(super) fn sync_selection(&mut self, focus: DashboardFocus) {
        self.update_selection(focus, RowMove::By(0));
    }

    fn update_selection(&mut self, focus: DashboardFocus, movement: RowMove) {
        match focus {
            DashboardFocus::Devices => self.devices.update(
                self.clients.len(),
                || self.clients.iter().map(|client| &client.client_id),
                |id| id.as_str().into(),
                movement,
            ),
            DashboardFocus::Chats => self.chats.update(
                self.gateway.sessions.len(),
                || ordered_sessions(&self.gateway.sessions).map(|session| &session.session_id),
                mobius::SessionId::share,
                movement,
            ),
            DashboardFocus::Bots => self.bots.update(
                self.gateway.bots.len(),
                || self.gateway.bots.iter().map(|bot| &bot.id),
                Arc::clone,
                movement,
            ),
        }
    }

    pub(super) fn sync_devices(&mut self) {
        self.clients.sort_by(|left, right| {
            (left.connections == 0)
                .cmp(&(right.connections == 0))
                .then_with(|| left.label.cmp(&right.label))
        });
        self.sync_selection(DashboardFocus::Devices);
    }
}

#[derive(Clone, Copy)]
pub(super) enum RowMove {
    First,
    Last,
    By(isize),
}

impl RowMove {
    fn apply(self, list: &mut ListState, length: usize) {
        list.select(self.index(list.selected(), length));
    }

    pub(super) fn index(self, current: Option<usize>, length: usize) -> Option<usize> {
        let last = length.checked_sub(1)?;
        Some(match self {
            Self::First => 0,
            Self::Last => last,
            Self::By(delta) => current
                .unwrap_or_default()
                .saturating_add_signed(delta)
                .min(last),
        })
    }
}

pub(super) fn ordered_sessions(sessions: &[SessionRecord]) -> impl Iterator<Item = &SessionRecord> {
    [false, true].into_iter().flat_map(|idle| {
        sessions
            .iter()
            .filter(move |session| (session.activity.state == SessionActivityState::Idle) == idle)
    })
}

/// One list's selection: the selected record's id, which survives reordering, and its row.
pub(super) struct Selection<Id> {
    pub(super) id: Option<Id>,
    pub(super) list: ListState,
}

impl<Id> Default for Selection<Id> {
    fn default() -> Self {
        Self {
            id: None,
            list: ListState::default(),
        }
    }
}

impl<Id: PartialEq> Selection<Id> {
    /// Tracks the selected id across reordering and only takes ownership when it changes.
    fn update<'row, I>(
        &mut self,
        len: usize,
        ids: impl Fn() -> I,
        own: fn(&Id) -> Id,
        movement: RowMove,
    ) where
        Id: 'row,
        I: Iterator<Item = &'row Id>,
    {
        let current = self
            .id
            .as_ref()
            .and_then(|id| ids().position(|candidate| candidate == id));
        let selected = movement.index(current, len);
        if current.is_none() || current != selected {
            self.id = selected.and_then(|index| ids().nth(index)).map(own);
        }
        self.list.select(selected);
    }
}

#[derive(Default)]
pub(in crate::frontend) struct CapabilityOverlay {
    pub(super) title: String,
    pub(super) session_id: Option<mobius::SessionId>,
    pub(super) slots: Vec<FrontendSlot>,
    pub(super) widgets: KeyedWidgets,
    pub(super) widget_list: ListState,
    pub(super) open: Option<(Arc<str>, Arc<str>)>,
    pub(super) option_list: ListState,
    pub(super) action_index: usize,
    pub(super) input: Option<ActionInput>,
}

pub(super) struct ActionInput {
    pub(super) op: Op,
    pub(super) text: String,
    pub(super) cursor: usize,
}

/// Updates chat and overlay from the same terminal-safe widget.
pub(in crate::frontend) fn apply_widget_event(
    event: FrontendEvent,
    chat: Option<&mut KeyedWidgets>,
    mut overlay: Option<&mut CapabilityOverlay>,
) {
    match event {
        FrontendEvent::Widget { capability, item } => {
            let visible = overlay.as_deref_mut().and_then(|view| {
                if view.slots.contains(&item.slot) {
                    Some(&mut view.widgets)
                } else {
                    remove(&mut view.widgets, &capability, &item.id);
                    None
                }
            });
            let mut targets = chat.into_iter().chain(visible);
            if let Some(first) = targets.next() {
                let item = bounded_widget(item, MAX_ENTRY_BYTES);
                if let Some(second) = targets.next() {
                    upsert(second, Arc::clone(&capability), Arc::clone(&item));
                }
                upsert(first, capability, item);
            }
        }
        FrontendEvent::RemoveWidget { capability, id } => {
            for widgets in chat
                .into_iter()
                .chain(overlay.as_deref_mut().map(|view| &mut view.widgets))
            {
                remove(widgets, &capability, &id);
            }
        }
        _ => return,
    }
    if let Some(overlay) = overlay {
        overlay.sync_selection();
    }
}

fn upsert(widgets: &mut KeyedWidgets, capability: Arc<str>, item: Arc<FrontendWidget>) {
    if let Some((_, current)) = widgets
        .iter_mut()
        .find(|(owner, widget)| *owner == capability && widget.id == item.id)
    {
        *current = item;
    } else {
        widgets.push((capability, item));
    }
}

fn remove(widgets: &mut KeyedWidgets, capability: &str, id: &str) {
    widgets.retain(|(owner, widget)| **owner != *capability || *widget.id != *id);
}

impl CapabilityOverlay {
    pub(super) fn from_session(payload: SessionReadyPayload) -> Self {
        let session_id = payload.session.session_id.share();
        let widgets = payload
            .contributions
            .into_iter()
            .flat_map(|contribution| {
                let capability = contribution.capability;
                contribution.widgets.into_iter().filter_map(move |item| {
                    (item.slot == FrontendSlot::Navigation).then(|| {
                        (
                            Arc::clone(&capability),
                            bounded_widget(item, CONTRIBUTED_WIDGET_BYTES),
                        )
                    })
                })
            })
            .collect();
        let mut overlay = Self {
            title: format!("Chat capabilities · {session_id}"),
            session_id: Some(session_id),
            slots: vec![FrontendSlot::Navigation],
            widgets,
            ..Self::default()
        };
        for widget in payload.widgets {
            apply_widget_event(
                FrontendEvent::Widget {
                    capability: widget.capability,
                    item: widget.item,
                },
                None,
                Some(&mut overlay),
            );
        }
        overlay.sync_selection();
        overlay
    }

    pub(in crate::frontend) fn attention(widgets: KeyedWidgets) -> Self {
        let mut overlay = Self {
            title: "Attention".into(),
            slots: vec![FrontendSlot::Attention],
            widgets,
            ..Self::default()
        };
        overlay.sync_selection();
        overlay
    }

    pub(in crate::frontend) fn from_widgets(fallback_title: String, widgets: KeyedWidgets) -> Self {
        let title = widgets
            .first()
            .map_or_else(|| fallback_title, |(_, item)| item.text.clone()); // rules-allow: the widget keeps its text while the overlay owns the title
        let mut slots = widgets
            .iter()
            .map(|(_, item)| item.slot)
            .collect::<Vec<_>>();
        slots.sort_unstable();
        slots.dedup();
        let open = match widgets.as_slice() {
            [(capability, widget)] => Some((Arc::clone(capability), Arc::clone(&widget.id))),
            _ => None,
        };
        let mut overlay = Self {
            title,
            slots,
            widgets,
            open,
            ..Self::default()
        };
        overlay.sync_selection();
        overlay
    }

    pub(in crate::frontend) const fn is_editing(&self) -> bool {
        self.input.is_some()
    }

    pub(in crate::frontend) fn can_go_back(&self) -> bool {
        self.widgets.len() > 1 && self.open.is_some()
    }

    pub(in crate::frontend) fn close_widget(&mut self) {
        self.open = None;
        self.input = None;
        self.sync_selection();
    }

    pub(super) fn sync_selection(&mut self) {
        RowMove::By(0).apply(&mut self.widget_list, self.widgets.len());
        if self
            .open
            .as_ref()
            .is_some_and(|key| self.widget(key).is_none())
        {
            self.open = None;
        }
        let options = self.action_items().map_or(0, <[_]>::len);
        RowMove::By(0).apply(&mut self.option_list, options);
        self.move_action(0);
    }

    pub(super) fn move_selection(&mut self, movement: RowMove) {
        let (list, length) = match self.action_items().map(<[_]>::len) {
            Some(length) => (&mut self.option_list, length),
            None if self.open.is_none() => (&mut self.widget_list, self.widgets.len()),
            None => return,
        };
        movement.apply(list, length);
        self.action_index = 0;
    }

    pub(super) fn selected_widget(&self) -> Option<&(Arc<str>, Arc<FrontendWidget>)> {
        self.widgets.get(self.widget_list.selected()?)
    }

    pub(in crate::frontend) fn open_widget(&self) -> Option<&FrontendWidget> {
        self.widget(self.open.as_ref()?)
    }

    fn widget(&self, (capability, id): &(Arc<str>, Arc<str>)) -> Option<&FrontendWidget> {
        self.widgets
            .iter()
            .find(|(owner, widget)| owner == capability && widget.id == *id)
            .map(|(_, widget)| &**widget)
    }

    pub(super) fn action_items(&self) -> Option<&[FrontendActionListItem]> {
        let FrontendWidgetContent::ActionList { items, .. } =
            self.open_widget()?.content.as_ref()?;
        Some(items)
    }

    fn actions(&self) -> &[FrontendAction] {
        self.action_items()
            .and_then(|items| items.get(self.option_list.selected()?))
            .map_or(&[], |item| item.actions.as_slice())
    }

    pub(super) fn move_action(&mut self, delta: isize) {
        self.action_index = RowMove::By(delta)
            .index(Some(self.action_index), self.actions().len())
            .unwrap_or_default();
    }

    pub(super) fn selected_action(&self) -> Option<&FrontendAction> {
        self.actions().get(self.action_index)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DashboardFocus {
    Devices,
    Chats,
    Bots,
}

impl DashboardFocus {
    pub(super) const fn next(self) -> Self {
        match self {
            Self::Devices => Self::Chats,
            Self::Chats => Self::Bots,
            Self::Bots => Self::Devices,
        }
    }
}
