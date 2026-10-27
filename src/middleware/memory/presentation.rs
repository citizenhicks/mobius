use std::borrow::Cow;
use std::sync::Arc;

use super::{Basis, MANIFEST, MemoryItem, MemoryOwner, Partition, text};
use crate::Result;
use crate::middleware::{FrontendEventSink, MiddlewareCommandOutput};
use crate::protocol::{
    FrontendAction, FrontendActionListItem, FrontendEditor, FrontendEvent, FrontendListItemState,
    FrontendSlot, FrontendSymbol, FrontendTone, FrontendWidget, FrontendWidgetContent, Op,
};

pub(super) fn surface_widgets(memories: &[MemoryItem]) -> Vec<Arc<FrontendWidget>> {
    vec![Arc::new(FrontendWidget {
        id: "navigation".into(),
        count: None,
        priority: crate::protocol::FrontendPriority::Primary,
        slot: FrontendSlot::Navigation,
        text: text::DEFINITION.widget_text.as_str().into(),
        tone: FrontendTone::Neutral,
        symbol: Some(FrontendSymbol::Brain),
        icon_only: false,
        progress: None,
        content: Some(action_list_content(memories)),
        action: Some(Op::command(MANIFEST.id, "memory", "refresh")),
    })]
}

fn action_list_content(memories: &[MemoryItem]) -> FrontendWidgetContent {
    FrontendWidgetContent::ActionList {
        title: text::DEFINITION.widget_global_title.as_str().into(),
        actions: vec![FrontendAction {
            id: "add".into(),
            label: text::DEFINITION.action_add_global.clone(), // rules-allow: owned frontend record built from static text
            symbol: FrontendSymbol::Plus,
            tone: FrontendTone::Neutral,
            op: Op::CapabilityCommand {
                capability: MANIFEST.id.into(),
                command: "memory".into(),
                arguments: "add".into(),
                input: Some(String::new()),
                target: None,
            },
            input_from_label: false,
            editor: Some(FrontendEditor {
                title: text::DEFINITION.editor_global_title.clone(), // rules-allow: owned frontend record built from static text
                label: text::DEFINITION.editor_label.clone(), // rules-allow: owned frontend record built from static text
                description: text::DEFINITION.editor_global_description.clone(), // rules-allow: owned frontend record built from static text
                submit_label: text::DEFINITION.editor_submit.clone(), // rules-allow: owned frontend record built from static text
            }),
        }],
        items: memories.iter().rev().map(action_list_item).collect(),
    }
}

fn action_list_item(memory: &MemoryItem) -> FrontendActionListItem {
    let actions = vec![
        list_action(
            memory,
            "edit",
            FrontendSymbol::Edit,
            text::DEFINITION.action_edit.as_str(),
            FrontendTone::Neutral,
            format!("edit {}", memory.id),
            Some(&memory.text),
        ),
        list_action(
            memory,
            "delete",
            FrontendSymbol::Delete,
            text::DEFINITION.action_delete.as_str(),
            FrontendTone::Error,
            format!("forget {}", memory.id),
            None,
        ),
    ];
    FrontendActionListItem {
        id: (*memory.id).into(),
        text: (*memory.text).into(),
        state: FrontendListItemState::Plain,
        actions,
    }
}

fn list_action(
    memory: &MemoryItem,
    id: &str,
    symbol: FrontendSymbol,
    label: &str,
    tone: FrontendTone,
    arguments: String,
    input: Option<&str>,
) -> FrontendAction {
    FrontendAction {
        input_from_label: false,
        editor: None,
        id: format!("{id}:{}", memory.id),
        label: label.into(),
        symbol,
        tone,
        op: Op::CapabilityCommand {
            capability: MANIFEST.id.into(),
            command: "memory".into(),
            arguments,
            input: input.map(str::to_owned),
            target: None,
        },
    }
}

pub(super) fn widget_events(memories: &[MemoryItem]) -> Vec<FrontendEvent> {
    surface_widgets(memories)
        .into_iter()
        .map(|item| FrontendEvent::Widget {
            capability: MANIFEST.id.into(),
            item,
        })
        .collect()
}

pub(super) fn publish_widgets(frontend: &FrontendEventSink, memories: &[MemoryItem]) -> Result<()> {
    for event in widget_events(memories) {
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

pub(super) fn format_snapshot(partitions: &[Partition]) -> String {
    let sections = partitions
        .iter()
        .filter(|partition| partition.owner.is_none() || !partition.memories.is_empty())
        .map(|partition| {
            let heading = partition.owner.as_deref().map_or(
                Cow::Borrowed(text::DEFINITION.message_global_heading.as_str()),
                |owner| Cow::Owned(heading(owner)),
            );
            if partition.memories.is_empty() {
                return format!("{heading}\n{}", text::DEFINITION.message_no_memories);
            }
            let memories = partition
                .memories
                .iter()
                .map(|entry| {
                    format!(
                        "[{}] {}\n  {} · updated at Unix time {}",
                        entry.id,
                        entry.text,
                        basis_label(entry.basis),
                        entry.updated_at
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!("{heading}\n{memories}")
        })
        .collect::<Vec<_>>();
    sections.join("\n\n")
}

fn heading(owner: &MemoryOwner) -> String {
    let project = owner
        .project
        .as_ref()
        .map_or(text::DEFINITION.message_no_project.as_str(), |project| {
            project.label.as_deref().unwrap_or(&project.id)
        });
    format!("{} · {project}", owner.bot)
}

fn basis_label(basis: Basis) -> &'static str {
    match basis {
        Basis::AgentObservation => text::DEFINITION.message_agent_observation.as_str(),
        Basis::UserConfirmed => text::DEFINITION.message_user_confirmed.as_str(),
    }
}
