use mobius::backend::model::provider::BaseUrl;
use std::sync::Arc;

use crate::bots::BotStore;
use crate::config::{ConfigStore, CredentialStore};
use crate::wire::{DictationTarget, ProviderEndpointAuth, ProviderTint};

use super::*;

fn selection(instance: &str, provider: &str, model: &str) -> ProviderConfig {
    ProviderConfig {
        base_url: Some(BaseUrl::new("https://gateway.example/v1").unwrap()),
        endpoint_auth: ProviderEndpointAuth::Credentialless,
        ..crate::config::test_provider(instance, provider, model)
    }
}

async fn gateway_with_providers(
    primary: ProviderConfig,
    secondary: Option<ProviderConfig>,
) -> (tempfile::TempDir, GatewayHost) {
    let root = tempfile::tempdir().expect("root");
    let (store, config) = ConfigStore::initialize(
        root.path().join("state"),
        "127.0.0.1:8741".parse().expect("listen"),
        None,
    )
    .expect("config");
    let mut config = config
        .registering_provider(
            primary.clone(),
            "Primary".into(),
            ProviderTint::default(),
            vec![crate::wire::ConfiguredModel {
                id: primary.model.clone(),
                reasoning_efforts: Some(Vec::new()),
                default_reasoning: None,
                ..Default::default()
            }],
            Vec::new(),
        )
        .expect("primary provider");
    if let Some(secondary) = secondary {
        config = config
            .registering_provider(
                secondary.clone(),
                "Secondary".into(),
                ProviderTint::default(),
                vec![crate::wire::ConfiguredModel {
                    id: secondary.model,
                    reasoning_efforts: Some(Vec::new()),
                    default_reasoning: None,
                    ..Default::default()
                }],
                Vec::new(),
            )
            .expect("secondary provider");
    }
    store
        .save(&config.clone().validated().expect("valid config"))
        .expect("save config");
    let gateway = crate::host::tests::start_test_gateway(store, config).await;
    (root, gateway)
}

