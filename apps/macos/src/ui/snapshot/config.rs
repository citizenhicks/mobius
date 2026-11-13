//! Config scenarios: Gateway, Providers, Settings and Event Centre.
//! Steps only navigate or open dialogs; they never submit, pair or change settings.

use super::Scenario;
use super::capture::catalog;
use super::probes::{click_probe, undo_after_capture};

use crate::host::platform::{CloudAge, CloudErrorKind, CloudStage, CloudStatus};
use crate::ui::navigation::Section;

use crate::ui::router::{GatewayPage, GatewayRef, PageTab, SettingsPage};
use crate::ui::theme::Accent;
use crate::ui::{Workspace, navigation, theme};

use gpui::{Context, Keystroke, MouseButton, Window, px};

use std::sync::Arc;

fn settings(
    this: &mut Workspace,
    page: SettingsPage,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    this.open_page(PageTab::Settings(page), window, cx);
}

/// The Account page with local Cloud fixture state; the Cloud service is never called.
fn cloud(
    this: &mut Workspace,
    status: CloudStatus,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    settings(this, SettingsPage::Account, window, cx);
    this.set_cloud_status(status);
}

fn signed_in(subscribed: bool) -> CloudStatus {
    CloudStatus {
        signed_in: true,
        name: subscribed.then(|| "Ada Lovelace".into()),
        email: Some("ada@example.com".into()),
        subscribed,
        plan: subscribed.then(|| "Cloud Plus".into()),
        subscription: subscribed.then(|| "Active".into()),
        gateway: subscribed.then(|| "ready".into()),
        age: Some(CloudAge::Eligible),
        ..CloudStatus::default()
    }
}

/// A subscribed account in each age state; only an adult decision shows the gateway row.
fn age(age: CloudAge) -> CloudStatus {
    CloudStatus {
        age: Some(age),
        ..signed_in(true)
    }
}

fn failed(kind: CloudErrorKind, message: &str) -> CloudStatus {
    CloudStatus {
        error_kind: Some(kind),
        error: Some(message.into()),
        ..CloudStatus::default()
    }
}

/// Rebinds New Chat to ⌃⌘N in memory; snapshot runs never save preferences.
fn custom_shortcut(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    settings(this, SettingsPage::Shortcuts, window, cx);
    this.record_shortcut("new_chat", window, cx);
    if let Ok(keystroke) = Keystroke::parse("ctrl-cmd-n") {
        this.recorded(&keystroke, window, cx);
    }
}

/// A local approval and routine result, so the Event Centre has something to show.
fn event_fixtures(this: &mut Workspace) {
    use mobius_gateway::wire::{BackgroundApproval, RoutineRun, RoutineRunStatus};
    let Some(session) = this
        .client
        .state
        .sessions
        .first()
        .map(|s| s.session_id.share())
    else {
        return;
    };
    let Some(gateway) = &mut this.client.state.gateway else {
        return;
    };
    let bot = gateway
        .bots
        .first()
        .map_or_default(|bot| Arc::clone(&bot.id));
    gateway
        .background_approvals
        .push(Arc::new(BackgroundApproval {
            session_id: session.share(),
            bot_id: Arc::clone(&bot),
            turn_id: "fixture-turn".into(),
            request_id: "fixture-approval".into(),
        }));
    let now = chrono::Utc::now().timestamp();
    this.client.state.routine_history.push(RoutineRun {
        id: "fixture-run".into(),
        routine_id: "fixture-routine".into(),
        bot_id: bot,
        started_at: now - 3_700,
        finished_at: Some(now - 3_600),
        status: RoutineRunStatus::Failed,
        session_id: Some(session),
        message: Some("The workspace folder is missing.".into()),
    });
}

const SOL_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// One model as a catalog or a setup lists it, with `levels` in catalog order.
fn fixture_model(
    id: &str,
    label: &str,
    default: Option<&str>,
    levels: &[&str],
) -> serde_json::Value {
    use serde_json::json;
    let reasoning: Vec<serde_json::Value> = [
        ("none", "None"),
        ("low", "Low"),
        ("medium", "Medium"),
        ("high", "High"),
        ("xhigh", "Extra high"),
        ("max", "Maximum"),
    ]
    .iter()
    .filter(|(level, _)| levels.contains(level))
    .map(|(level, label)| json!({"id": level, "label": label, "description": ""}))
    .collect();
    json!({"id": id, "label": label, "description": "", "context_window": 1_048_576,
        "reasoning": reasoning, "default_reasoning": default, "tool_discovery": "native"})
}

