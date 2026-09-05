use super::{Basis, Entry, MANIFEST, Scope, Snapshot, text};
use crate::Result;
use crate::middleware::{FrontendEventSink, MiddlewareCommandOutput};
use crate::protocol::{
    FrontendAction, FrontendActionListItem, FrontendEditor, FrontendEvent, FrontendListItemState,
    FrontendSlot, FrontendSymbol, FrontendTone, FrontendWidget, FrontendWidgetContent, Op,
};

pub(super) fn surface_widgets(snapshot: &Snapshot) -> Vec<FrontendWidget> {
    vec![global_widget(&snapshot.global)]
}

pub(super) fn global_widget(entries: &[Entry]) -> FrontendWidget {
    frontend_widget(
        "navigation",
        FrontendSlot::Navigation,
        text::WIDGET_TEXT,
        action_list_content(text::WIDGET_GLOBAL_TITLE, Scope::Global, entries),
    )
}

pub(super) fn swarm_widget(entries: &[Entry]) -> FrontendWidget {
    frontend_widget(
        "swarm",
        FrontendSlot::Navigation,
        text::WIDGET_TEXT,
        action_list_content(text::WIDGET_SWARM_TITLE, Scope::Swarm, entries),
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

fn action_list_content(title: &str, scope: Scope, entries: &[Entry]) -> FrontendWidgetContent {
    FrontendWidgetContent::ActionList {
        title: title.into(),
        actions: vec![FrontendAction {
            id: "add".into(),
            label: match scope {
                Scope::Swarm => text::ACTION_ADD_SWARM,
                Scope::Global => text::ACTION_ADD_GLOBAL,
            }
            .into(),
            symbol: FrontendSymbol::Custom("plus".into()),
            tone: FrontendTone::Neutral,
            op: Op::CapabilityCommand {
                capability: MANIFEST.id.into(),
                command: "scratchpad".into(),
                arguments: "add".into(),
                input: Some(String::new()),
                target: None,
            },
            editor: Some(FrontendEditor {
                title: match scope {
                    Scope::Swarm => text::EDITOR_SWARM_TITLE,
                    Scope::Global => text::EDITOR_GLOBAL_TITLE,
                }
                .into(),
                label: text::EDITOR_LABEL.into(),
                description: match scope {
                    Scope::Swarm => text::EDITOR_SWARM_DESCRIPTION,
                    Scope::Global => text::EDITOR_GLOBAL_DESCRIPTION,
                }
                .into(),
                submit_label: text::EDITOR_SUBMIT.into(),
            }),
        }],
        items: entries
            .iter()
            .rev()
            .map(|entry| action_list_item(scope, entry))
            .collect(),
    }
}

pub(super) fn action_list_item(scope: Scope, entry: &Entry) -> FrontendActionListItem {
    let scope_name = scope_name(scope);
    let actions = vec![
        list_action(
            entry,
            "edit",
            FrontendSymbol::Edit,
            text::ACTION_EDIT,
            FrontendTone::Neutral,
            format!("edit {scope_name} {}", entry.id),
            Some(&entry.note),
        ),
        list_action(
            entry,
            "delete",
            FrontendSymbol::Delete,
            text::ACTION_DELETE,
            FrontendTone::Error,
            format!("forget {scope_name} {}", entry.id),
            None,
        ),
    ];
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
        editor: None,
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

pub(super) fn parse_scope(scope: &str) -> Option<Scope> {
    match scope {
        "swarm" => Some(Scope::Swarm),
        "global" => Some(Scope::Global),
        _ => None,
    }
}

const fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Swarm => "swarm",
        Scope::Global => "global",
    }
}

pub(super) fn usage() -> MiddlewareCommandOutput {
    MiddlewareCommandOutput::render(MANIFEST.id, text::COMMAND_USAGE, FrontendTone::Warning)
}

pub(super) fn format_snapshot(snapshot: &Snapshot) -> String {
    let mut sections = Vec::new();
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