#[tokio::test]
async fn saved_bot_routes_survive_missing_credentials_without_advertising_unavailable_models() {
    let root = tempfile::tempdir().expect("root");
    let (store, config) = ConfigStore::initialize(
        root.path().join("state"),
        "127.0.0.1:8741".parse().expect("listen"),
        None,
    )
    .expect("config");
    let mut primary = selection("managed", "openai_socket", "gpt-6-luna");
    primary.endpoint_auth = ProviderEndpointAuth::ProviderDefault;
    let mut config = config
        .registering_provider(
            primary.clone(),
            "Managed".into(),
            ProviderTint::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("provider");
    config
        .bot_defaults
        .as_mut()
        .expect("defaults")
        .config
        .middleware
        .set_enabled("voice", true);
    store
        .save(&config.clone().validated().expect("valid config"))
        .expect("save config");
    let route = "managed::gpt-6-luna::low";
    let mut composition = config
        .bot_defaults
        .as_ref()
        .expect("defaults")
        .config
        .clone();
    composition.middleware.set_setting(
        "subagents",
        "model_route",
        Some(mobius::protocol::FrontendSettingValue::String(route.into())),
    );
    composition.middleware.set_setting(
        "image_generation",
        "model",
        Some(mobius::protocol::FrontendSettingValue::String(
            "managed::gpt-image-2.6-sunburst::high".into(),
        )),
    );
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let saved = bots
        .create_bot(
            "Saved route",
            "Keep the selected subagent model.",
            composition.clone().into(),
        )
        .expect("save Bot");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let gateway = GatewayHost::start(
        store.clone(),
        config.clone(),
        Arc::clone(&credentials),
        Arc::clone(&bots),
    )
    .await
    .expect("saved configured route must not require a credential to boot");
    crate::harness::mobius::assembly::prepare_bot(
        &config,
        bots.runtime_bot(&saved.id).expect("saved Bot"),
        &store,
        &credentials,
        mobius::backend::session_files::SessionFileStore::new(store.state_dir(), None).into(),
        0,
        Arc::new(crate::computer_runtime::ComputerConfig::default()),
    )
    .await
    .expect("saved image route survives unavailable credentials");
    let ready = gateway.ready().await.expect("ready without credentials");
    assert!(ready.agent_catalog.models.is_empty());
    assert!(!ready.provider_instances[0].configured);
    assert!(matches!(
        bots.bot(&saved.id).expect("saved Bot").capabilities.voice,
        crate::wire::VoiceCapability::Unavailable { .. }
    ));
    assert_eq!(
        bots.bot(&saved.id).expect("unchanged Bot").config,
        saved.config
    );
    assert!(
        gateway
            .configure_bot_defaults(1, composition.clone())
            .await
            .is_err()
    );

    gateway
        .set_credential(
            &primary.instance,
            &primary.provider,
            "restored-test-credential".into(),
            primary.base_url.clone(),
            None,
        )
        .await
        .expect("install credential");
    assert!(matches!(
        bots.bot(&saved.id).expect("saved Bot").capabilities.voice,
        crate::wire::VoiceCapability::Ready { .. }
    ));
    let ready = gateway.ready().await.expect("ready with credentials");
    gateway
        .clear_credential(&primary.instance)
        .await
        .expect("clear credential");
    assert!(matches!(
        bots.bot(&saved.id).expect("saved Bot").capabilities.voice,
        crate::wire::VoiceCapability::Unavailable { .. }
    ));
    drop(gateway);
    assert!(
        ready
            .agent_catalog
            .models
            .iter()
            .any(|model| *model.route == *route)
    );
    assert!(ready.provider_instances[0].configured);
    assert_eq!(
        bots.bot(&saved.id).expect("unchanged Bot").config,
        saved.config
    );

    composition.middleware.set_setting(
        "subagents",
        "model_route",
        Some(mobius::protocol::FrontendSettingValue::String(
            "unconfigured::gpt-6-luna::low".into(),
        )),
    );
    bots.update_bot(
        &saved.id,
        crate::bots::BotIdentity {
            name: &saved.name,
            description: &saved.description,
            tint: saved.tint,
            shape: saved.shape,
        },
        Arc::new(crate::wire::VersionedAgentConfig {
            revision: saved.config.revision + 1,
            config: crate::wire::HarnessConfig::Mobius(Arc::new(composition)),
        }),
        crate::harness::ApprovalMode::Mobius(std::sync::Arc::new(
            crate::harness::mobius::middleware_manifest::MobiusSettings::parse(
                &crate::harness::mobius::middleware_manifest::default_config(
                    mobius::middleware::subagents::SubagentCeilings::default(),
                ),
            )
            .expect("default settings"),
        )),
    )
    .expect("persist invalid route for startup validation");
    let error = GatewayHost::start(store, config, credentials, bots)
        .await
        .err()
        .expect("unconfigured route must still fail boot");
    assert!(error.to_string().contains("subagents.model_route"));
}

#[tokio::test]
async fn provider_replacement_rejects_a_bot_reference_without_changing_either_store() {
    let primary = selection("primary", "openrouter", "openai/gpt-5");
    let original = selection("secondary", "openrouter", "openai/gpt-5.1");
    let replacement = selection("secondary", "openrouter", "openai/gpt-5.2");
    let (root, gateway) = gateway_with_providers(primary, Some(original.clone())).await;
    let composition = AgentComposition {
        provider: original.clone(),
        ..AgentComposition::default()
    };
    let bot = gateway
        .state
        .lock()
        .await
        .bots
        .create_bot("secondary", "Secondary", composition.into())
        .expect("Bot");
    let state_dir = root.path().join("state");
    let gateway_path = state_dir.join("gateway.toml");
    let bots_path = state_dir.join("bots.sqlite3");
    let gateway_before = std::fs::read(&gateway_path).expect("gateway config before");
    let bots_before = std::fs::read(&bots_path).expect("Bot state before");

    let error = gateway
        .save_provider_form(crate::auth::ProviderAccess::Paired, serde_json::from_value(serde_json::json!({
            "config": replacement, "label": "Secondary",
            "groups":{"language":{"models":[{"id":replacement.model, "values":{"reasoning":""}}]}}
        })).unwrap())
        .await
        .expect_err("referenced provider replacement");

    assert_eq!(error.code, "provider_in_use");
    assert!(error.message.contains("@secondary"));
    assert_eq!(
        std::fs::read(&gateway_path).expect("gateway config after"),
        gateway_before
    );
    assert_eq!(
        std::fs::read(&bots_path).expect("Bot state after"),
        bots_before
    );
    let (_, reopened_gateway) = ConfigStore::open(state_dir.clone()).expect("reopen gateway");
    assert_eq!(
        reopened_gateway.configured_providers["secondary"].selection,
        original
    );
    let reopened_bots = BotStore::open(&state_dir).expect("reopen Bots");
    assert_eq!(reopened_bots.bot(&bot.id).expect("Bot"), bot);
}

#[tokio::test]
async fn paired_form_validates_unsaved_settings_then_persists_the_same_model_defaults() {
    use crate::harness::mobius::provider_catalog;
    use crate::wire::{ProviderForm, ProviderModelKind};
    use mobius::backend::model::ImageAspect;
    use serde_json::json;
    let config = selection("primary", "openrouter", "qa/chat");
    let (root, gateway) = gateway_with_providers(config.clone(), None).await;
    let path = root.path().join("state/gateway.toml");
    let saved = std::fs::read(&path).unwrap();
    let form = |window: i64| -> ProviderForm {
        serde_json::from_value(json!({
            "config": config,
            "groups": {
                "language": {"models":[{"id":"qa/chat","values":{"context_window":window}}]},
                "image": {"values":{"endpoint":"qa/images"},
                          "models":[{"id":"qa/image","values":{"image_size":"2K",
                                      "image_aspect":"landscape","image_quality":"high"}}]}
            }
        }))
        .unwrap()
    };
    let error = gateway
        .prepare_provider_form(crate::auth::ProviderAccess::Paired, form(0))
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "invalid_config");
    gateway
        .prepare_provider_form(crate::auth::ProviderAccess::Paired, form(24576))
        .await
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), saved);
    gateway
        .save_provider_form(crate::auth::ProviderAccess::Paired, form(24576))
        .await
        .unwrap();
    let (_, config) = ConfigStore::open(root.path().join("state")).unwrap();
    let saved = &config.configured_providers["primary"];
    let definition = mobius::backend::model::provider::provider("openrouter").unwrap();
    let groups = provider_catalog::model_groups(
        definition,
        saved.selection.base_url.as_ref().unwrap(),
        Some(saved),
    );
    let context = groups
        .iter()
        .find(|g| g.kind == ProviderModelKind::Language)
        .unwrap()
        .models[0]
        .fields
        .iter()
        .find(|f| f.setting.id == "context_window")
        .unwrap();
    assert!(context.editable);
    assert_eq!(
        context.value,
        mobius::protocol::FrontendSettingValue::Integer(24576)
    );
    let defaults = provider_catalog::image_defaults(definition, saved, "qa/image");
    assert_eq!(defaults.aspect, ImageAspect::Landscape);
    assert_eq!(defaults.size.as_deref(), Some("2K"));
    assert_eq!(defaults.quality.as_deref(), Some("high"));
    assert_eq!(saved.group_endpoints[0].path, "qa/images");
}

