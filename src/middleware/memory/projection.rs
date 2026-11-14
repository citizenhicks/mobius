use std::collections::BTreeMap;

use crate::backend::model::ModelInput;
use serde::Deserialize;
use serde_json::Value;

use super::pages::Scope;
use super::{Basis, PROJECTION_KIND, text};
use crate::backend::model::{internal_user_message, set_field};
use crate::protocol::internal_message_kind;

// Each scope's index, newest first, with short user-confirmed pages in full.
const MAX_SCOPE_INDEX_BYTES: usize = 6 * 1024;
const MAX_INLINE_BODY_BYTES: usize = 500;
/// Page name to version and content digest per `[global, owner]` scope. The
/// leading underscore keeps it out of provider requests.
const KNOWN_FIELD: &str = "_memory_known";

type Known = [BTreeMap<String, (u64, String)>; 2];

pub(super) fn is_projection_item(item: &Value) -> bool {
    internal_message_kind(item) == Some(PROJECTION_KIND)
}

/// The full memory index when `input` has none, else a list of the pages changed since the
/// model last heard about them, like deferred tools: names now, content through `read_memory`.
pub(super) fn next_projection(input: ModelInput<'_>, visible: &[Scope; 2]) -> Option<Value> {
    let previous = input
        .iter()
        .rev()
        .find(|item| is_projection_item(item))
        .and_then(|item| Known::deserialize(item.get(KNOWN_FIELD)?).ok());
    let current: [BTreeMap<&str, (u64, String)>; 2] = visible.each_ref().map(|scope| {
        scope
            .pages
            .iter()
            .map(|page| (page.name.as_str(), page.version()))
            .collect()
    });
    let text = match previous {
        None if current.iter().all(BTreeMap::is_empty) => return None,
        None => render(visible),
        Some(known)
            if known.iter().zip(&current).all(|(known, current)| {
                known
                    .iter()
                    .map(|(name, at)| (name.as_str(), at))
                    .eq(current.iter().map(|(name, at)| (*name, at)))
            }) =>
        {
            return None;
        }
        Some(known) => render_changes(&known, &current, visible),
    };
    let mut item = internal_user_message(PROJECTION_KIND, &text);
    set_field(&mut item, KNOWN_FIELD, serde_json::to_value(current).ok()?);
    Some(item)
}

fn render_changes(
    known: &Known,
    current: &[BTreeMap<&str, (u64, String)>; 2],
    visible: &[Scope; 2],
) -> String {
    let mut text = text::DEFINITION.prompt_changes_header.clone(); // rules-allow: the notice owns its text
    for (((label, known), current), scope) in [
        &text::DEFINITION.prompt_changes_global,
        &text::DEFINITION.prompt_changes_owner,
    ]
    .into_iter()
    .zip(known)
    .zip(current)
    .zip(visible)
    {
        text.extend(
            scope
                .pages
                .iter()
                .filter(|page| known.get(page.name.as_str()) != current.get(page.name.as_str()))
                .map(|page| {
                    format!(
                        "- {label}[[{}]] (updated_at {}) {}\n",
                        page.name, page.updated_at, page.description
                    )
                }),
        );
        text.extend(
            known
                .keys()
                .filter(|name| !current.contains_key(name.as_str()))
                .map(|name| format!("- {label}[[{name}]] deleted\n")),
        );
    }
    text.push_str(&text::DEFINITION.prompt_changes_footer);
    text
}

fn render(visible: &[Scope; 2]) -> String {
    let mut text = text::DEFINITION.prompt_projection_header.clone(); // rules-allow: the projection owns its text
    for (heading, scope) in [
        &text::DEFINITION.prompt_projection_global,
        &text::DEFINITION.prompt_projection_owner,
    ]
    .into_iter()
    .zip(visible)
    {
        text.push_str(heading);
        if scope.pages.is_empty() {
            text.push_str(&text::DEFINITION.prompt_projection_empty);
        }
        let start = text.len();
        for (shown, page) in scope.pages.iter().enumerate() {
            let mut line = format!(
                "- [[{}]] (updated_at {}) {}\n",
                page.name, page.updated_at, page.description
            );
            if page.basis == Basis::UserConfirmed && page.body.len() <= MAX_INLINE_BODY_BYTES {
                for body in page.body.lines() {
                    line.push_str("  > ");
                    line.push_str(body);
                    line.push('\n');
                }
            }
            // ponytail: newest pages win the index budget; search finds the rest.
            if text.len() - start + line.len() > MAX_SCOPE_INDEX_BYTES {
                text.push_str(
                    &text::DEFINITION
                        .prompt_projection_more
                        .replace("{count}", &(scope.pages.len() - shown).to_string()),
                );
                break;
            }
            text.push_str(&line);
        }
    }
    text.push_str(&text::DEFINITION.prompt_projection_footer);
    text
}

#[cfg(test)]
mod tests {
    use super::super::pages::Page;
    use super::*;

    fn page(name: &str, updated_at: u64) -> Page {
        Page {
            name: name.into(),
            description: format!("about {name}"),
            body: "body".into(),
            basis: Basis::AgentObservation,
            updated_at,
            author: None,
            digest: Default::default(),
        }
    }

    fn scopes(global: Vec<Page>, owner: Vec<Page>) -> [Scope; 2] {
        [global, owner].map(|pages| Scope {
            owner: None,
            pages: pages.into(),
        })
    }

    fn content(item: &Value) -> &str {
        item.pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    #[test]
    fn manual_page_edit_is_reported_even_without_a_timestamp_change() {
        let original = scopes(vec![page("style", 1)], vec![]);
        let input: [Value; 0] = [];
        let index = next_projection(input.as_slice().into(), &original).expect("index");
        let mut edited = original;
        let mut pages = edited[0].pages.to_vec();
        pages[0].description = "updated by hand".into();
        edited[0].pages = pages.into();
        let notice = next_projection([index].as_slice().into(), &edited).expect("edit notice");
        assert!(content(&notice).contains("updated by hand"));
    }

    #[test]
    fn index_is_sent_once_then_only_changed_page_names_follow() {
        let mut input = Vec::new();
        let start = scopes(
            vec![page("style", 1)],
            vec![page("deploy", 1), page("ci", 1)],
        );
        let index = next_projection(input.as_slice().into(), &start).expect("index");
        assert!(content(&index).contains("[[deploy]]"));
        input.push(index);
        assert!(next_projection(input.as_slice().into(), &start).is_none());

        let changed = scopes(
            vec![page("style", 1), page("tone", 3)],
            vec![page("deploy", 2), page("office", 3)],
        );
        let notice = next_projection(input.as_slice().into(), &changed).expect("notice");
        assert_eq!(
            content(&notice),
            format!(
                "{}- global [[tone]] (updated_at 3) about tone\n- [[deploy]] (updated_at 2) about deploy\n- [[office]] (updated_at 3) about office\n- [[ci]] deleted\n{}",
                text::DEFINITION.prompt_changes_header,
                text::DEFINITION.prompt_changes_footer
            )
        );
        input.push(notice);
        assert!(next_projection(input.as_slice().into(), &changed).is_none());
    }
}
