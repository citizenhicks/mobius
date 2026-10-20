//! Model-composed native interface documents shown inline in the transcript.
//!
//! The document is the tool call's arguments, so replaying history re-renders it without any
//! store of its own.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::tools::{Catalog, ExecutionMode, Tool, ToolContext};
use super::{Middleware, PromptSection, RuntimeContext};
use crate::backend::model::ToolDefinition;
use crate::protocol::{
    EventMsg, FrontendBlock, FrontendBlockFormat, FrontendBlockRole, FrontendBlockUpdate,
    FrontendContribution, ToolResponse, UiBlock, UiChartKind,
};
use crate::{BoxFuture, Error, Result};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    show_ui: super::tools::ToolSpec,
    default_enabled: bool,
    manifest_description: String,
    manifest_label: String,
    prompt_main: String,
    result_shown: String,
}
crate::embedded_config! { static DEFINITION: Definition = include_str!("intelligent_ui.toml"); }
super::manifest::middleware_manifest! {
    /// Configuration metadata for native interface documents.
    "intelligent_ui", DEFINITION, required: false, settings: &[]
}

/// The tool's arguments; the title becomes the block title.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UiDocument {
    title: String,
    blocks: Vec<UiBlock>,
}

const MAX_DOCUMENT_BYTES: usize = 64 * 1024;
const MAX_BLOCKS: usize = 24;
const MAX_LIST_ITEMS: usize = 30;
const MAX_COLUMNS: usize = 6;
const MAX_ROWS: usize = 50;
const MAX_STATS: usize = 6;
const MAX_LABELS: usize = 40;
const MAX_PIE_SLICES: usize = 12;
const MAX_SERIES: usize = 4;

/// Shows native interface documents composed by the model.
#[derive(Debug)]
pub struct IntelligentUi;

impl Middleware for IntelligentUi {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn register(&self, catalog: &mut Catalog, _runtime: &RuntimeContext) -> Result<()> {
        catalog.register(Arc::new(ShowUi))
    }

    fn prompt_section(&self, _runtime: &RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(Some(PromptSection::new(DEFINITION.prompt_main.as_str())))
    }

    fn frontend(&self, _session_id: &str) -> FrontendContribution {
        FrontendContribution {
            capability: MANIFEST.id.into(),
            ..FrontendContribution::default()
        }
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        match event {
            EventMsg::ToolCallBegin(call) if *call.name == *DEFINITION.show_ui.tool.name => {
                let Ok(UiDocument { title, blocks }) = parse(&call.arguments) else {
                    return DEFINITION.show_ui.render(event);
                };
                Some(FrontendBlock {
                    id: Some(format!("{}/{}", call.turn_id, call.call_id)),
                    update: FrontendBlockUpdate::Replace,
                    role: FrontendBlockRole::Artifact,
                    format: FrontendBlockFormat::Ui,
                    title,
                    text: document_text(&blocks),
                    ui: blocks,
                    ..Default::default()
                })
            }
            // Complete the registered tool without replacing the document.
            EventMsg::ToolCallEnd(result)
                if *result.name == *DEFINITION.show_ui.tool.name && !result.is_error =>
            {
                Some(FrontendBlock {
                    id: Some(format!("{}/{}", result.turn_id, result.call_id)),
                    update: FrontendBlockUpdate::Append,
                    role: FrontendBlockRole::Artifact,
                    format: FrontendBlockFormat::Ui,
                    ..Default::default()
                })
            }
            _ => DEFINITION.show_ui.render(event),
        }
    }
}

struct ShowUi;

impl Tool for ShowUi {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &DEFINITION.show_ui.tool
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn read_only(&self) -> bool {
        true
    }

    fn call(&self, _context: ToolContext, arguments: Value) -> BoxFuture<'_, Result<ToolResponse>> {
        Box::pin(async move {
            parse(&arguments).map_err(Error::Tool)?;
            Ok(DEFINITION.result_shown.as_str().into())
        })
    }
}