#[tokio::test]
async fn paired_context_override_preserves_a_locked_language_catalog() {
    use crate::harness::mobius::provider_catalog;
    use crate::wire::ProviderForm;
    use serde_json::json;
    let (_, gateway) = gateway_with_providers(selection("qa", "openrouter", "qa/chat"), None).await;
    let definition = mobius::backend::model::provider::provider("openai_socket").unwrap();
    assert!(definition.locked_models());
    let primary = definition.models().first().unwrap();
    let selection = crate::config::test_provider("preset", definition.id(), &primary.id);
    let mut models: Vec<_> = definition
        .models()
        .iter()
        .map(|m| json!({"id":m.id,"values":{}}))
        .collect();
    models[0]["values"] = json!({"context_window":87654});
    let draft = |models: &[serde_json::Value]| -> ProviderForm {
        serde_json::from_value(json!({"config":selection, "groups":{"language":{"models":models}}}))
            .unwrap()
    };
    let snapshot = gateway
        .prepare_provider_form(crate::auth::ProviderAccess::Paired, draft(&models))
        .await
        .unwrap();
    let configured = &snapshot.configured_providers["preset"];
    assert_eq!(
        provider_catalog::selected_route(definition, Some(configured), &selection).context_window,
        87654
    );
    assert_eq!(configured.models[0].reasoning, primary.reasoning);
    models[0]["values"]["reasoning"] = json!("unlisted-effort");
    let error = gateway
        .prepare_provider_form(crate::auth::ProviderAccess::Paired, draft(&models))
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "invalid_config");
}

