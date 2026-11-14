//! Dreaming: while a chat sits idle, one model call reads the chat's transcript since the last
//! dream next to the visible pages, then patches the pages through the same path as
//! `patch_memory`. No approval is involved; every change lands in the scope's log and trash.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::pages::Scope;
use super::tools::parse_ops;
use super::{Basis, Memory, Writer, items, publish_widgets, text};
use crate::backend::checkpoint::{CheckpointStore, TranscriptPageRequest};
use crate::backend::model::{ModelRequest, internal_user_message};
use crate::middleware::ModelContext;
use crate::middleware::sessions::history_text;
use crate::{Error, Result, SessionId};

const STATE_SCOPE: &str = "memory_dream";
const MAX_TRANSCRIPT_CHARS: usize = 60_000;
const MAX_ITEM_CHARS: usize = 4_000;
const MAX_PAGE_CHARS: usize = 40_000;

/// Dreams once; a failure is logged and leaves the watermark, so a later idle period retries.
pub(super) async fn run(memory: &Memory, context: &mut ModelContext<'_>) {
    if let Err(error) = dream(memory, context).await {
        tracing::warn!(session = %context.session_id, %error, "memory dream failed");
    }
}

async fn dream(memory: &Memory, context: &mut ModelContext<'_>) -> Result<()> {
    let store = &memory.store;
    let owner = memory.owner()?;
    let key = context.session_id.to_string();
    let watermark = store
        .checkpoints
        .load_state(STATE_SCOPE, &key)
        .await?
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let (transcript, Some(newest)) =
        transcript_since(store.checkpoints.as_ref(), context.session_id, watermark).await?
    else {
        return Ok(());
    };
    if transcript.is_empty() {
        return store
            .checkpoints
            .save_state(STATE_SCOPE, &key, &newest.into())
            .await;
    }
    let visible = {
        let _access = store.access.read().await;
        store.visible(owner).await?
    };
    let prompt = format!(
        "{}\n<transcript>\n{transcript}\n</transcript>",
        render_pages(&visible)?
    );
    let input = [internal_user_message("memory_dream", &prompt)];
    let route = context.provider;
    let output = {
        let definitions = context.tools.registered_definitions();
        let tool = definitions
            .iter()
            .find(|tool| tool.name == "patch_memory")
            .ok_or_else(|| Error::Config("patch_memory is not registered".into()))?;
        let request = ModelRequest {
            session_id: Some(context.session_id),
            cancellation: context.cancellation,
            prompt_cache: None,
            instructions: &text::DEFINITION.prompt_dream,
            input: (&input).into(),
            catalog_revision: context.tools.revision()?,
            tools: std::slice::from_ref(tool),
            deferred_tools: &[],
            allow_hosted_tools: false,
            allow_continuation: false,
        };
        // ponytail: one attempt; a failed dream retries in the next idle period.
        context
            .model
            .respond(route, request, Arc::new(|_| Box::pin(async { Ok(()) })))
            .await?
    };
    let (_, calls, usage) = output.into_parts();
    context.record_usage(route, usage);
    let mut patched = false;
    for call in calls
        .into_iter()
        .filter(|call| &*call.name == "patch_memory")
    {
        let ops = parse_ops(call.arguments)?;
        let access = store.access.write().await;
        let results = store
            .apply_locked(
                owner,
                ops,
                Basis::AgentObservation,
                Writer::Dream(context.session_id),
                &access,
            )
            .await?;
        drop(access);
        tracing::info!(session = %context.session_id, ?results, "memory dream patched pages");
        patched = true;
    }
    if patched {
        let all = {
            let _access = store.access.read().await;
            store.pages.all().await?
        };
        publish_widgets(&context.runtime.frontend, &items(&all)?)?;
    }
    store
        .checkpoints
        .save_state(STATE_SCOPE, &key, &newest.into())
        .await
}