fn document_text(blocks: &[UiBlock]) -> String {
    blocks
        .iter()
        .map(|block| match block {
            UiBlock::Section { title } => title.to_owned(),
            UiBlock::Text { markdown } => markdown.to_owned(),
            UiBlock::List { items } => items
                .iter()
                .map(|item| {
                    std::iter::once(item.title.as_str())
                        .chain(item.subtitle.as_deref())
                        .chain(item.trailing.as_deref())
                        .chain(item.url.as_deref())
                        .collect::<Vec<_>>()
                        .join(" · ")
                })
                .collect::<Vec<_>>()
                .join("\n"),
            UiBlock::Table { columns, rows } => std::iter::once(columns)
                .chain(rows)
                .map(|row| row.join(" | "))
                .collect::<Vec<_>>()
                .join("\n"),
            UiBlock::Stats { items } => items
                .iter()
                .map(|item| {
                    std::iter::once(item.label.as_str())
                        .chain([item.value.as_str()])
                        .chain(item.detail.as_deref())
                        .collect::<Vec<_>>()
                        .join(" · ")
                })
                .collect::<Vec<_>>()
                .join("\n"),
            UiBlock::Link {
                title,
                url,
                description,
                ..
            } => std::iter::once(title.as_str())
                .chain(description.as_deref())
                .chain([url.as_str()])
                .collect::<Vec<_>>()
                .join("\n"),
            UiBlock::Image { url, alt, caption } => std::iter::once(alt.as_str())
                .chain(caption.as_deref())
                .chain([url.as_str()])
                .collect::<Vec<_>>()
                .join("\n"),
            UiBlock::Chart {
                title,
                labels,
                series,
                unit,
                ..
            } => {
                let mut lines: Vec<std::borrow::Cow<'_, str>> =
                    title.iter().map(|title| title.as_str().into()).collect();
                for series in series {
                    lines.push(series.name.as_str().into());
                    lines.extend(labels.iter().zip(&series.values).map(|(label, value)| {
                        format!("{label}: {value}{}", unit.as_deref().unwrap_or_default()).into()
                    }));
                }
                lines.join("\n")
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Parses and bounds one document; the same check gates the tool call and its rendering.
fn parse(arguments: &Value) -> std::result::Result<UiDocument, String> {
    if crate::serialized_len(arguments).map_err(|error| error.to_string())? > MAX_DOCUMENT_BYTES {
        return Err(format!("document exceeds {MAX_DOCUMENT_BYTES} bytes"));
    }
    let document = if let Ok(document) = UiDocument::deserialize(arguments) {
        document
    } else {
        let mut presentation_arguments = arguments.clone(); // rules-allow: history events are borrowed; only a rejected document needs an owned repair copy
        super::tools::arguments::normalize(
            &mut presentation_arguments,
            &DEFINITION.show_ui.tool.parameters,
        )?;
        UiDocument::deserialize(presentation_arguments).map_err(|error| error.to_string())?
    };
    if document.title.trim().is_empty() {
        return Err("title must not be empty".into());
    }
    within("blocks", document.blocks.len(), MAX_BLOCKS)?;
    document.blocks.iter().try_for_each(check)?;
    Ok(document)
}

fn check(block: &UiBlock) -> std::result::Result<(), String> {
    match block {
        UiBlock::Section { .. } | UiBlock::Text { .. } => Ok(()),
        UiBlock::List { items } => {
            within("list items", items.len(), MAX_LIST_ITEMS)?;
            items.iter().try_for_each(|item| {
                optional_https(item.url.as_deref())?;
                optional_https(item.image_url.as_deref())
            })
        }
        UiBlock::Table { columns, rows } => {
            within("columns", columns.len(), MAX_COLUMNS)?;
            within("rows", rows.len(), MAX_ROWS)?;
            if rows.iter().any(|row| row.len() != columns.len()) {
                Err("every table row needs one cell per column".into())
            } else {
                Ok(())
            }
        }
        UiBlock::Stats { items } => within("stats", items.len(), MAX_STATS),
        UiBlock::Link { url, image_url, .. } => {
            https(url)?;
            optional_https(image_url.as_deref())
        }
        UiBlock::Image { url, .. } => https(url),
        UiBlock::Chart {
            kind,
            labels,
            series,
            ..
        } => {
            let (max_labels, max_series) = match kind {
                UiChartKind::Pie => (MAX_PIE_SLICES, 1),
                UiChartKind::Bar | UiChartKind::Line | UiChartKind::Area => {
                    (MAX_LABELS, MAX_SERIES)
                }
            };
            within("chart labels", labels.len(), max_labels)?;
            within("chart series", series.len(), max_series)?;
            series.iter().try_for_each(|series| {
                if series.values.len() != labels.len() {
                    return Err(format!(
                        "series {:?} needs one value per label",
                        series.name
                    ));
                }
                let negative = series
                    .values
                    .iter()
                    .any(|value| value.as_f64().is_none_or(|value| value < 0.0));
                if *kind == UiChartKind::Pie && negative {
                    Err("pie values must be non-negative".into())
                } else {
                    Ok(())
                }
            })
        }
    }
}

fn within(name: &str, count: usize, max: usize) -> std::result::Result<(), String> {
    if (1..=max).contains(&count) {
        Ok(())
    } else {
        Err(format!("{name} must number 1–{max}"))
    }
}

fn https(url: &str) -> std::result::Result<(), String> {
    match reqwest::Url::parse(url) {
        Ok(parsed) if parsed.scheme() == "https" && parsed.host().is_some() => Ok(()),
        _ => Err(format!("{url:?} is not an https URL")),
    }
}

fn optional_https(url: Option<&str>) -> std::result::Result<(), String> {
    url.map_or(Ok(()), https)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ToolCallBeginEvent, ToolCallEndEvent};

    #[test]
    fn repaired_documents_render_from_raw_history_without_rewriting_it() {
        let raw = serde_json::json!({
            "title": "Tokyo", "blocks": {"item": {"type": "text", "markdown": "Day 1"}}
        })
        .to_string();
        let history = [(
            crate::protocol::MessageTarget {
                checkpoint_sequence: 1,
                batch_item_count: std::num::NonZeroUsize::MIN,
            },
            serde_json::json!({"type": "function_call", "call_id": "call", "name": "show_ui", "arguments": raw}),
        )];
        let events = crate::protocol::replay_events(&history, "session");
        let block = IntelligentUi
            .render(&events[0], "session")
            .expect("document");
        assert_eq!(block.format, FrontendBlockFormat::Ui);
        assert!(matches!(block.ui.as_slice(), [UiBlock::Text { markdown }] if markdown == "Day 1"));
        assert_eq!(history[0].1["arguments"], raw);
    }

    #[test]
    fn documents_validate_render_text_and_do_not_claim_other_tools() {
        let event = EventMsg::ToolCallBegin(ToolCallBeginEvent {
            turn_id: "turn".into(),
            call_id: "call".into(),
            name: "show_ui".into(),
            arguments: serde_json::json!({"title":"Prices", "blocks":[
                {"type":"chart", "kind":"bar", "labels":["A"],
                 "series":[{"name":"Price", "values":[0.04]}], "unit":"$"},
                {"type":"link", "url":"https://example.com/a", "title":"Source"}
            ]}),
        });
        let block = IntelligentUi.render(&event, "s").expect("document");
        assert_eq!(block.format, FrontendBlockFormat::Ui);
        assert_eq!(block.ui.len(), 2);
        assert!(block.text.contains("A: 0.04$") && block.text.contains("https://example.com/a"));
        for name in ["show_ui", "bash"] {
            let end = EventMsg::ToolCallEnd(ToolCallEndEvent {
                turn_id: "turn".into(),
                call_id: "call".into(),
                name: name.into(),
                output: "done".into(),
                is_error: false,
            });
            let rendered = IntelligentUi.render(&end, "s");
            assert_eq!(rendered.is_some(), name == "show_ui");
            assert!(rendered.is_none_or(|block| block.ui.is_empty() && block.title.is_empty()));
        }
        for block in [
            serde_json::json!({"type":"image", "url":"http://x.com/a.png", "alt":"a"}),
            serde_json::json!({"type":"table", "columns":["a","b"], "rows":[["1"]]}),
            serde_json::json!({"type":"chart", "kind":"pie", "labels":["a"], "series":[{"name":"s", "values":[-1]}]}),
            serde_json::json!({"type":"text", "markdown":"x", "color":"red"}),
        ] {
            assert!(parse(&serde_json::json!({"title":"T", "blocks":[block]})).is_err());
        }
        for blocks in [
            serde_json::json!([]),
            serde_json::json!(vec![serde_json::json!({"type":"text", "markdown":"x"}); 25]),
        ] {
            assert!(parse(&serde_json::json!({"title":"T", "blocks":blocks})).is_err());
        }
    }
}