#[tokio::test]
async fn concurrent_form_saves_preserve_both_group_endpoints() {
    use crate::wire::ProviderForm;
    use serde_json::json;
    let selection = selection("qa", "openrouter", "qa/chat");
    let (root, gateway) = gateway_with_providers(selection.clone(), None).await;
    let draft = |group: &str, model: &str| -> ProviderForm {
        serde_json::from_value(json!({
            "config": selection, "groups": { (group): {
                "values": {"endpoint": format!("qa/{group}")}, "models":[{"id":model}]
            }}
        }))
        .unwrap()
    };
    // Queue both submissions behind the same held write lock before either can publish.
    let gate = gateway.begin_exclusive_mutation().await.unwrap();
    let language = gateway.save_provider_form(
        crate::auth::ProviderAccess::Paired,
        draft("language", "qa/chat"),
    );
    let image = gateway.save_provider_form(
        crate::auth::ProviderAccess::Paired,
        draft("image", "qa/image"),
    );
    tokio::pin!(language, image);
    assert!(futures_util::poll!(&mut language).is_pending());
    assert!(futures_util::poll!(&mut image).is_pending());
    drop(gate);
    let (language, image) = tokio::join!(language, image);
    language.unwrap();
    image.unwrap();
    let (_, configured) = ConfigStore::open(root.path().join("state")).unwrap();
    let paths: std::collections::BTreeSet<_> = configured.configured_providers["qa"]
        .group_endpoints
        .iter()
        .map(|e| e.path.as_str())
        .collect();
    assert_eq!(paths, ["qa/image", "qa/language"].into_iter().collect());
}

#[test]
fn submitted_model_group_rejects_misspelled_fields() {
    let draft = serde_json::json!({"models": [], "vaules": {}});
    assert!(serde_json::from_value::<crate::wire::ProviderGroupDraft>(draft).is_err());
}

#[tokio::test]
async fn unavailable_pinned_dictation_is_not_an_active_route() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut config) = ConfigStore::initialize(
        root.path().join("state"),
        "127.0.0.1:8741".parse().unwrap(),
        None,
    )
    .unwrap();
    let missing = DictationTarget::Route {
        id: "removed::asr".into(),
    };
    config.default_dictation = Some(missing.clone());
    let gateway = crate::host::tests::start_test_gateway(store, config).await;
    let ready = gateway.ready().await.unwrap();
    let catalog = &ready.agent_catalog.dictation;
    assert!(catalog.resolved.is_none());
    assert_eq!(catalog.selection.as_ref(), Some(&missing));
    assert!(
        catalog
            .choices
            .iter()
            .any(|choice| choice.target.as_ref() == Some(&missing))
    );
    let reset = gateway.configure_dictation(None).await.unwrap();
    drop(gateway);
    assert_eq!(
        reset.agent_catalog.dictation.resolved,
        Some(DictationTarget::OnDevice {})
    );
}

