//! Event Centre (WP5): approvals, chats needing attention, extension reviews and routine
//! results (EventCentreView.swift).

use super::*;
use mobius::SessionId;
use mobius_gateway::wire::{RoutineRun, RoutineRunStatus, SessionActivityState, SessionRecord};

#[derive(Clone, PartialEq)]
pub(in crate::ui) enum EventTarget {
    Chat(SessionId),
    Extension(Arc<str>),
    Bot(Arc<str>),
}

#[derive(Clone, PartialEq)]
pub(in crate::ui) struct EventItem {
    pub id: String,
    /// Changes when the event changes, so a read event can become unread again.
    pub revision: String,
    pub title: String,
    pub detail: String,
    pub glyph: &'static str,
    pub tone: Tone,
    /// Unix seconds; 0 while an approval is waiting.
    pub occurred_at: i64,
    pub needs_action: bool,
    pub target: EventTarget,
}

impl EventItem {
    fn approval(request_id: &str, session_id: &SessionId, title: String) -> Self {
        Self {
            id: format!("approval:{request_id}"),
            revision: request_id.to_owned(),
            title,
            detail: "Approval required".into(),
            glyph: "hi.shieldCheck",
            tone: Tone::Warning,
            occurred_at: 0,
            needs_action: true,
            target: EventTarget::Chat(session_id.share()),
        }
    }

    /// A chat waiting on the reader for something other than an approval, such as a question.
    fn attention(session: &SessionRecord) -> Self {
        let count = session.activity.attention;
        Self {
            id: format!("attention:{}", session.session_id),
            revision: count.to_string(),
            title: single_line(&crate::ui::navigation::session_title(session)).to_string(),
            detail: format!(
                "{} your attention",
                counted(count, "item needs", "items need")
            ),
            glyph: "hi.mousePointerClick",
            tone: Tone::Warning,
            occurred_at: session.updated_at,
            needs_action: true,
            target: EventTarget::Chat(session.session_id.share()),
        }
    }

    /// `message` is the run's message in plain words.
    fn routine(run: &RoutineRun, bot: String, message: Option<String>) -> Self {
        let (status, glyph, tone) = match run.status {
            RoutineRunStatus::Succeeded => ("Routine succeeded", "hi.checkCircle", Tone::Success),
            RoutineRunStatus::Failed => ("Routine failed", "hi.warning", Tone::Error),
            RoutineRunStatus::Skipped => ("Routine skipped", "hi.warning", Tone::Warning),
            RoutineRunStatus::Cancelled => ("Routine cancelled", "hi.stopFill", Tone::Info),
            RoutineRunStatus::Running => ("Routine running", "hi.playFill", Tone::Info),
        };
        let detail = match message {
            Some(message) => format!("{status} · {message}"),
            None => status.to_owned(),
        };
        Self {
            id: format!("routine:{}", run.id),
            revision: format!("{:?}", run.status),
            title: bot,
            detail,
            glyph,
            tone,
            occurred_at: run.finished_at.unwrap_or(run.started_at),
            needs_action: false,
            target: EventTarget::Bot(Arc::clone(&run.bot_id)),
        }
    }

    fn kind(&self) -> &'static str {
        match self.target {
            EventTarget::Chat(_) if self.id.starts_with("attention:") => "Needs attention",
            EventTarget::Chat(_) => "Approval",
            EventTarget::Extension(_) => "Extension review",
            EventTarget::Bot(_) => "Routine run",
        }
    }

    fn action(&self) -> &'static str {
        match self.target {
            EventTarget::Chat(_) => "Open chat",
            EventTarget::Extension(_) => "Review extension",
            EventTarget::Bot(_) => "Open Bot",
        }
    }
}

/// Attention an approval does not already cover, such as an open question.
fn needs_attention(session: &SessionRecord) -> bool {
    session.activity.attention > 0
        && session.activity.state != SessionActivityState::AwaitingApproval
}

/// Needs-action first, then newest, then by id; the first of each id wins.
fn ordered(mut items: Vec<EventItem>) -> Vec<EventItem> {
    items.sort_by(|a, b| a.id.cmp(&b.id));
    items.dedup_by(|later, earlier| later.id == earlier.id);
    items.sort_by(|a, b| {
        (!a.needs_action, std::cmp::Reverse(a.occurred_at), &a.id).cmp(&(
            !b.needs_action,
            std::cmp::Reverse(b.occurred_at),
            &b.id,
        ))
    });
    items
}

