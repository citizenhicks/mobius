//! Harness scenarios on an offline typed catalog: the möbius rail group, a möbius Bot's
//! settings and a Bot whose harness the gateway lacks. Nothing is connected, sent or saved.

use super::Scenario;
use super::capture::catalog;

use crate::gateway::{Link, Update};
use crate::ui::navigation::Section;

use crate::ui::router::{AgentSection, PageTab};
use crate::ui::{Workspace, pages};

use gpui::{Context, Window};

use serde_json::{Value, json};
use std::sync::Arc;

pub(super) const SCENARIOS: &[Scenario] = &[
    ("send-failure-copy", |this, window, cx| {
        fixture(this);
        let notices = std::mem::take(&mut this.shell.notices);
        let link = this.client.link.clone(); // rules-allow: the self-check restores the original connection presentation
        let loading = "The gateway did not finish loading this chat in time.";
        this.gateway_update(
            Update::SendFailed {
                request_id: Some("fixture-open".into()),
                delivery_uncertain: true,
                message: loading.into(),
            },
            window,
            cx,
        );
        assert_eq!(
            this.shell.notices.last().map(|(_, text, _)| text.as_str()),
            Some(loading)
        );
        assert!(
            matches!(&this.client.link, Link::Failed(message) if &**message == "Connection failed.")
        );
        this.client.insert_pending(
            "fixture-send".into(),
            crate::gateway::PendingMessage {
                draft_key: "fixture-draft".into(),
                text: "Keep this message".into(),
                attachments: Vec::new(),
                files: Vec::new(),
                reply: None,
            },
        );
        this.gateway_update(
            Update::SendFailed {
                request_id: Some("fixture-send".into()),
                delivery_uncertain: true,
                message: "Gateway writer closed.".into(),
            },
            window,
            cx,
        );
        assert!(
            this.shell
                .notices
                .last()
                .is_some_and(|(_, text, _)| text.starts_with("Delivery is uncertain."))
        );
        assert!(this.client.pending_submission("fixture-send"));
        assert!(
            matches!(&this.client.link, Link::Failed(message) if &**message == "Delivery failed.")
        );
        drop(this.client.take_pending("fixture-send"));
        this.shell.notices = notices;
        this.set_link(link, cx);
    }),
    ("rail-mobius", |this, window, cx| {
        fixture(this);
        this.select_section(Section::Mobius, window, cx);
    }),
    ("rail-mobius-extensions", |this, window, cx| {
        fixture(this);
        this.open_page(PageTab::Extensions, window, cx);
    }),
    ("rail-agent-defaults", |this, window, cx| {
        fixture(this);
        this.open_page(PageTab::AgentDefaults(AgentSection::Model), window, cx);
    }),
    ("agent-defaults-capabilities", |this, window, cx| {
        fixture(this);
        this.open_page(
            PageTab::AgentDefaults(AgentSection::Capabilities),
            window,
            cx,
        );
    }),
    ("bots-create-mobius-only", |this, window, cx| {
        fixture(this);
        this.select_section(Section::Bots, window, cx);
        this.create_bot_dialog(window, cx);
    }),
    ("bots-settings-mobius", |this, window, cx| {
        fixture(this);
        settings(this, "mobius", AgentSection::Identity, window, cx);
    }),
    ("bots-gateway-switch", |this, window, cx| {
        fixture(this);
        let scope = pages::bots::AgentScope::Bot("fixture-mobius".into());
        this.snapshot_edit_draft(&scope, window, cx);
        this.draft_workspace = "/gateway-a/work".into();
        this.draft_bot = Some("fixture-mobius".into());
        assert!(this.snapshot_draft_is_dirty(&scope, cx));
        this.connecting("same gateway".into(), true, window, cx);
        assert_eq!(this.draft_workspace.to_str(), Some("/gateway-a/work"));
        assert_eq!(this.draft_bot.as_deref(), Some("fixture-mobius"));
        assert!(this.snapshot_draft_is_dirty(&scope, cx));
        this.connecting("another gateway".into(), false, window, cx);
        assert_eq!(this.draft_workspace.to_str(), Some("."));
        assert_eq!(this.draft_bot, None);
        fixture(this);
        this.open_page(
            PageTab::BotSettings("fixture-mobius".into(), AgentSection::Identity),
            window,
            cx,
        );
        assert!(
            !this.snapshot_draft_is_dirty(&scope, cx),
            "A gateway with the same Bot ID and revision must not inherit another gateway’s draft"
        );
    }),
    // The Codex Bot stays listed and readable, with why it cannot run.
    ("bots-harness-unavailable", |this, window, cx| {
        fixture(this);
        this.open_page(PageTab::Bot("fixture-codex".into()), window, cx);
    }),
];