#[tokio::test]
async fn image_route_survives_resetting_model_defaults() {
    use crate::harness::mobius::provider_catalog::media_routes;
    use crate::wire::ProviderForm;
    let (root, gateway) =
        gateway_with_providers(selection("qa", "openrouter", "qa/chat"), None).await;
    let definition = provider("openai_socket").unwrap();
    let selection =
        crate::config::test_provider("preset", definition.id(), &definition.models()[0].id);
    gateway
        .set_credential("preset", definition.id(), "fixture-only".into(), None, None)
        .await
        .unwrap();
    let images = definition.image_models();
    let model = images
        .iter()
        .find(|preset| !preset.variants.is_empty())
        .unwrap();
    let mut expected = None;
    for aspect in ["landscape", "square"] {
        let models: Vec<_> = images
            .iter()
            .map(|preset| {
                serde_json::json!({
                    "id":preset.id, "values":{"image_aspect":aspect}
                })
            })
            .collect();
        let form: ProviderForm = serde_json::from_value(serde_json::json!({
            "config":selection, "groups":{"image":{"models":models}}
        }))
        .unwrap();
        gateway
            .save_provider_form(crate::auth::ProviderAccess::Paired, form)
            .await
            .unwrap();
        let (_, config) = ConfigStore::open(root.path().join("state")).unwrap();
        let media = media_routes(&config.configured_providers, None).unwrap();
        let route = media
            .images
            .iter()
            .find(|r| r.choice.model.as_ref() == model.id && r.choice.reasoning_effort.is_none())
            .unwrap();
        if let Some(previous) = &expected {
            assert_eq!(previous, &route.choice.route);
            assert!(
                !config.configured_providers["preset"]
                    .image_defaults
                    .contains_key(&model.id)
            );
        } else {
            let mut bot = config.bot_defaults.as_ref().unwrap().config.clone();
            bot.middleware.set_setting(
                "image_generation",
                "model",
                Some(mobius::protocol::FrontendSettingValue::String(
                    route.choice.route.to_string(),
                )),
            );
            gateway
                .state
                .lock()
                .await
                .bots
                .create_bot("Illustrator", "Draw pictures", bot.into())
                .unwrap();
            expected = Some(Arc::clone(&route.choice.route));
        }
    }
}

#[tokio::test]
async fn repeated_voice_ids_in_a_form_keep_the_users_order() {
    use crate::wire::ProviderForm;
    let (_root, gateway) =
        gateway_with_providers(selection("qa", "openrouter", "qa/chat"), None).await;
    let config = selection("asr", "responses", "chat");
    let form: ProviderForm = serde_json::from_value(serde_json::json!({
        "config": config, "groups":{"voice":{"models":[
            {"id":"custom-voice","values":{"voice_ids":"alloy, echo, alloy"}}
        ]}}
    }))
    .unwrap();
    let snapshot = gateway
        .prepare_provider_form(crate::auth::ProviderAccess::Paired, form)
        .await
        .unwrap();
    drop(gateway);
    let voices = snapshot.configured_providers["asr"]
        .voice_models
        .as_ref()
        .unwrap();
    assert_eq!(
        voices[0]
            .variants
            .iter()
            .map(|v| v.id.as_str())
            .collect::<Vec<_>>(),
        ["alloy", "echo"]
    );
}

#[tokio::test]
async fn dictation_form_accepts_only_the_applicable_display_mode_and_restores_native_defaults() {
    use crate::auth::ProviderAccess::Paired;
    use crate::wire::ProviderForm;
    use serde_json::json;

    let (root, gateway) = gateway_with_providers(selection("qa", "openrouter", "chat"), None).await;
    let config = selection("asr", "responses", "chat");
    let form = |path, mode| -> ProviderForm {
        serde_json::from_value(json!({
            "config": config,
            "groups": {"dictation": {
                "values": {"endpoint": path},
                "models": [{"id":"gpt-live-transcribe", "values":{"dictation_mode":mode}}]
            }}
        }))
        .unwrap()
    };

    // Clients may submit the stored value or the first matching display case.
    gateway
        .prepare_provider_form(Paired, form("custom/transcribe", "Live"))
        .await
        .unwrap();
    gateway
        .save_provider_form(Paired, form("custom/transcribe", "After Stop"))
        .await
        .unwrap();
    let (_, stored) = ConfigStore::open(root.path().join("state")).unwrap();
    assert_eq!(
        stored.configured_providers["asr"].group_endpoints[0].path,
        "custom/transcribe"
    );

    gateway
        .save_provider_form(Paired, form("/audio/transcriptions", "Live"))
        .await
        .unwrap();
    let (_, stored) = ConfigStore::open(root.path().join("state")).unwrap();
    assert!(
        stored.configured_providers["asr"]
            .group_endpoints
            .is_empty()
    );

    // Both the native and generic adapter cases match here. Only the first is effective.
    for (path, mode) in [
        ("/audio/transcriptions", "After Stop"),
        ("//audio/transcriptions", "Live"),
        ("/", "Live"),
    ] {
        assert_eq!(
            gateway
                .prepare_provider_form(Paired, form(path, mode))
                .await
                .err()
                .unwrap()
                .code,
            "invalid_config",
            "{path}"
        );
    }
}
