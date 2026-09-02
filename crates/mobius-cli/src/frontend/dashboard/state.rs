use mobius::protocol::{
    FrontendActionListItem, FrontendEvent, FrontendSlot, FrontendWidget, FrontendWidgetContent, Op,
};
use mobius_gateway::wire::{ClientStatus, ProfileSnapshot, ReadyPayload, SessionReadyPayload};
use ratatui::widgets::ListState;

pub(super) struct DashboardState {
    pub(super) endpoint: String,
    pub(super) gateway: ReadyPayload,
    pub(super) clients: Vec<ClientStatus>,
    pub(super) current_client_id: Option<String>,
    pub(super) selected_client_id: Option<String>,
    pub(super) selected_session_id: Option<String>,
    pub(super) selected_bot_id: Option<String>,
    pub(super) device_list: ListState,
    pub(super) chat_list: ListState,
    pub(super) bot_list: ListState,
    pub(super) focus: DashboardFocus,
    pub(super) pending_unpair: Option<(String, String)>,
    pub(super) profile: Option<ProfileSnapshot>,
    pub(super) pending_open: Option<(String, String)>,
    pub(super) overlay: Option<CapabilityOverlay>,
    pub(super) error: Option<String>,
}

pub(in crate::frontend) struct CapabilityOverlay {
    pub(super) title: String,
    pub(super) session_id: String,
    pub(super) slots: Vec<FrontendSlot>,
    pub(super) widgets: Vec<((String, String), FrontendWidget)>,
    pub(super) widget_list: ListState,
    pub(super) open: Option<(String, String)>,
    pub(super) option_list: ListState,
    pub(super) action_index: usize,
    pub(super) input: Option<ActionInput>,
}

pub(super) struct ActionInput {
    pub(super) op: Op,
    pub(super) text: String,
    pub(super) cursor: usize,
}

impl CapabilityOverlay {
    pub(super) fn from_session(payload: SessionReadyPayload) -> Self {
        let session_id = payload.session.session_id;
        let widgets = payload
            .contributions
            .into_iter()
            .flat_map(|contribution| {
                contribution.widgets.into_iter().filter_map(move |item| {
                    (item.slot == FrontendSlot::Navigation)
                        .then(|| ((contribution.capability.clone(), item.id.clone()), item))
                })
            })
            .collect();
        let mut overlay = Self {
            title: format!("Chat capabilities · {session_id}"),
            session_id,
            slots: vec![FrontendSlot::Navigation],
            widgets,
            widget_list: ListState::default(),
            open: None,
            option_list: ListState::default(),
            action_index: 0,
            input: None,
        };
        for widget in payload.widgets {
            overlay.apply(FrontendEvent::Widget {
                capability: widget.capability,
                item: widget.item,
            });
        }
        overlay.sync_selection();
        overlay
    }

    pub(in crate::frontend) fn from_widgets(
        capability: String,
        items: Vec<FrontendWidget>,
    ) -> Self {
        let title = items
            .first()
            .map_or_else(|| capability.clone(), |item| item.text.clone());
        let mut slots = items.iter().map(|item| item.slot).collect::<Vec<_>>();
        slots.sort_unstable();
        slots.dedup();
        let widgets = items
            .into_iter()
            .map(|item| ((capability.clone(), item.id.clone()), item))
            .collect::<Vec<_>>();
        let open = (widgets.len() == 1).then(|| widgets[0].0.clone());
        let mut overlay = Self {
            title,
            session_id: String::new(),
            slots,
            widgets,
            widget_list: ListState::default(),
            open,
            option_list: ListState::default(),
            action_index: 0,
            input: None,
        };
        overlay.sync_selection();
        overlay
    }

    pub(in crate::frontend) fn apply(&mut self, event: FrontendEvent) {
        match event {
            FrontendEvent::Widget { capability, item } => {
                let key = (capability, item.id.clone());
                if self.slots.contains(&item.slot) {
                    if let Some((_, widget)) = self
                        .widgets
                        .iter_mut()
                        .find(|(candidate, _)| candidate == &key)
                    {
                        *widget = item;
                    } else {
                        self.widgets.push((key, item));
                    }
                } else {
                    self.widgets.retain(|(candidate, _)| candidate != &key);
                }
            }
            FrontendEvent::RemoveWidget { capability, id } => {
                let key = (capability, id);
                self.widgets.retain(|(candidate, _)| candidate != &key);
            }
            _ => return,
        }
        self.sync_selection();
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
        let selected = self
            .widgets
            .len()
            .checked_sub(1)
            .map(|last| self.widget_list.selected().unwrap_or_default().min(last));
        self.widget_list.select(selected);
        if self
            .open
            .as_ref()
            .is_some_and(|key| self.widget(key).is_none())
        {
            self.open = None;
        }
        let options = self
            .open_widget()
            .and_then(|widget| match widget.content.as_ref() {
                Some(FrontendWidgetContent::Picker { options, .. }) => Some(options.len()),
                Some(FrontendWidgetContent::ActionList { items, .. }) => Some(items.len()),
                _ => None,
            })
            .unwrap_or_default();
        self.option_list.select(
            options
                .checked_sub(1)
                .map(|last| self.option_list.selected().unwrap_or_default().min(last)),
        );
        self.action_index = self
            .selected_action_list_item()
            .and_then(|item| item.actions.len().checked_sub(1))
            .map_or(0, |last| self.action_index.min(last));
    }

    pub(super) fn selected_key(&self) -> Option<(String, String)> {
        self.widgets
            .get(self.widget_list.selected()?)
            .map(|(key, _)| key)
            .cloned()
    }

    pub(in crate::frontend) fn open_widget(&self) -> Option<&FrontendWidget> {
        self.widget(self.open.as_ref()?)
    }

    pub(super) fn widget(&self, key: &(String, String)) -> Option<&FrontendWidget> {
        self.widgets
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, widget)| widget)
    }

    pub(super) fn selected_action_list_item(&self) -> Option<&FrontendActionListItem> {
        let FrontendWidgetContent::ActionList { items, .. } =
            self.open_widget()?.content.as_ref()?
        else {
            return None;
        };
        items.get(self.option_list.selected()?)
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
