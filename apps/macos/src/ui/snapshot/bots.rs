//! Bots scenarios: Bots, Bot settings, Bot defaults and Extensions. They open pages and
//! dialogs but never submit them, so no Bot, routine, extension or default changes.

use super::*;
use navigation::Section;
use pages::bots::AgentScope;

/// `MOBIUS_SNAPSHOT_BOT`, else @mobius, else the first Bot.
fn bot(this: &Workspace) -> Option<Arc<str>> {
    let bots = &this.client.state.gateway.as_ref()?.bots;
    let wanted = std::env::var("MOBIUS_SNAPSHOT_BOT").ok();
    bots.iter()
        .find(|bot| {
            wanted
                .as_deref()
                .is_some_and(|id| *bot.id == *id || *bot.handle == *id)
        })
        .or_else(|| bots.iter().find(|bot| &*bot.handle == "mobius"))
        .or_else(|| bots.first())
        .map(|bot| Arc::clone(&bot.id))
}

fn settings(
    this: &mut Workspace,
    section: AgentSection,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match bot(this) {
        Some(id) => this.open_page(PageTab::BotSettings(id, section), window, cx),
        None => this.select_section(Section::Bots, window, cx),
    }
}

fn overview(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    match bot(this) {
        Some(id) => this.open_page(PageTab::Bot(id), window, cx),
        None => this.select_section(Section::Bots, window, cx),
    }
}

/// An in-memory typed catalog for the identity form; never connected or saved.
fn identity_fixture(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    use serde_json::json;
    this.client.gateway.take();
    this.client.state.gateway = catalog(
        "typed offline Bot catalog",
        json!({
            "gateway_version": "preview", "machine_name": "Local preview", "computer_view": "unavailable", "sessions": [],
            "background_approvals": [], "providers": [], "provider_instances": [],
            "agent_catalog": {
                "bot_defaults": null, "default_capabilities": null, "models": [],
                "model_providers": {}, "image_models": [], "voice_models": [],
                "middleware_features": [], "extensions": [], "contributions": []
            },
            "harnesses": [], "max_active_sessions": 1, "revisions": {}, "omitted": [],
            "session_file_limits": mobius::backend::session_files::session_file_limits(),
            "bots": [{
                "id": "preview-bot", "handle": "demo", "name": "Demo Bot",
                "description": "An offline Bot settings preview.", "tint": "blue", "shape": "circle",
                "harness": "mobius",
                "conversation_session_id": "preview-conversation", "capabilities": {"attachments": false, "computer": false, "voice": {"state": "off"}, "memory": {"enabled": false, "global": false}},
                "routine_interaction_policy": "unattended",
                "config": {"revision": 1, "config": {
                    "harness": "mobius",
                    "provider": {"instance": "preview", "provider": "preview", "model": "preview",
                        "endpoint_auth": "credentialless", "web_search": "off"},
                    "middleware": {"enabled": [], "settings": {}},
                    "extensions": [], "system_prompt": "", "max_model_steps": 64
                }}
            }]
        }),
    );
    settings(this, AgentSection::Identity, window, cx);
}

/// A provider status: a fixed catalog, or one whose setups list their own model ids.
fn media_provider(id: &str, label: &str, symbol: &str, configurable: bool) -> serde_json::Value {
    use serde_json::json;
    json!({
        "provider": id, "label": label, "symbol": symbol, "description": "",
        "model_ids_configurable": configurable, "auth": "api_key",
        "default_base_url": "https://api.example",
        "web_search": [], "tool_discovery": "native",
        "supported_tool_discovery": ["native"],
        "custom_endpoint_tool_discovery": null,
        "models": if configurable { json!([]) } else { json!([{
            "id": "gpt-6", "label": "GPT-6", "description": "", "context_window": 100,
            "reasoning": [
                {"id": "low", "label": "Low", "description": ""},
                {"id": "high", "label": "High", "description": ""}
            ], "default_reasoning": null, "tool_discovery": "native"
        }]) },
        "image_models": if configurable { json!([]) } else {
            json!([
                {"id": "gpt-image-2.5-sunburst", "label": "GPT Image 2.5 Sunburst", "description": ""},
                {"id": "gpt-image-2.5-flare", "label": "GPT Image 2.5 Flare", "description": ""}
            ])
        },
        "image_model_ids_configurable": configurable,
        "voice_models": if configurable { json!([]) } else {
            json!([{"id": "gpt-live-1", "label": "GPT Live 1", "description": "", "variants": [
                {"id": "alloy", "label": "Alloy", "description": ""},
                {"id": "marin", "label": "Marin", "description": ""},
                {"id": "cedar", "label": "Cedar", "description": ""}
            ]}])
        }
    })
}