/// Kimi's catalog, whose models rebuild context when tools are discovered.
fn kimi_models() -> Vec<serde_json::Value> {
    let mut models = vec![
        fixture_model("kimi-k3", "Kimi K3", Some("max"), &["low", "high", "max"]),
        fixture_model("kimi-k2.7-code", "Kimi K2.7 Code", None, &[]),
    ];
    for model in &mut models {
        model["tool_discovery"] = "rebuild".into();
    }
    models
}

/// The OpenRouter, Kimi and Codex catalogs as the core's provider TOMLs define them;
/// only Codex is locked.
fn provider_statuses() -> [serde_json::Value; 3] {
    use serde_json::{Value, json};
    let model = fixture_model;
    // Hosted search modes as provider.toml labels them.
    let searches = |modes: &[&str]| -> Vec<Value> {
        [
            ("off", "Off", "Do not use provider-hosted web search"),
            ("cached", "Cached", "Allow cached provider-hosted search"),
            ("live", "Live", "Allow live provider-hosted search"),
        ]
        .into_iter()
        .filter(|(value, _, _)| modes.contains(value))
        .map(|(value, label, description)| {
            json!({"value": value, "label": label, "description": description,
                "symbol": null, "tone": "neutral", "disables": []})
        })
        .collect()
    };
    let status = |id: &str,
                  label: &str,
                  symbol: &str,
                  models: Vec<Value>,
                  web: Vec<Value>,
                  discovery: &str| {
        json!({
            "provider": id, "label": label, "symbol": symbol, "description": "",
            "model_ids_configurable": id != "openai_codex", "auth": "api_key",
            "default_base_url": "https://api.example",
            "web_search": web, "tool_discovery": discovery,
            "supported_tool_discovery": [discovery],
            "custom_endpoint_tool_discovery": null, "models": models, "image_models": [],
            "image_model_ids_configurable": false, "voice_models": []
        })
    };
    let sol_levels = SOL_LEVELS;
    [
        status(
            "openrouter",
            "OpenRouter",
            "route",
            Vec::new(),
            searches(&["off", "live"]),
            "native",
        ),
        status(
            "kimi",
            "Kimi",
            "kimi",
            kimi_models(),
            searches(&["off"]),
            "rebuild",
        ),
        status(
            "openai_codex",
            "Codex",
            "chat_gpt",
            vec![
                model("gpt-6.1-sol", "6.1 Sol", Some("medium"), &sol_levels),
                model(
                    "gpt-6-luna",
                    "6 Luna",
                    Some("medium"),
                    &["none", "low", "medium", "high", "xhigh", "max"],
                ),
                model("gpt-6-astra", "6 Astra", Some("medium"), &sol_levels),
            ],
            searches(&["off", "cached", "live"]),
            "native",
        ),
    ]
}

const ROUTER: &str = "7210ee4a-5d0a-41e9-b439-888ea396bdde";
const LUNA: &str = "openai/gpt-6-luna";

/// The local gateway's four provider setups as its ready payload lists them.
fn fixture_setups() -> [serde_json::Value; 4] {
    use serde_json::json;
    let setup = |id: &str,
                 label: &str,
                 tint: &str,
                 provider: &str,
                 model: &str,
                 models: Vec<serde_json::Value>,
                 effort: &str| {
        json!({
            "label": label, "tint": tint, "configured": true, "models": models,
            "selection": {"instance": id, "provider": provider, "model": model,
                "base_url": null, "endpoint_auth": "provider_default",
                "reasoning_effort": effort,
                "web_search": if provider == "kimi" { "off" } else { "live" }}
        })
    };
    [
        setup(
            ROUTER,
            "OpenRouter",
            "green",
            "openrouter",
            LUNA,
            vec![fixture_model(
                LUNA,
                LUNA,
                Some("low"),
                &["low", "medium", "high"],
            )],
            "low",
        ),
        setup(
            "kimi",
            "Kimi",
            "blue",
            "kimi",
            "kimi-k3",
            kimi_models(),
            "max",
        ),
        setup(
            "openai_codex",
            "Codex",
            "purple",
            "openai_codex",
            "gpt-6.1-sol",
            Vec::new(),
            "medium",
        ),
        setup(
            "openrouter",
            "OpenRouter",
            "blue",
            "openrouter",
            LUNA,
            vec![fixture_model(
                LUNA,
                LUNA,
                Some("low"),
                &["low", "medium", "high", "max"],
            )],
            "low",
        ),
    ]
}