/// The transcript after `watermark`, oldest first, newest-biased within the character budget.
async fn transcript_since(
    checkpoints: &dyn CheckpointStore,
    session_id: &SessionId,
    watermark: u64,
) -> Result<(String, Option<u64>)> {
    let mut before_sequence = None;
    let mut newest = None;
    let mut lines = Vec::new();
    let mut chars = 0;
    'pages: loop {
        let page = checkpoints
            .transcript_page(
                session_id,
                TranscriptPageRequest {
                    before_sequence,
                    max_batches: 8,
                },
            )
            .await?;
        for batch in &page.batches {
            if batch.sequence <= watermark {
                break 'pages;
            }
            newest.get_or_insert(batch.sequence);
            for (kind, text) in batch.items.iter().rev().filter_map(history_text) {
                let text = text.chars().take(MAX_ITEM_CHARS).collect::<String>();
                chars += text.len();
                lines.push(format!("{}: {text}", kind.to_ascii_uppercase()));
                // ponytail: an over-long gap dreams its newest part only.
                if chars >= MAX_TRANSCRIPT_CHARS {
                    break 'pages;
                }
            }
        }
        match page.next_before_sequence {
            Some(next) => before_sequence = Some(next),
            None => break,
        }
    }
    lines.reverse();
    Ok((lines.join("\n\n"), newest))
}

/// Visible pages in full while they fit, then index lines, then the links memory could not
/// resolve, by the same scope rules as search and the clients.
fn render_pages(visible: &[Scope; 2]) -> Result<String> {
    let dangling = items(visible)?
        .into_iter()
        .flat_map(|item| item.links)
        .filter(|link| link.target.is_none())
        .map(|link| link.name)
        .collect::<BTreeSet<_>>();
    let mut text = String::from("<memory_pages>\n");
    for scope in visible {
        let global = scope.owner.is_none();
        for page in scope.pages.iter() {
            let header = format!(
                "## [[{}]] global: {global} base_updated_at: {}\n{}\n",
                page.name, page.updated_at, page.description
            );
            text.push_str(&header);
            if text.len() + page.body.len() < MAX_PAGE_CHARS {
                text.push_str(&page.body);
                text.push_str("\n\n");
            } else {
                text.push_str(
                    "(body omitted for length; keep it unless the transcript changes it)\n\n",
                );
            }
        }
    }
    if visible.iter().all(|scope| scope.pages.is_empty()) {
        text.push_str("(no pages yet)\n");
    }
    text.push_str("</memory_pages>\n");
    if !dangling.is_empty() {
        text.push_str(
            "<lint>
Links to missing pages: ",
        );
        text.push_str(&dangling.into_iter().collect::<Vec<_>>().join(", "));
        text.push_str(
            "
</lint>
",
        );
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::memory::pages::Page;
    use crate::middleware::memory::{MemoryOwner, MemoryProject};

    fn page(name: &str, body: &str) -> Page {
        Page {
            name: name.into(),
            description: format!("about {name}"),
            body: body.into(),
            basis: Basis::AgentObservation,
            updated_at: 1,
            author: None,
            digest: Default::default(),
        }
    }

    #[test]
    fn lint_lists_links_memory_cannot_resolve_from_the_linking_scope() {
        let owner = Arc::new(MemoryOwner {
            bot: "bot".into(),
            project: Some(MemoryProject {
                id: "project".into(),
                label: None,
            }),
        });
        // A global page cannot see project pages, so its link is dangling; the project page
        // resolves the same name in its own scope.
        let visible = [
            Scope {
                owner: None,
                pages: vec![page("about-the-user", "See [[deploy]].")].into(),
            },
            Scope {
                owner: Some(owner),
                pages: vec![page("deploy", "Run make. See [[missing]].")].into(),
            },
        ];
        let text = render_pages(&visible).expect("render");
        assert!(
            text.contains("Links to missing pages: deploy, missing"),
            "{text}"
        );
    }
}