/// A configured provider setup; the custom one lists its own chat and image model ids.
fn media_instance(
    id: &str,
    label: &str,
    tint: &str,
    provider: &str,
    model: &str,
) -> serde_json::Value {
    use serde_json::json;
    json!({
        "label": label, "tint": tint, "configured": true,
        "models": if provider == "lab" {
            json!([{"id": model, "label": model, "description": "", "context_window": 100,
                "reasoning": [], "default_reasoning": null, "tool_discovery": "native"}])
        } else {
            json!([])
        },
        "image_model_ids": if provider == "lab" { json!(["flux-2"]) } else { json!([]) },
        "selection": {"instance": id, "provider": provider, "model": model,
            "base_url": null, "endpoint_auth": "provider_default",
            "reasoning_effort": null, "web_search": "off"}
    })
}

/// The optional voice capability, its `model` setting offering every voice route.
fn voice_feature(voices: &[serde_json::Value]) -> serde_json::Value {
    use serde_json::json;
    let options: Vec<_> = voices
        .iter()
        .map(|voice| {
            json!({"value": voice["route"], "label": voice["variant_label"], "description": "",
                "symbol": null, "tone": "neutral", "disables": []})
        })
        .collect();
    json!({
        "id": "voice", "label": "Voice",
        "description": "Talk with the Bot in a live voice call", "required": false,
        "settings": [{
            "id": "model", "label": "Voice", "description": "Voice used for live calls",
            "composer": false, "type": "select", "options": options,
            "unset_label": "Automatic"
        }]
    })
}