/// The bell only needs routine identity/status, not titles, detail text or sorted rows.
fn routine_attention(history: &[RoutineRun], reads: &BTreeMap<String, String>) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    history
        .iter()
        .filter(|run| run.status != RoutineRunStatus::Running)
        .any(|run| {
            seen.insert(run.id.as_ref())
                && reads.get(&format!("routine:{}", run.id)) != Some(&format!("{:?}", run.status))
        })
}

impl Workspace {
    pub(in crate::ui) fn refresh_events(&mut self, cx: &mut Context<Self>) {
        self.send(
            ClientMessage::ListRoutineHistory {
                request_id: crate::gateway::request_id(),
                id: None,
            },
            cx,
        );
    }

    fn bot_name(&self, id: &str) -> String {
        self.client
            .state
            .bot(id)
            .map_or_else(|| "Bot".into(), |bot| bot.name.to_string())
    }

    /// Source order is also duplicate priority; only the sidebar needs sorted presentation.
    fn event_candidates(&self) -> impl Iterator<Item = EventItem> + '_ {
        let gateway = self.client.state.gateway.as_ref();
        let approvals = gateway
            .into_iter()
            .flat_map(|gateway| &gateway.background_approvals)
            .map(|approval| {
                let bot = self.bot_name(&approval.bot_id);
                EventItem::approval(&approval.request_id, &approval.session_id, bot)
            });
        let waiting = self
            .client
            .state
            .sessions
            .iter()
            .filter(move |_| gateway.is_some())
            .filter_map(|session| {
                let request = session.activity.approval_request_id.as_deref()?;
                (session.activity.state == SessionActivityState::AwaitingApproval).then(|| {
                    let title = single_line(&crate::ui::navigation::session_title(session));
                    EventItem::approval(request, &session.session_id, title.to_string())
                })
            });
        let attention = self
            .client
            .state
            .sessions
            .iter()
            .filter(move |session| gateway.is_some() && needs_attention(session))
            .map(EventItem::attention);
        let extensions = gateway
            .into_iter()
            .flat_map(|gateway| &gateway.agent_catalog.extensions)
            .filter(|record| !record.hooks_trusted && !record.hooks.is_empty())
            .map(|record| EventItem {
                id: format!("extension:{}", record.id),
                revision: record.digest.to_string(),
                title: record.name.to_string(),
                detail: "Review untrusted hooks".into(),
                glyph: "hi.squaresFour",
                tone: Tone::Warning,
                occurred_at: 0,
                needs_action: true,
                target: EventTarget::Extension(Arc::clone(&record.id)),
            });
        let routines = self
            .client
            .state
            .routine_history
            .iter()
            .filter(move |run| gateway.is_some() && run.status != RoutineRunStatus::Running)
            .map(|run| {
                let message = run
                    .message
                    .as_deref()
                    .map(|text| self.provider_failure_text(text));
                EventItem::routine(run, self.bot_name(&run.bot_id), message)
            });
        approvals
            .chain(waiting)
            .chain(attention)
            .chain(extensions)
            .chain(routines)
    }

    /// Every event the gateway reports, in Event Centre order.
    pub(in crate::ui) fn event_items(&self) -> Vec<EventItem> {
        ordered(self.event_candidates().collect())
    }

    fn event_unread(&self, item: &EventItem) -> bool {
        self.config.event_reads.get(&item.id) != Some(&item.revision)
    }

    /// Whether the rail bell should show: something needs action or is unread.
    pub(in crate::ui) fn events_need_attention(&self) -> bool {
        let Some(gateway) = &self.client.state.gateway else {
            return false;
        };
        if !gateway.background_approvals.is_empty()
            || self.client.state.sessions.iter().any(|session| {
                (session.activity.state == SessionActivityState::AwaitingApproval
                    && session.activity.approval_request_id.is_some())
                    || needs_attention(session)
            })
            || gateway
                .agent_catalog
                .extensions
                .iter()
                .any(|record| !record.hooks_trusted && !record.hooks.is_empty())
        {
            return true;
        }
        routine_attention(&self.client.state.routine_history, &self.config.event_reads)
    }

    pub(super) fn mark_events_read(&mut self, id: Option<&str>) {
        for item in self.event_items() {
            if id.is_none_or(|id| id == item.id) {
                self.config.event_reads.insert(item.id, item.revision);
            }
        }
    }

    fn open_event_target(
        &mut self,
        target: &EventTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page = match target {
            EventTarget::Chat(id) => PageTab::Chat(id.share()),
            EventTarget::Extension(id) => PageTab::Extension(id.to_string()),
            EventTarget::Bot(id) => PageTab::Bot(Arc::clone(id)),
        };
        self.open_page(page, window, cx);
    }

    /// The Event Centre landing: nothing selected yet.
    pub(in crate::ui) fn render_events(&self, _cx: &mut Context<Self>) -> AnyElement {
        let count = self
            .event_candidates()
            .map(|item| item.id)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let empty = if count == 0 {
            EmptyState::new("hi.bell", "No events yet")
                .detail("Approvals, extension reviews and routine results appear here.")
        } else {
            EmptyState::new("hi.bell", "Choose an event").detail(format!(
                "{} on this gateway.",
                counted(count, "event", "events")
            ))
        };
        page(
            "events",
            PageHeader::new("Event Centre").subtitle("Approvals and results"),
        )
        .child(empty)
        .into_any_element()
    }

    /// Opens what the event is about: its chat, extension or Bot.
    pub(super) fn event_actions(&self, id: &str, cx: &Context<Self>) -> Vec<AnyElement> {
        let Some(item) = self.event_candidates().find(|item| item.id == id) else {
            return Vec::new();
        };
        let action = item.action();
        let target = item.target;
        vec![
            toolbar_button("event-open", action, "hi.arrowUpRight01", cx)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_event_target(&target, window, cx);
                }))
                .into_any_element(),
        ]
    }

    pub(in crate::ui) fn render_event(&self, id: &str) -> AnyElement {
        let Some(item) = self.event_candidates().find(|item| item.id == id) else {
            return page("event", PageHeader::new("Event"))
                .child(
                    EmptyState::new("hi.bell", "Event unavailable")
                        .detail("It was resolved or is no longer reported by the gateway."),
                )
                .into_any_element();
        };
        let when = if item.occurred_at > 0 {
            ago(item.occurred_at)
        } else {
            "Waiting for you".into()
        };
        let kind = item.kind();
        let header = PageHeader::new(item.title)
            .leading(icon(item.glyph, &self.palette).size(glyph::ICON))
            .subtitle(format!("{kind} · {when}"));
        let mut group = Group::new()
            .child(Row::new("event-kind", "Kind").value(kind))
            .child(Row::new("event-when", "When").value(when));
        group = group.child(Row::new("event-detail", "Detail").subtitle(item.detail));
        page(SharedString::from(format!("event:{id}")), header)
            .child(Section::new("Details").child(group))
            .into_any_element()
    }

    /// The Event Centre list (WP5): Mark all read, then Needs action and Recent.
    pub(in crate::ui) fn events_list(&self, cx: &Context<Self>) -> AnyElement {
        let items = self.event_items();
        let unread = items.iter().any(|item| self.event_unread(item));
        let mut list = crate::ui::navigation::sidebar_list().child(
            SidebarRow::new("mark-events-read", "Mark all read")
                .leading(Leading::glyph("hi.checkCircle"))
                .disabled(!unread)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.mark_events_read(None);
                    cx.notify();
                })),
        );
        if items.is_empty() {
            return list
                .child(sidebar_note("No events yet."))
                .into_any_element();
        }
        let (action, recent): (Vec<_>, Vec<_>) =
            items.into_iter().partition(|item| item.needs_action);
        for (label, group) in [("Needs action", action), ("Recent", recent)] {
            if group.is_empty() {
                continue;
            }
            list = list.child(sidebar_label(label));
            for item in group {
                let unread = self.event_unread(&item);
                let id = SharedString::from(item.id.as_str());
                let mut row = self.page_row(
                    id,
                    item.title,
                    Leading::Glyph(item.glyph.into()),
                    PageTab::Event(item.id),
                    cx,
                );
                if item.occurred_at > 0 {
                    row = row.meta(ago(item.occurred_at));
                }
                if unread {
                    row = row.status(status_dot(self.palette.accent));
                }
                list = list.child(row.tooltip(item.detail));
            }
        }
        list.into_any_element()
    }
}
