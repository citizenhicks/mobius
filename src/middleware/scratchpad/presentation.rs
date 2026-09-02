use std::collections::BTreeSet;

use super::{
    Basis, Entry, MANIFEST, PromotionTarget, Scope, ScratchpadStore, Snapshot, WriteOutcome, text,
};
use crate::Result;
use crate::middleware::{FrontendEventSink, MiddlewareCommandOutput};
use crate::protocol::{
    FrontendAction, FrontendActionListItem, FrontendEvent, FrontendListItemState, FrontendSlot,
    FrontendSymbol, FrontendTone, FrontendWidget, FrontendWidgetContent, Op,
};

pub(super) fn surface_widgets(snapshot: &Snapshot) -> Vec<FrontendWidget> {
    let global_notes = snapshot
        .global
        .iter()
        .map(|entry| entry.note.as_str())
        .collect::<BTreeSet<_>>();
    let swarm_notes = snapshot.swarm.as_ref().map(|entries| {
        entries
            .iter()
            .map(|entry| entry.note.as_str())
            .collect::<BTreeSet<_>>()
    });
    vec![
        global_widget(&snapshot.global),
        frontend_widget(
            "chat_menu",
            FrontendSlot::ChatMenu,
            text::WIDGET_TEXT,
            action_list_content(
                text::WIDGET_SESSION_TITLE,
                Scope::Session,
                &snapshot.session,
                Some(&global_notes),
                swarm_notes.as_ref(),
            ),
        ),
    ]
}

pub(super) fn global_widget(entries: &[Entry]) -> FrontendWidget {
    frontend_widget(
        "navigation",
        FrontendSlot::Navigation,
        text::WIDGET_TEXT,
        action_list_content(
            text::WIDGET_GLOBAL_TITLE,
            Scope::Global,
            entries,
            None,
            None,
        ),
    )
}

pub(super) fn swarm_widget(entries: &[Entry]) -> FrontendWidget {
    frontend_widget(
        "swarm",
        FrontendSlot::Navigation,
        text::WIDGET_TEXT,
        action_list_content(text::WIDGET_SWARM_TITLE, Scope::Swarm, entries, None, None),
    )
}

fn frontend_widget(
    id: &str,
    slot: FrontendSlot,
    text: &str,
    content: FrontendWidgetContent,
) -> FrontendWidget {
    FrontendWidget {
        id: id.into(),
        slot,
        text: text.into(),
        tone: FrontendTone::Neutral,
        symbol: Some(FrontendSymbol::Brain),
        icon_only: false,
        progress: None,
        content: Some(content),
        action: Some(Op::CapabilityCommand {
            capability: MANIFEST.id.into(),
            command: "scratchpad".into(),
            arguments: "refresh".into(),
            input: None,
            target: None,
        }),
    }
}

fn action_list_content(
    title: &str,
    scope: Scope,
    entries: &[Entry],
    global_notes: Option<&BTreeSet<&str>>,
    swarm_notes: Option<&BTreeSet<&str>>,
) -> FrontendWidgetContent {
    FrontendWidgetContent::ActionList {
        title: title.into(),
        items: entries
            .iter()
            .rev()
            .map(|entry| {
                action_list_item(
                    scope,
                    entry,
                    global_notes.is_some_and(|notes| notes.contains(entry.note.as_str())),
                    swarm_notes.map(|notes| notes.contains(entry.note.as_str())),
                )
            })
            .collect(),
    }
}

pub(super) fn action_list_item(
    scope: Scope,
    entry: &Entry,
    already_global: bool,
    already_swarm: Option<bool>,
) -> FrontendActionListItem {
    let scope_name = scope_name(scope);
    let mut actions = Vec::with_capacity(if scope == Scope::Session { 4 } else { 2 });
    if scope == Scope::Session && !already_global {
        actions.push(list_action(
            entry,
            "promote-global",
            FrontendSymbol::Promote,
            text::ACTION_PROMOTE_GLOBAL,
            FrontendTone::Neutral,
            format!("promote global {}", entry.id),
            None,
        ));
    }
    if scope == Scope::Session && matches!(already_swarm, Some(false)) {
        actions.push(list_action(
            entry,
            "promote-swarm",
            FrontendSymbol::Promote,
            text::ACTION_PROMOTE_SWARM,
            FrontendTone::Neutral,
            format!("promote swarm {}", entry.id),
            None,
        ));
    }
    actions.push(list_action(
        entry,
        "edit",
        FrontendSymbol::Edit,
        text::ACTION_EDIT,
        FrontendTone::Neutral,
        format!("edit {scope_name} {}", entry.id),
        Some(&entry.note),
    ));
    actions.push(list_action(
        entry,
        "delete",
        FrontendSymbol::Delete,
        text::ACTION_DELETE,
        FrontendTone::Error,
        format!("forget {scope_name} {}", entry.id),
        None,
    ));
    FrontendActionListItem {
        id: entry.id.clone(),
        text: entry.note.clone(),
        state: FrontendListItemState::Plain,
        actions,
    }
}