/// An in-memory catalog with chat, image and voice routes on two tinted provider setups;
/// never connected or saved.
pub(super) fn media_fixture(this: &mut Workspace) {
    use serde_json::{Value, json};
    this.client.gateway.take();
    let choice = |route: &str,
                  model: &str,
                  variant: Option<&str>,
                  label: Option<&str>,
                  image: bool| {
        json!({
            "route": route, "group": "", "model": model, "reasoning_effort": variant,
            "variant_label": label,
            "context_window": null, "supports_image_input": false,
            "supports_image_generation": image, "supports_realtime_voice": !image && variant.is_some(),
            "tool_discovery": "native"
        })
    };
    // Chat routes leave the variant label to the advertised reasoning labels.
    let models = vec![
        choice("work::gpt-6::low", "gpt-6", Some("low"), None, false),
        choice("work::gpt-6::high", "gpt-6", Some("high"), None, false),
        choice("lab::lab-1::default", "lab-1", None, None, false),
    ];
    let mut images: Vec<Value> = ["sunburst", "flare"]
        .into_iter()
        .flat_map(|tier| {
            [("low", "Low"), ("high", "High")].map(|(quality, label)| {
                let model = format!("gpt-image-2.5-{tier}");
                let route = format!("work::{model}::{quality}");
                choice(&route, &model, Some(quality), Some(label), true)
            })
        })
        .collect();
    images.push(choice("lab::flux-2::default", "flux-2", None, None, true));
    let voices: Vec<Value> = [("alloy", "Alloy"), ("marin", "Marin"), ("cedar", "Cedar")]
        .into_iter()
        .map(|(voice, label)| {
            let route = format!("work::gpt-live-1::{voice}");
            choice(&route, "gpt-live-1", Some(voice), Some(label), false)
        })
        .collect();
    let model_providers: serde_json::Map<String, Value> = models
        .iter()
        .chain(&images)
        .chain(&voices)
        .filter_map(|choice| choice["route"].as_str())
        .map(|route| (route.to_owned(), json!(route.split("::").next())))
        .collect();
    let image_options: Vec<Value> = images
        .iter()
        .map(|image| {
            json!({"value": image["route"], "label": image["model"], "description": "",
                "symbol": null, "tone": "neutral", "disables": []})
        })
        .collect();
    let feature = json!({
        "id": "image_generation", "label": "Image generation",
        "description": "Generate images.", "required": false,
        "settings": [{
            "id": "model", "label": "Model", "description": "The image model to use.",
            "composer": false, "type": "select", "options": image_options,
            "unset_label": "Automatic"
        }]
    });
    let voice = voice_feature(&voices);
    let bot = json!({
        "id": "preview-bot", "handle": "demo", "name": "Demo Bot",
        "description": "An offline media preview.", "tint": "blue", "shape": "circle", "harness": "mobius",
        "conversation_session_id": "preview-conversation", "routine_interaction_policy": "unattended",
        "capabilities": {"attachments": false, "computer": false, "voice": {"state": "ready",
            "route": "work::gpt-live-1::marin", "waits_for_session_started": true}, "memory": {"enabled": false, "global": false}},
        "config": {"revision": 1, "config": {"harness": "mobius",
            "provider": {"instance": "work", "provider": "openai", "model": "gpt-6",
                "reasoning_effort": "high", "endpoint_auth": "provider_default", "web_search": "off"},
            "middleware": {"enabled": ["image_generation", "voice"],
                "settings": {"image_generation": {"model": "work::gpt-image-2.5-flare::high"},
                    "voice": {"model": "work::gpt-live-1::marin"}}},
            "extensions": [], "system_prompt": "", "max_model_steps": 64
        }}
    });
    this.client.state.gateway = catalog(
        "typed offline media catalog",
        json!({
            "gateway_version": "preview", "machine_name": "Local preview", "computer_view": "unavailable",
            "sessions": [], "background_approvals": [],
            "providers": [
                media_provider("openai", "OpenAI", "chat_gpt", false),
                media_provider("lab", "Custom", "route", true)
            ],
            "provider_instances": [
                media_instance("work", "Work", "purple", "openai", "gpt-6"),
                media_instance("lab", "Lab", "orange", "lab", "lab-1")
            ],
            "agent_catalog": {
                "bot_defaults": null, "default_capabilities": null, "models": models,
                "image_models": images, "voice_models": voices, "model_providers": model_providers,
                "middleware_features": [feature, voice], "extensions": [], "contributions": []
            },
            "harnesses": [], "max_active_sessions": 1, "revisions": {}, "omitted": [],
            "session_file_limits": mobius::backend::session_files::session_file_limits(),
            "bots": [bot]
        }),
    );
}

fn conversation(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let session = bot(this).and_then(|id| {
        this.client
            .state
            .bot(&id)
            .map(|bot| bot.conversation_session_id.share())
    });
    if let Some(session) = session {
        this.open_page(PageTab::Chat(session), window, cx);
    }
}