/// The local gateway's provider setups and the catalogs they use, in memory; never
/// connected or saved. Mirrors the configured state, so the editor shows real data.
fn provider_fixture(this: &mut Workspace) {
    use serde_json::json;
    let sol_levels = SOL_LEVELS;
    let providers = provider_statuses();
    let instances = fixture_setups();
    let mut models = Vec::new();
    let mut model_providers = serde_json::Map::new();
    let mut route = |instance: &str, model: &str, effort: Option<&str>| {
        let id = format!("{instance}::{model}::{}", effort.unwrap_or("default"));
        models.push(json!({
            "route": &id, "group": "", "model": model, "reasoning_effort": effort,
            "variant_label": null, "context_window": null, "supports_image_input": false,
            "supports_image_generation": false, "supports_realtime_voice": false,
            "tool_discovery": "native"
        }));
        model_providers.insert(id, json!(instance));
    };
    for effort in ["low", "medium", "high"] {
        route(ROUTER, LUNA, Some(effort));
    }
    for effort in ["low", "high", "max"] {
        route("kimi", "kimi-k3", Some(effort));
    }
    route("kimi", "kimi-k2.7-code", None);
    for (model, levels) in [
        ("gpt-6.1-sol", sol_levels.as_slice()),
        ("gpt-6-astra", &sol_levels),
    ] {
        for effort in levels {
            route("openai_codex", model, Some(effort));
        }
    }
    for effort in ["low", "medium", "high", "max"] {
        route("openrouter", LUNA, Some(effort));
    }
    this.client.gateway.take();
    this.client.state.gateway = catalog(
        "typed offline provider catalog",
        json!({
            "gateway_version": "preview", "machine_name": "Local preview", "computer_view": "unavailable",
            "sessions": [], "background_approvals": [], "bots": [],
            "providers": providers, "provider_instances": instances,
            "agent_catalog": {
                "bot_defaults": null, "default_capabilities": null,
                "models": models, "image_models": [], "voice_models": [],
                "model_providers": model_providers,
                "middleware_features": [], "extensions": [], "contributions": []
            },
            "harnesses": [], "max_active_sessions": 1, "revisions": {}, "omitted": [],
            "session_file_limits": mobius::backend::session_files::session_file_limits()
        }),
    );
}

fn configurable(
    gateway: &mobius_gateway::wire::ReadyPayload,
    setup: &mobius_gateway::wire::ProviderInstance,
) -> bool {
    gateway
        .providers
        .iter()
        .any(|p| p.provider == setup.selection.provider && p.model_ids_configurable)
}

/// The first live setup whose model IDs are typed in, such as OpenRouter.
fn custom_setup(this: &Workspace) -> Option<String> {
    let gateway = this.client.state.gateway.as_ref()?;
    gateway
        .provider_instances
        .iter()
        .find(|setup| configurable(gateway, setup))
        .map(|setup| setup.selection.instance.clone()) // rules-allow: the page tab owns its id while the catalog keeps the setup
}