pub(super) fn settings(
    this: &mut Workspace,
    harness: &str,
    section: AgentSection,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let id: Arc<str> = format!("fixture-{harness}").into();
    let scope = pages::bots::AgentScope::Bot(Arc::clone(&id));
    this.snapshot_reset_draft(&scope);
    this.open_page(PageTab::BotSettings(id, section), window, cx);
}

fn bot(
    harness: &str,
    name: &str,
    tint: &str,
    shape: &str,
    description: &str,
    config: &Value,
) -> Value {
    json!({
        "id": format!("fixture-{harness}"), "handle": harness, "name": name,
        "description": description, "tint": tint, "shape": shape, "harness": harness,
        "conversation_session_id": format!("fixture-conversation-{harness}"),
        "capabilities": {"attachments": false, "computer": false, "voice": {"state": "off"},
            "memory": {"enabled": true, "global": false}},
        "routine_interaction_policy": "unattended",
        "config": {"revision": 1, "config": config}
    })
}

/// Replaces the catalog with a möbius Bot and a Codex Bot this gateway cannot run.
/// The live connection, catalog and chats are kept and restored after the capture, so
/// later scenarios can still open real chats.
pub(super) fn fixture(this: &mut Workspace) {
    let live = (
        this.client.gateway.take(),
        this.client.state.gateway.take(),
        this.client.state.sessions.clone(), // rules-allow: the live chats come back after the fixture's capture
    );
    LIVE.with(|stash| {
        stash.borrow_mut().get_or_insert(live);
    });
    super::probes::undo_after_capture(|this| {
        if let Some((gateway, catalog, sessions)) = LIVE.with(|stash| stash.borrow_mut().take()) {
            this.client.gateway = gateway;
            this.client.state.gateway = catalog;
            this.client.state.sessions = sessions;
            this.gateway_revision += 1;
        }
    });
    let mobius = json!({
        "kind": "mobius", "label": "möbius", "symbol": "agent",
        "install": {"state": "not_applicable"}, "auth": {"state": "not_applicable"},
        "capabilities": {"attachments": true, "steering": true, "approvals": true, "resume": true,
            "voice": true, "computer": true},
        "models": [], "settings": []
    });
    let bots = json!([
        bot(
            "mobius",
            "Researcher",
            "purple",
            "circle",
            "Finds and summarises sources.",
            &json!({
                "harness": "mobius",
                "provider": {"instance": "preview", "provider": "preview", "model": "preview",
                    "endpoint_auth": "credentialless", "web_search": "off"},
                "middleware": {"enabled": ["memory"], "settings": {}},
                "extensions": [], "system_prompt": "Cite every source.", "max_model_steps": 64
            })
        ),
        bot(
            "codex",
            "Builder",
            "green",
            "diamond",
            "Keep changes small and run the tests.",
            &json!({
                "harness": "codex", "model": "gpt-5-codex", "settings": {"effort": "high"},
                "access": "accept_edits"
            })
        ),
    ]);
    let mut defaults = bots.get(0).and_then(|bot| bot.get("config")).cloned(); // rules-allow: defaults and the Bot are independent fixture records
    if let Some(config) = defaults
        .as_mut()
        .and_then(|defaults| defaults.get_mut("config"))
        .and_then(Value::as_object_mut)
    {
        config.remove("harness");
    }
    this.client.state.gateway = catalog(
        "typed offline harness catalog",
        json!({
            "gateway_version": "preview", "machine_name": "Local preview", "computer_view": "unavailable",
            "sessions": [], "background_approvals": [], "providers": [], "provider_instances": [],
            "agent_catalog": {
                "bot_defaults": defaults, "default_capabilities": null, "models": [],
                "model_providers": {}, "image_models": [], "voice_models": [],
                "middleware_features": [{"id": "memory", "label": "Memory",
                    "description": "Remember short facts across this Bot's chats.",
                    "required": false, "settings": []}],
                "extensions": [], "contributions": [{"capability": "memory", "count": null,
                    "commands": [], "references": [], "widgets": [{"id": "navigation",
                        "slot": "navigation", "text": "Memory", "tone": "neutral",
                        "symbol": "brain", "icon_only": false, "progress": null,
                        "content": null, "action": null}]}]
            },
            "harnesses": [mobius],
            "max_active_sessions": 1, "revisions": {}, "omitted": [],
            "session_file_limits": mobius::backend::session_files::session_file_limits(),
            "bots": bots
        }),
    );
}

/// The live connection, catalog and chats an offline fixture replaces.
type Live = (
    Option<crate::gateway::Gateway>,
    Option<mobius_gateway::wire::ReadyPayload>,
    Vec<mobius_gateway::wire::SessionRecord>,
);

thread_local! {
    /// The live connection, catalog and chats while an offline fixture is shown.
    static LIVE: std::cell::RefCell<Option<Live>> =
        const { std::cell::RefCell::new(None) };
}