#[expect(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "a self-checking scenario: a failed step or missing probe is the failure report, as in tests"
)]
fn conversation_pointer(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    window.resize(size(px(860.), px(720.)));
    conversation(this, window, cx);
    Workspace::when_chat_ready(window, cx, |this, window, cx| {
        this.apply_fixture(crate::ui::chat::Scene::Transcript, cx);
        cx.spawn_in(window, async move |this, cx| {
            let scope = this
                .read_with(cx, |this, _| {
                    format!("chat:{}", this.selected_chat().unwrap_or_default())
                })
                .expect("pointer fixture chat");
            wait(cx.background_executor(), 100).await;
            this.update(cx, |this, cx| {
                this.fixture_scroll_transcript(&scope, -height::BOT_CHAT_HEADER);
                cx.notify();
            })
            .expect("scroll behind header");
            wait(cx.background_executor(), 100).await;
            let key = "selection:assistant:answer:FinalAnswer";
            let target = cx
                .update(|window, cx| {
                    window.draw(cx).clear(cx);
                    let bounds = PROBES.with(|probes| probes.borrow()[key]);
                    bounds.bottom() - glyph::INLINE
                        + (window.viewport_size().height + height::TITLEBAR) / 2.
                        - (height::TITLEBAR + height::BOT_CHAT_HEADER - space::L)
                })
                .expect("copy target position");
            this.update(cx, |this, cx| {
                this.fixture_center_transcript(&scope, target);
                cx.notify();
            })
            .expect("copy action beneath header");
            wait(cx.background_executor(), 100).await;
            let (position, before) = cx
                .update(|window, cx| {
                    window.draw(cx).clear(cx);
                    let bounds = PROBES.with(|probes| probes.borrow()[key]);
                    let position = point(
                        bounds.left() + glyph::INLINE,
                        bounds.bottom() - glyph::INLINE,
                    );
                    assert!(
                        position.y < height::TITLEBAR + height::BOT_CHAT_HEADER,
                        "copy position {position:?}, prose {bounds:?}"
                    );
                    let clipboard = cx.read_from_clipboard();
                    click(position, window, cx);
                    let copied = cx.read_from_clipboard().and_then(|item| item.text());
                    cx.write_to_clipboard(
                        clipboard.unwrap_or_else(|| ClipboardItem::new_string(String::new())),
                    );
                    assert!(
                        copied.is_some_and(|text| text.starts_with("The composer now uses")),
                        "blank header must pass the copy click through"
                    );
                    window.dispatch_event(
                        PlatformInput::ScrollWheel(ScrollWheelEvent {
                            position,
                            delta: ScrollDelta::Pixels(point(px(0.), -space::L)),
                            modifiers: Modifiers::default(),
                            touch_phase: TouchPhase::Moved,
                        }),
                        cx,
                    );
                    (position, bounds.top())
                })
                .expect("pointer events through blank header");
            wait(cx.background_executor(), 100).await;
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                let after = PROBES.with(|probes| probes.borrow()[key].top());
                assert!(
                    after < before,
                    "blank header must pass the scroll at {position:?} through"
                );
            })
            .expect("pointer scroll frame");
            eprintln!("bot-conversation-pointer: copy click and wheel passed through blank header");
        })
        .detach();
    });
}

fn face_gallery(window: &mut Window, cx: &mut Context<Workspace>) {
    use mobius_gateway::wire::BotShape;
    const MOODS: [BotMood; 4] = [
        BotMood::Idle,
        BotMood::Thinking,
        BotMood::Sleepy,
        BotMood::Happy,
    ];
    window.open_dialog(cx, |dialog, window, cx| {
        let rows = BotShape::ALL
            .into_iter()
            .zip(Accent::ALL)
            .map(|(shape, accent)| {
                let tint = accent.color();
                let lively = MOODS.map(|mood| {
                    let key = format!("gallery:{shape:?}:{mood:?}");
                    lively_face(key, tint, shape, mood, face::FORM, window, cx)
                });
                div()
                    .flex()
                    .items_center()
                    .gap(space::L)
                    .py(space::S)
                    .children(lively)
                    .child(BotFace::new(tint, shape, face::ROW))
                    .child(BotFace::new(tint, shape, face::SIDEBAR))
            });
        let picker = shape_picker(
            "gallery-shapes",
            BotShape::Star,
            Accent::Purple.color(),
            |_, _, _| {},
        );
        dialog
            .w(width::DIALOG_FORM)
            .child(dialog_body().children(rows).child(picker))
    });
}

fn first_extension(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    this.select_section(Section::Mobius, window, cx);
    if let Some(id) = this.first_extension_id() {
        this.open_page(PageTab::Extension(id), window, cx);
    }
}