pub(super) const SCENARIOS: &[Scenario] = &[
    ("gateway-machine", |this, window, cx| {
        this.select_section(Section::Gateway, window, cx);
    }),
    ("gateway-devices", |this, window, cx| {
        this.open_page(PageTab::Gateway(GatewayPage::Devices), window, cx);
    }),
    ("gateway-github", |this, window, cx| {
        this.open_page(PageTab::Gateway(GatewayPage::GitHub), window, cx);
    }),
    ("gateway-github-dialog", |this, window, cx| {
        this.open_page(PageTab::Gateway(GatewayPage::GitHub), window, cx);
        this.git_credential_dialog(window, cx);
    }),
    ("gateway-ssh", |this, window, cx| {
        this.open_page(PageTab::Gateway(GatewayPage::Ssh), window, cx);
    }),
    ("gateway-pair-dialog", |this, window, cx| {
        this.select_section(Section::Gateway, window, cx);
        this.pair_gateway_dialog(window, cx);
    }),
    ("gateway-rename-dialog", |this, window, cx| {
        this.select_section(Section::Gateway, window, cx);
        this.rename_gateway_dialog(GatewayRef::Local, window, cx);
    }),
    ("providers-edit", |this, window, cx| {
        this.select_section(Section::Mobius, window, cx);
    }),
    ("providers-add-dialog", |this, window, cx| {
        this.select_section(Section::Mobius, window, cx);
        this.add_provider_dialog(window, cx);
    }),
    ("providers-draft", |this, window, cx| {
        let service = this.client.state.gateway.as_ref().and_then(|gateway| {
            let providers = &gateway.providers;
            providers.last()
        });
        let service = service.map(|provider| provider.provider.clone()); // rules-allow: the page tab owns its id while the catalog keeps the provider
        if let Some(service) = service {
            this.open_page(PageTab::ProviderDraft(service), window, cx);
        }
    }),
    ("providers-model-rows", |this, window, cx| {
        provider_fixture(this);
        if let Some(id) = custom_setup(this) {
            this.snapshot_model_rows(&id, None, None, window, cx);
        }
    }),
    ("providers-model-levels", |this, window, cx| {
        provider_fixture(this);
        if let Some(id) = custom_setup(this) {
            this.snapshot_model_rows(&id, Some(0), None, window, cx);
        }
    }),
    ("providers-model-mixed", |this, window, cx| {
        provider_fixture(this);
        if let Some(id) = custom_setup(this) {
            let extra = ("anthropic/claude-sonnet-5", "low, high");
            this.snapshot_model_rows(&id, Some(1), Some(extra), window, cx);
        }
    }),
    ("providers-model-kimi", |this, window, cx| {
        provider_fixture(this);
        this.snapshot_model_rows("kimi", Some(0), None, window, cx);
    }),
    ("providers-model-catalog-open", |this, window, cx| {
        provider_fixture(this);
        this.snapshot_model_rows("openai_codex", Some(0), None, window, cx);
    }),
    ("providers-model-catalog", |this, window, cx| {
        provider_fixture(this);
        let id = this.client.state.gateway.as_ref().and_then(|gateway| {
            gateway
                .provider_instances
                .iter()
                .find(|setup| !configurable(gateway, setup))
                .map(|setup| setup.selection.instance.clone()) // rules-allow: the page tab owns its id while the catalog keeps the setup
        });
        if let Some(id) = id {
            this.open_page(PageTab::Provider(id), window, cx);
        }
    }),
    ("providers-media-editor", |this, window, cx| {
        super::bots::media_fixture(this);
        this.open_page(PageTab::Provider("lab".into()), window, cx);
    }),
    ("providers-media-catalog", |this, window, cx| {
        super::bots::media_fixture(this);
        this.open_page(PageTab::Provider("work".into()), window, cx);
    }),
    ("contribution", |this, window, cx| {
        let key = this.client.state.gateway.as_ref().and_then(|gateway| {
            gateway
                .agent_catalog
                .contributions
                .iter()
                .find_map(|contribution| {
                    let widget = contribution.widgets.iter().find(|widget| {
                        matches!(
                            widget.content,
                            Some(mobius::protocol::FrontendWidgetContent::ActionList { .. })
                        )
                    })?;
                    Some(format!("{}:{}", contribution.capability, widget.id))
                })
        });
        if let Some(key) = key {
            this.open_page(PageTab::Contribution(key), window, cx);
        }
    }),
    ("settings-account", |this, window, cx| {
        settings(this, SettingsPage::Account, window, cx);
    }),
    ("cloud-signed-out", |this, window, cx| {
        cloud(this, CloudStatus::default(), window, cx);
    }),
    ("cloud-signing-in", |this, window, cx| {
        let status = CloudStatus {
            stage: Some(CloudStage::SigningIn),
            ..CloudStatus::default()
        };
        cloud(this, status, window, cx);
    }),
    ("cloud-signed-in", |this, window, cx| {
        cloud(this, signed_in(true), window, cx);
    }),
    ("cloud-unsubscribed", |this, window, cx| {
        cloud(this, signed_in(false), window, cx);
    }),
    ("cloud-age-checking", |this, window, cx| {
        let status = CloudStatus {
            stage: Some(CloudStage::Checking),
            ..age(CloudAge::Checking)
        };
        cloud(this, status, window, cx);
    }),
    ("cloud-age-declined", |this, window, cx| {
        cloud(this, age(CloudAge::Declined), window, cx);
    }),
    ("cloud-underage", |this, window, cx| {
        cloud(this, age(CloudAge::Underage), window, cx);
    }),
    ("cloud-error-sign-in", |this, window, cx| {
        let message =
            "möbius Cloud did not accept this Apple sign-in. Please try signing in again.";
        cloud(this, failed(CloudErrorKind::Other, message), window, cx);
    }),
    ("settings-appearance", |this, window, cx| {
        settings(this, SettingsPage::Appearance, window, cx);
    }),
    ("icons-pink-chat", |this, window, cx| {
        this.accent = Accent::Purple;
        this.refresh_appearance(cx);
        this.client.link = crate::ui::runtime::Link::Ready;
        this.new_chat(window, cx);
        this.fixture_type("Hello", window, cx);
        undo_after_capture(|this| this.accent = Accent::default());
    }),
    ("settings-font-picker", |this, window, cx| {
        settings(this, SettingsPage::Appearance, window, cx);
        click_probe("font-set".into(), px(80.), MouseButton::Left, window, cx);
    }),
    ("settings-appearance-white", |this, window, cx| {
        this.accent = Accent::White;
        this.refresh_appearance(cx);
        undo_after_capture(|this| this.accent = Accent::default());
        settings(this, SettingsPage::Appearance, window, cx);
    }),
    ("settings-appearance-controls", |this, window, cx| {
        settings(this, SettingsPage::Appearance, window, cx);
        this.fixture_color_controls(window, cx);
    }),
    ("settings-appearance-custom", |this, window, cx| {
        this.dark_colors = theme::Colors {
            background: Some(0x11_11_11),
            foreground: Some(0xfc_fc_fc),
        };
        this.light_colors = theme::Colors {
            background: Some(0xff_ff_ff),
            foreground: Some(0x0d_0d_0d),
        };
        this.config.color_edits = [None, None];
        this.refresh_appearance(cx);
        undo_after_capture(|this| {
            this.dark_colors = Default::default();
            this.light_colors = Default::default();
        });
        settings(this, SettingsPage::Appearance, window, cx);
    }),
    ("settings-appearance-editing", |this, window, cx| {
        settings(this, SettingsPage::Appearance, window, cx);
        this.edit_color(false, window, cx);
        undo_after_capture(|this| this.config.color_edits = [None, None]);
    }),
    ("settings-security", |this, window, cx| {
        settings(this, SettingsPage::Security, window, cx);
    }),
    ("settings-data", |this, window, cx| {
        this.shell.notices.clear();
        this.shell.toast = None;
        storage_fixture(this);
        settings(this, SettingsPage::Data, window, cx);
    }),
    ("storage-cleanup", |this, window, cx| {
        this.shell.notices.clear();
        this.shell.toast = None;
        storage_fixture(this);
        settings(this, SettingsPage::Data, window, cx);
        this.open_storage_cleanup(window, cx);
    }),
    ("settings-shortcuts", |this, window, cx| {
        settings(this, SettingsPage::Shortcuts, window, cx);
    }),
    ("settings-shortcuts-recording", |this, window, cx| {
        settings(this, SettingsPage::Shortcuts, window, cx);
        this.record_shortcut("toggle_split", window, cx);
    }),
    ("settings-shortcuts-conflict", |this, window, cx| {
        settings(this, SettingsPage::Shortcuts, window, cx);
        this.record_shortcut("new_tab", window, cx);
        if let Ok(keystroke) = Keystroke::parse("cmd-n") {
            this.recorded(&keystroke, window, cx);
        }
    }),
    ("settings-shortcuts-custom", |this, window, cx| {
        custom_shortcut(this, window, cx);
    }),
    ("settings-shortcuts-palette", |this, window, cx| {
        custom_shortcut(this, window, cx);
        this.command_palette(window, cx);
    }),
    ("settings-usage", |this, window, cx| {
        settings(this, SettingsPage::Usage, window, cx);
    }),
    ("settings-whats-new", |this, window, cx| {
        settings(this, SettingsPage::WhatsNew, window, cx);
    }),
    ("events-list", |this, window, cx| {
        this.select_section(Section::Events, window, cx);
    }),
    ("event-detail", |this, window, cx| {
        event_fixtures(this);
        let first = this.event_items().into_iter().next();
        let page = first.map_or(PageTab::Empty(Section::Events), |item| {
            PageTab::Event(item.id)
        });
        this.open_page(page, window, cx);
    }),
];

fn storage_fixture(this: &mut Workspace) {
    use mobius_gateway::storage_usage::{SessionStorageUsage, StorageSize, StorageUsage};
    this.client.storage.request = Some("snapshot-storage".into());
    let size = |bytes, files| StorageSize {
        bytes,
        files,
        complete: true,
    };
    this.client.storage.usage = Some(StorageUsage {
        version: 1,
        measured_at: 0,
        categories: Vec::new(),
        sessions: this
            .client
            .state
            .sessions
            .iter()
            .take(4)
            .enumerate()
            .map(|(index, session)| SessionStorageUsage {
                session_id: session.session_id.share(),
                project_id: navigation::project_id(session).map(str::to_owned),
                uploads: size(24_000_000 / (index as u64 + 1), 3),
                artifacts: size(8_000_000, 2),
                observations: size(2_000_000, 4),
            })
            .collect(),
        session_count: this.client.state.sessions.len().min(4),
        projects: Vec::new(),
        project_count: 0,
        details_truncated: false,
        gateway_total: size(92_000_000, 40),
        limit_bytes: None,
        used_bytes: 68_000_000,
    });
}
