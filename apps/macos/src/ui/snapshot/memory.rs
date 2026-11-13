//! Memory scenarios on the offline harness catalog: the Memory page with memories across
//! projects and Global, and its inline editor. Nothing is connected, sent or saved.

use super::harness::fixture;
use super::*;
use serde_json::json;

pub(super) const SCENARIOS: &[Scenario] = &[
    ("memory-page", |this, window, cx| {
        memories(this);
        this.open_page(
            PageTab::Contribution(pages::memory::PAGE.to_owned()),
            window,
            cx,
        );
    }),
    ("memory-graph", |this, window, cx| {
        memories(this);
        this.snapshot_memory_graph();
        this.open_page(
            PageTab::Contribution(pages::memory::PAGE.to_owned()),
            window,
            cx,
        );
        undo_after_capture(Workspace::snapshot_close_memory_editor);
    }),
    ("memory-open-page", |this, window, cx| {
        memories(this);
        this.open_page(
            PageTab::Contribution(pages::memory::PAGE.to_owned()),
            window,
            cx,
        );
        this.snapshot_open_first_memory(cx);
        undo_after_capture(Workspace::snapshot_close_memory_editor);
    }),
    ("memory-page-live", |this, window, cx| {
        this.open_page(
            PageTab::Contribution(pages::memory::PAGE.to_owned()),
            window,
            cx,
        );
    }),
    ("memory-edit", |this, window, cx| {
        memories(this);
        this.open_page(
            PageTab::Contribution(pages::memory::PAGE.to_owned()),
            window,
            cx,
        );
        this.snapshot_edit_first_memory(window, cx);
        undo_after_capture(Workspace::snapshot_close_memory_editor);
    }),
    ("memory-save-pending", |this, window, cx| {
        memories(this);
        this.open_page(
            PageTab::Contribution(pages::memory::PAGE.to_owned()),
            window,
            cx,
        );
        this.snapshot_edit_first_memory(window, cx);
        assert!(
            this.snapshot_pending_memory_save(window, cx),
            "A pending memory save must not queue again or replace its editor"
        );
        undo_after_capture(Workspace::snapshot_close_memory_editor);
    }),
];

/// Scope (`global`, `app`, `site` or `loose`), name, Bot, description, body, age in minutes.
const PAGES: &[(&str, &str, &str, &str, &str, i64)] = &[
    (
        "global",
        "writing-style",
        "fixture-mobius",
        "How the user wants prose written; read before writing anything they read.",
        "Prefer plain words over jargon. Short sentences. See [[time-zone]] for dates.",
        120,
    ),
    (
        "global",
        "time-zone",
        "fixture-mobius",
        "Which time zone to show times in.",
        "Show times in Singapore time (UTC+8). Was Tallinn time until 2026-09-01, per the user.",
        8_640,
    ),
    (
        "global",
        "about-the-user",
        "fixture-mobius",
        "Who the user is and how they work.",
        "Builds the möbius apps. Prefers small diffs; see [[writing-style]] and [[time-zone]].",
        60,
    ),
    (
        "app",
        "release-process",
        "fixture-mobius",
        "How a desktop release is cut; read before tagging.",
        "## Steps\n1. Run [[checks]].\n2. Bump the version, then follow [[signing]].\n3. Publish a prerelease; promotion is a separate step.",
        20,
    ),
    (
        "app",
        "checks",
        "fixture-mobius",
        "The checks a change must pass before it is done.",
        "Run `cargo clippy --all-targets -- -D warnings` and the snapshot suite. See [[snapshot-suite]].",
        25,
    ),
    (
        "app",
        "signing",
        "fixture-mobius",
        "Where signing keys live and how notarization runs.",
        "Keys stay in the shared private store, never in the repo. Notarize after [[checks]] pass.",
        400,
    ),
    (
        "app",
        "snapshot-suite",
        "fixture-mobius",
        "How UI snapshot scenarios are rendered and reviewed.",
        "Scenarios live in `src/ui/snapshot`. Review every changed frame; see [[release-process]].",
        90,
    ),
    (
        "app",
        "gateway-pairing",
        "fixture-mobius",
        "How the app pairs with a gateway.",
        "Pair with a one-time code; the gateway owns the protocol. Links to [[release-process]].",
        2_000,
    ),
    (
        "app",
        "design-tokens",
        "fixture-mobius",
        "Where spacing, colours and sizes come from.",
        "Use the central tokens; never ad-hoc values. Follows [[writing-style]] for labels.",
        1_200,
    ),
    (
        "site",
        "legal-pages",
        "fixture-mobius",
        "What the legal pages must name.",
        "Legal pages name thinkingsand, Tallinn, Estonia. See [[deploy-site]].",
        300,
    ),
    (
        "site",
        "deploy-site",
        "fixture-mobius",
        "How the website deploys.",
        "Push to main; previews build per branch. Uses [[package-manager]].",
        500,
    ),
    (
        "site",
        "package-manager",
        "fixture-retired",
        "Which package manager the web projects use.",
        "Use pnpm, not npm, in the web projects.",
        12_960,
    ),
    (
        "loose",
        "scratch-notes",
        "fixture-mobius",
        "Notes from chats without a project.",
        "Nothing links here yet.",
        9_000,
    ),
];

/// The harness catalog with a small linked wiki across two projects, Global, No project
/// and a Bot that was deleted.
fn memories(this: &mut Workspace) {
    fixture(this);
    let now = chrono::Utc::now().timestamp();
    let records: Vec<serde_json::Value> = PAGES
        .iter()
        .map(|&(scope, name, bot, description, text, minutes)| {
            let project = match scope {
                "app" => json!({"id": "ws-app", "path": "/Users/you/work/mobius-app"}),
                "site" => json!({"id": "ws-site", "path": "/Users/you/work/website"}),
                _ => serde_json::Value::Null,
            };
            let links: Vec<serde_json::Value> = text
                .split("[[")
                .skip(1)
                .filter_map(|rest| rest.split_once("]]").map(|(name, _)| name))
                .map(|link| {
                    let target = match (name, link) {
                        ("writing-style" | "about-the-user", "time-zone") => {
                            Some("global/time-zone")
                        }
                        ("about-the-user" | "design-tokens", "writing-style") => {
                            Some("global/writing-style")
                        }
                        ("release-process" | "signing", "checks") => Some("app/checks"),
                        ("release-process", "signing") => Some("app/signing"),
                        ("checks", "snapshot-suite") => Some("app/snapshot-suite"),
                        ("snapshot-suite" | "gateway-pairing", "release-process") => {
                            Some("app/release-process")
                        }
                        ("legal-pages", "deploy-site") => Some("site/deploy-site"),
                        _ => None,
                    };
                    json!({"name": link, "target": target})
                })
                .collect();
            json!({"id": format!("{scope}/{name}"), "name": name, "description": description,
                "links": links, "harness": "mobius", "project": project, "bot_id": bot,
                "global": scope == "global", "text": text, "updated_at": now - minutes * 60,
                "editable": true})
        })
        .collect();
    match serde_json::from_value(serde_json::Value::Array(records)) {
        Ok(records) => this.client.state.memories = Some(records),
        Err(error) => eprintln!("snapshot memory fixture: {error}"),
    }
}