pub(super) const SCENARIOS: &[Scenario] = &[
    ("bot-conversation", conversation),
    ("bot-conversation-pointer", conversation_pointer),
    ("bot-conversation-face-click", |this, window, cx| {
        let Some(id) = bot(this) else { return };
        this.new_chat(window, cx);
        click_probe(
            format!("bot-conversation-face:{id}"),
            face::HERO / 2.,
            MouseButton::Left,
            window,
            cx,
        );
    }),
    ("bot-conversation-narrow", |this, window, cx| {
        window.resize(size(px(860.), px(720.)));
        conversation(this, window, cx);
    }),
    ("bot-conversation-scrolled", |this, window, cx| {
        window.resize(size(px(860.), px(720.)));
        conversation(this, window, cx);
        Workspace::when_chat_ready(window, cx, |this, window, cx| {
            this.apply_fixture(crate::ui::chat::Scene::Transcript, cx);
            let scope = format!("chat:{}", this.selected_chat().unwrap_or_default());
            // Layout the long fixture first, then scroll its text behind the fixed header.
            let view = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                let scrolled = view.update(cx, |this, cx| {
                    this.fixture_scroll_transcript(&scope, -height::BOT_CHAT_HEADER);
                    cx.notify();
                    cx.notify();
                });
                report("bot-conversation-scrolled", scrolled);
            });
        });
    }),
    ("bots-overview", overview),
    ("bots-identity", |this, window, cx| {
        settings(this, AgentSection::Identity, window, cx);
    }),
    ("bots-identity-preview", identity_fixture),
    ("bots-prompt", |this, window, cx| {
        settings(this, AgentSection::Prompt, window, cx);
    }),
    ("bots-model", |this, window, cx| {
        settings(this, AgentSection::Model, window, cx);
    }),
    ("perf-bot-model", |this, window, cx| {
        settings(this, AgentSection::Model, window, cx);
    }),
    ("perf-bot-capabilities", |this, window, cx| {
        settings(this, AgentSection::Capabilities, window, cx);
    }),
    ("bots-model-picker", |this, window, cx| {
        settings(this, AgentSection::Model, window, cx);
        if let Some(id) = bot(this) {
            this.snapshot_agent_model_picker(&Rc::new(AgentScope::Bot(id)), false, cx);
        }
    }),
    ("bots-capabilities", |this, window, cx| {
        if let Some(id) = bot(this) {
            this.disclose_first_capability(&AgentScope::Bot(id));
        }
        settings(this, AgentSection::Capabilities, window, cx);
    }),
    ("bots-defaults-prompt", |this, window, cx| {
        this.open_page(PageTab::AgentDefaults(AgentSection::Prompt), window, cx);
    }),
    ("bots-defaults-model", |this, window, cx| {
        this.open_page(PageTab::AgentDefaults(AgentSection::Model), window, cx);
        this.snapshot_agent_model_picker(&Rc::new(AgentScope::Defaults), false, cx);
    }),
    ("bots-defaults-capabilities", |this, window, cx| {
        this.open_page(
            PageTab::AgentDefaults(AgentSection::Capabilities),
            window,
            cx,
        );
    }),
    // Every face shape in the moods that move differently, beside its list sizes.
    ("bot-faces", |_, window, cx| face_gallery(window, cx)),
    ("bots-create-dialog", |this, window, cx| {
        overview(this, window, cx);
        this.create_bot_dialog(window, cx);
    }),
    ("bots-routine-dialog", |this, window, cx| {
        overview(this, window, cx);
        if let Some(id) = bot(this) {
            this.routine_dialog(id, None, window, cx);
        }
    }),
    ("bots-delete-confirm", |this, window, cx| {
        this.select_section(Section::Bots, window, cx);
        let id = this.client.state.gateway.as_ref().and_then(|gateway| {
            gateway
                .bots
                .iter()
                .find(|bot| &*bot.handle != "mobius")
                .map(|bot| Arc::clone(&bot.id))
        });
        if let Some(id) = id {
            this.open_page(PageTab::Bot(Arc::clone(&id)), window, cx);
            this.delete_bot(&id, window, cx);
        }
    }),
    ("extensions-detail", first_extension),
    ("extensions-install-dialog", |this, window, cx| {
        first_extension(this, window, cx);
        this.install_extension_dialog(window, cx);
    }),
    // Last: the unsaved draft outlives the scenario.
    ("bots-unsaved", |this, window, cx| {
        settings(this, AgentSection::Identity, window, cx);
        if let Some(id) = bot(this) {
            this.snapshot_edit_draft(&AgentScope::Bot(id), window, cx);
        }
    }),
    ("bots-unsaved-narrow", |this, window, cx| {
        window.resize(size(px(760.), px(720.)));
        settings(this, AgentSection::Identity, window, cx);
        if let Some(id) = bot(this) {
            this.snapshot_edit_draft(&AgentScope::Bot(id), window, cx);
        }
    }),
    ("bots-overview-narrow", |this, window, cx| {
        window.resize(size(px(760.), px(720.)));
        overview(this, window, cx);
    }),
    ("bots-model-unsaved", |this, window, cx| {
        model_unsaved(this, false, window, cx);
    }),
    ("bots-model-unsaved-picker", |this, window, cx| {
        model_unsaved(this, true, window, cx);
    }),
    ("bots-subagent-model", |this, window, cx| {
        subagent_model(this, false, window, cx);
    }),
    ("bots-subagent-model-list", |this, window, cx| {
        subagent_model(this, true, window, cx);
    }),
    ("bots-media-model", |this, window, cx| {
        media_fixture(this);
        settings(this, AgentSection::Model, window, cx);
    }),
    ("bots-media-chat", |this, window, cx| {
        media_fixture(this);
        settings(this, AgentSection::Model, window, cx);
        this.snapshot_agent_model_picker(&media_scope(), false, cx);
    }),
    ("bots-media-voice", |this, window, cx| {
        media_voice(this, false, window, cx);
    }),
    ("bots-media-voice-list", |this, window, cx| {
        media_voice(this, true, window, cx);
    }),
    ("bots-media-image", |this, window, cx| {
        media_image(this, false, window, cx);
    }),
    ("bots-media-image-list", |this, window, cx| {
        media_image(this, true, window, cx);
    }),
];

