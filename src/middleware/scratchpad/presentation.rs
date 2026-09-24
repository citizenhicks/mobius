use super::{Basis, Entry, MANIFEST, Snapshot, text};
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
        text::DEFINITION.widget_text.as_str(),
        action_list_content(text::DEFINITION.widget_global_title.as_str(), entries),
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

fn action_list_content(title: &str, entries: &[Entry]) -> FrontendWidgetContent {
    FrontendWidgetContent::ActionList {
        title: title.into(),
        actions: vec![FrontendAction {
            id: "add".into(),
            label: text::DEFINITION.action_add_global.clone(),
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
                title: text::DEFINITION.editor_global_title.clone(),
                label: text::DEFINITION.editor_label.clone(),
                description: text::DEFINITION.editor_global_description.clone(),
                submit_label: text::DEFINITION.editor_submit.clone(),
            }),
        }],
        items: entries.iter().rev().map(action_list_item).collect(),
    }
}

pub(super) fn action_list_item(entry: &Entry) -> FrontendActionListItem {
    let actions = vec![
        list_action(
            entry,
            "edit",
            FrontendSymbol::Edit,
            text::DEFINITION.action_edit.as_str(),
            FrontendTone::Neutral,
            format!("edit {}", entry.id),
            Some(&entry.note),
        ),
        list_action(
            entry,
            "delete",
            FrontendSymbol::Delete,
            text::DEFINITION.action_delete.as_str(),
            FrontendTone::Error,
            format!("forget {}", entry.id),
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

pub(super) fn usage() -> MiddlewareCommandOutput {
    MiddlewareCommandOutput::render(
        MANIFEST.id,
        text::DEFINITION.command_usage.as_str(),
        FrontendTone::Warning,
    )
}

pub(super) fn format_snapshot(snapshot: &Snapshot) -> String {
    let mut sections = Vec::new();
    sections.push(format!(
        "{}\n{}",
        text::DEFINITION.message_global_heading.as_str(),
        format_entries(&snapshot.global)
    ));
    sections.join("\n\n")
}

fn format_entries(entries: &[Entry]) -> String {
    if entries.is_empty() {
        return text::DEFINITION.message_no_notes.clone();
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
        Basis::AgentObservation => text::DEFINITION.message_agent_observation.as_str(),
        Basis::UserConfirmed => text::DEFINITION.message_user_confirmed.as_str(),
    }
}