fn list_action(
    entry: &Entry,
    id: &str,
    symbol: FrontendSymbol,
    label: &str,
    tone: FrontendTone,
    arguments: String,
    input: Option<&str>,
) -> FrontendAction {
    FrontendAction {
        id: format!("{id}:{}", entry.id),
        label: label.into(),
        symbol,
        tone,
        op: Op::CapabilityCommand {
            capability: MANIFEST.id.into(),
            command: "scratchpad".into(),
            arguments,
            input: input.map(str::to_owned),
            target: None,
        },
    }
}

pub(super) fn widget_events(snapshot: &Snapshot) -> Vec<FrontendEvent> {
    surface_widgets(snapshot)
        .into_iter()
        .map(|item| FrontendEvent::Widget {
            capability: MANIFEST.id.into(),
            item,
        })
        .collect()
}

pub(super) fn publish_widgets(frontend: &FrontendEventSink, snapshot: &Snapshot) -> Result<()> {
    for event in widget_events(snapshot) {
        frontend(event)?;
    }
    Ok(())
}

pub(super) async fn publish_current_widgets(
    store: &ScratchpadStore,
    session_id: &str,
    swarm_id: Option<&str>,
    frontend: &FrontendEventSink,
) -> Result<()> {
    let snapshot = store.snapshot(session_id, swarm_id).await?;
    publish_widgets(frontend, &snapshot)
}

pub(super) fn parse_scope(scope: &str) -> Option<Scope> {
    match scope {
        "session" => Some(Scope::Session),
        "swarm" => Some(Scope::Swarm),
        "global" => Some(Scope::Global),
        _ => None,
    }
}

const fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Session => "session",
        Scope::Swarm => "swarm",
        Scope::Global => "global",
    }
}

pub(super) fn usage() -> MiddlewareCommandOutput {
    MiddlewareCommandOutput::render(MANIFEST.id, text::COMMAND_USAGE, FrontendTone::Warning)
}

pub(super) fn command_confirmation(
    target: PromotionTarget,
    outcome: WriteOutcome,
    snapshot: &Snapshot,
) -> MiddlewareCommandOutput {
    let text: String = match outcome {
        WriteOutcome::Added => match target {
            PromotionTarget::Global => text::MESSAGE_PROMOTED_GLOBAL,
            PromotionTarget::Swarm => text::MESSAGE_PROMOTED_SWARM,
        }
        .into(),
        WriteOutcome::Updated => text::MESSAGE_UPDATED_PROVENANCE.into(),
        WriteOutcome::Existing => match target {
            PromotionTarget::Global => text::MESSAGE_GLOBAL_EXISTING,
            PromotionTarget::Swarm => text::MESSAGE_SWARM_EXISTING,
        }
        .into(),
    };
    let mut events = widget_events(snapshot);
    events.extend(MiddlewareCommandOutput::render(MANIFEST.id, text, FrontendTone::Success).events);
    MiddlewareCommandOutput::events(events)
}

pub(super) fn format_snapshot(snapshot: &Snapshot) -> String {
    let mut sections = vec![format!(
        "{}\n{}",
        text::MESSAGE_SESSION_HEADING,
        format_entries(&snapshot.session)
    )];
    if let Some(swarm) = &snapshot.swarm {
        sections.push(format!(
            "{}\n{}",
            text::MESSAGE_SWARM_HEADING,
            format_entries(swarm)
        ));
    }
    sections.push(format!(
        "{}\n{}",
        text::MESSAGE_GLOBAL_HEADING,
        format_entries(&snapshot.global)
    ));
    sections.join("\n\n")
}

fn format_entries(entries: &[Entry]) -> String {
    if entries.is_empty() {
        return text::MESSAGE_NO_NOTES.into();
    }
    entries
        .iter()
        .map(|entry| format!("[{}] {}\n  {}", entry.id, entry.note, entry_metadata(entry)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn entry_metadata(entry: &Entry) -> String {
    format!(
        "{} · created at Unix time {}",
        basis_label(&entry.basis),
        entry.created_at
    )
}

fn basis_label(basis: &Basis) -> &'static str {
    match basis {
        Basis::AgentObservation => text::MESSAGE_AGENT_OBSERVATION,
        Basis::UserConfirmed => text::MESSAGE_USER_CONFIRMED,
    }
}