/// The offline media catalog's Bot.
fn media_scope() -> Rc<AgentScope> {
    Rc::new(AgentScope::Bot("preview-bot".into()))
}

/// The offline media catalog's Voice capability with its picker open.
fn media_voice(this: &mut Workspace, list: bool, window: &mut Window, cx: &mut Context<Workspace>) {
    media_fixture(this);
    settings(this, AgentSection::Capabilities, window, cx);
    this.snapshot_voice_picker(&media_scope(), list, cx);
}

/// The offline media catalog's image generation model picker.
fn media_image(this: &mut Workspace, list: bool, window: &mut Window, cx: &mut Context<Workspace>) {
    media_fixture(this);
    settings(this, AgentSection::Capabilities, window, cx);
    this.snapshot_setting_picker(&media_scope(), list, cx);
}

/// The Model section after picking another model, which waits for Save.
fn model_unsaved(
    this: &mut Workspace,
    picker: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    settings(this, AgentSection::Model, window, cx);
    let Some(id) = bot(this) else {
        return;
    };
    let scope = Rc::new(AgentScope::Bot(id));
    this.snapshot_reset_draft(&scope);
    this.snapshot_draft_model(&scope, window, cx);
    if picker {
        this.snapshot_agent_model_picker(&scope, false, cx);
    }
}

/// The subagents' default model with the shared picker open, tall enough to show its row.
fn subagent_model(
    this: &mut Workspace,
    list: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    window.resize(size(px(1600.), px(1400.)));
    settings(this, AgentSection::Capabilities, window, cx);
    if let Some(id) = bot(this) {
        let scope = Rc::new(AgentScope::Bot(id));
        this.snapshot_reset_draft(&scope);
        this.snapshot_setting_picker(&scope, list, cx);
    }
}
