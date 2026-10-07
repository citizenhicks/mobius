use std::sync::Arc;

use mobius::backend::model::provider::HostedWebSearch;

use crate::bots::BotStore;
use crate::config::{ConfigStore, CredentialStore};
use crate::wire::{ProviderEndpointAuth, ProviderTint};

use super::*;

fn selection(instance: &str, provider: &str, model: &str) -> ProviderConfig {
    ProviderConfig {
        tool_discovery: None,
        instance: instance.into(),
        provider: provider.into(),
        model: model.into(),
        base_url: Some("https://gateway.example/v1".into()),
        endpoint_auth: ProviderEndpointAuth::Credentialless,
        reasoning_effort: None,
        service_tier: None,
        web_search: HostedWebSearch::Off,
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
                    id: secondary.model.clone(),
                    reasoning_efforts: Some(Vec::new()),
                    default_reasoning: None,
                    ..Default::default()
                }],
                Vec::new(),
            )
            .expect("secondary provider");
    }
    store.save(&config).expect("save config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
    (root, gateway)
}

#[tokio::test]
async fn saved_explicit_custom_image_route_remains_available_after_restart() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let (store, config) = ConfigStore::initialize(
        root.path().join("state"),
        "127.0.0.1:8741".parse().expect("listen"),
        None,
    )
    .expect("config");
    let mut provider = selection("images", "responses", "chat-model");
    provider.endpoint_auth = ProviderEndpointAuth::ProviderDefault;
    provider.base_url = Some("http://127.0.0.1:1/v1".into());
    let mut config = config
        .registering_provider(
            provider,
            "Custom".into(),
            Default::default(),
            vec![crate::wire::ConfiguredModel {
                id: "chat-model".into(),
                reasoning_efforts: Some(Vec::new()),
                default_reasoning: None,
                ..Default::default()
            }],
            vec!["custom-image".into()],
        )
        .expect("explicit image opt-in");
    let default = &mut config.bot_defaults.as_mut().expect("defaults").config;
    default.middleware.set_enabled("image_generation", true);
    default.middleware.set_setting(
        "image_generation",
        "model",
        Some(mobius::protocol::FrontendSettingValue::String(
            "images::custom-image::default".into(),
        )),
    );
    store.save(&config).expect("saved image selection");
    let (store, config) = ConfigStore::open(store.state_dir().to_owned())
        .expect("saved configuration stays readable");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    credentials
        .set(
            "images",
            "responses",
            "test-key",
            Some("http://127.0.0.1:1/v1"),
            None,
        )
        .expect("credential");
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("saved image does not block startup");
    let ready = gateway.ready().await.expect("catalog");
    assert_eq!(
        ready
            .image_models
            .iter()
            .map(|model| model.route.as_str())
            .collect::<Vec<_>>(),
        ["images::custom-image::default"]
    );
    assert!(ready.voice_models.is_empty());
    let bot = ready.bots.into_iter().next().expect("Bot");
    let host = gateway
        .create_session(&workspace, &bot.id)
        .await
        .expect("selected image route assembles");
    assert!(host.is_alive());
    gateway.shutdown().await;
}

#[tokio::test]
async fn saved_unknown_voice_keeps_chat_usable_and_never_selects_a_different_voice() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let (store, config) = ConfigStore::initialize(
        root.path().join("state"),
        "127.0.0.1:8741".parse().expect("listen"),
        None,
    )
    .expect("config");
    let selection = AgentComposition::default().provider;
    let config = config
        .registering_provider(
            selection,
            "Native".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("native provider");
    let mut secondary = AgentComposition::default().provider;
    secondary.instance = "voice-proxy".into();
    let config = config
        .registering_provider(
            secondary,
            "Voice proxy".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("voice provider");
    let mut config = config;
    config
        .bot_defaults
        .as_mut()
        .expect("defaults")
        .config
        .realtime_voice = Some("cedar".into());
    store.save(&config).expect("save gateway");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    for configured in config.configured_providers.values() {
        let selected = &configured.selection;
        credentials
            .set(
                &selected.instance,
                &selected.provider,
                "test-key",
                selected.base_url.as_deref(),
                None,
            )
            .expect("credential");
    }
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let mut composition = config
        .bot_defaults
        .as_ref()
        .expect("defaults")
        .config
        .clone();
    composition.realtime_voice = Some("cedar".into());
    let bot = bots
        .create_bot("Voice", "Test saved voice selection.", composition)
        .expect("saved bare voice");
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway remains usable");
    let host = gateway
        .create_session(&workspace, &bot.id)
        .await
        .expect("chat remains usable");
    let error = host
        .realtime_model()
        .await
        .err()
        .expect("unknown voice is rejected");
    assert!(error.message.contains("cedar") && error.message.contains("select an available voice"));
    assert!(host.is_alive());
    let mut edited = bot.config.config.clone();
    edited.system_prompt.push_str("\nUpdated instructions.");
    gateway
        .configure_bot_defaults(1, edited.clone())
        .await
        .expect("unrelated defaults edit retains saved voice");
    assert!(
        gateway
            .create_bot("Invalid", "Test invalid voice defaults.")
            .await
            .is_err()
    );
    let mut bot = gateway
        .update_bot(
            &bot.id,
            bot.config.revision,
            crate::bots::BotIdentity {
                name: &bot.name,
                description: &bot.description,
                tint: bot.tint,
                shape: bot.shape,
            },
            edited,
        )
        .await
        .expect("unrelated Bot edit retains saved voice");
    assert_eq!(bot.config.config.realtime_voice.as_deref(), Some("cedar"));
    let mut invalid = bot.config.config.clone();
    invalid.realtime_voice = Some("invalid-new-voice".into());
    assert!(
        gateway
            .configure_bot_defaults(2, invalid.clone())
            .await
            .is_err()
    );
    assert!(
        gateway
            .update_bot(
                &bot.id,
                bot.config.revision,
                crate::bots::BotIdentity {
                    name: &bot.name,
                    description: &bot.description,
                    tint: bot.tint,
                    shape: bot.shape,
                },
                invalid
            )
            .await
            .is_err()
    );
    for voice in [
        Some("openai_socket::gpt-live-1::cedar"),
        None,
        Some("voice-proxy::gpt-live-1::cedar"),
    ] {
        let mut config = bot.config.config.clone();
        config.realtime_voice = voice.map(str::to_owned);
        bot = gateway
            .update_bot(
                &bot.id,
                bot.config.revision,
                crate::bots::BotIdentity {
                    name: &bot.name,
                    description: &bot.description,
                    tint: bot.tint,
                    shape: bot.shape,
                },
                config,
            )
            .await
            .expect("select voice");
        let model = host.realtime_model().await.expect("voice is available");
        assert_eq!(
            model.voice,
            voice.unwrap_or("openai_socket::gpt-live-1::marin")
        );
    }
    let error = gateway
        .remove_provider("voice-proxy".into())
        .await
        .expect_err("selected voice cannot be removed");
    assert_eq!(error.code, "provider_in_use");
    gateway.shutdown().await;
}

#[tokio::test]
async fn bot_updates_obey_operator_subagent_ceilings_and_persist_larger_trees() {
    let (root, gateway) =
        gateway_with_providers(selection("primary", "responses", "custom-model"), None).await;
    let bot = gateway.bots().await.expect("Bots").remove(0);
    let mut config = bot.config.config.clone();
    for (id, value) in [
        ("max_depth", 24),
        ("max_concurrency", 96),
        ("max_agents", 300),
    ] {
        config.middleware.set_setting(
            "subagents",
            id,
            Some(mobius::protocol::FrontendSettingValue::Integer(value)),
        );
    }
    let identity = || crate::bots::BotIdentity {
        name: &bot.name,
        description: &bot.description,
        tint: bot.tint,
        shape: bot.shape,
    };
    gateway
        .update_bot(&bot.id, bot.config.revision, identity(), config.clone())
        .await
        .expect_err("paired-client update cannot exceed default host ceilings");
    {
        let state = gateway.state.lock().await;
        let mut operator = state.config.lock().expect("gateway configuration");
        operator.execution.subagent_max_depth = 32;
        operator.execution.subagent_max_concurrency = 128;
        operator.execution.subagent_max_agents = 512;
        state
            .store
            .save(&operator)
            .expect("save trusted operator configuration");
    }
    let updated = gateway
        .update_bot(&bot.id, bot.config.revision, identity(), config.clone())
        .await
        .expect("trusted host ceiling permits larger tree");
    config.middleware.set_setting(
        "subagents",
        "max_depth",
        Some(mobius::protocol::FrontendSettingValue::Integer(33)),
    );
    gateway
        .update_bot(&bot.id, updated.config.revision, identity(), config)
        .await
        .expect_err("paired-client update cannot raise the host ceiling");
    let persisted = BotStore::open(&root.path().join("state")).expect("reopen Bot storage");
    assert_eq!(
        persisted.bot(&bot.id).expect("saved Bot").config,
        updated.config
    );
    gateway.shutdown().await;
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
    let config = config
        .registering_provider(
            primary.clone(),
            "Managed".into(),
            ProviderTint::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("provider");
    store.save(&config).expect("save config");
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
            "managed::gpt-image-2.5-sunburst::high".into(),
        )),
    );
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let saved = bots
        .create_bot(
            "Saved route",
            "Keep the selected subagent model.",
            composition.clone(),
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
    crate::assembly::prepare_bot(
        &config,
        bots.bot(&saved.id).expect("saved Bot"),
        &store,
        &credentials,
        mobius::backend::session_files::SessionFileStore::new(store.state_dir(), None),
        0,
        Arc::new(crate::computer_runtime::ComputerConfig::default()),
    )
    .await
    .expect("saved image route survives unavailable credentials");
    let ready = gateway.ready().await.expect("ready without credentials");
    assert!(ready.models.is_empty());
    assert!(!ready.provider_instances[0].configured);
    assert_eq!(bots.bot(&saved.id).expect("unchanged Bot"), saved);
    assert!(
        gateway
            .configure_bot_defaults(1, composition.clone())
            .await
            .is_err()
    );

    gateway
        .set_credential(
            primary.instance.clone(),
            primary.provider.clone(),
            "restored-test-credential".into(),
            primary.base_url.clone(),
            None,
        )
        .await
        .expect("install credential");
    let ready = gateway.ready().await.expect("ready with credentials");
    assert!(ready.models.iter().any(|model| model.route == route));
    assert!(ready.provider_instances[0].configured);
    assert_eq!(bots.bot(&saved.id).expect("unchanged Bot"), saved);

    composition.middleware.set_setting(
        "subagents",
        "model_route",
        Some(mobius::protocol::FrontendSettingValue::String(
            "unconfigured::gpt-6-luna::low".into(),
        )),
    );
    bots.update_bot(
        &saved.id,
        saved.config.revision,
        crate::bots::BotIdentity {
            name: &saved.name,
            description: &saved.description,
            tint: saved.tint,
            shape: saved.shape,
        },
        composition,
    )
    .expect("persist invalid route for startup validation");
    let error = GatewayHost::start(store, config, credentials, bots)
        .await
        .err()
        .expect("unconfigured route must still fail boot");
    assert!(error.to_string().contains("subagents.model_route"));
}

#[tokio::test]
async fn provider_removal_rejects_a_bot_reference() {
    let primary = selection("primary", "openrouter", "openai/gpt-5");
    let removable = selection("secondary", "openrouter", "openai/gpt-5.1");
    let (_root, gateway) = gateway_with_providers(primary.clone(), Some(removable.clone())).await;
    let composition = AgentComposition {
        provider: removable.clone(),
        ..AgentComposition::default()
    };
    let bot = gateway
        .state
        .lock()
        .await
        .bots
        .create_bot("secondary", "Secondary", composition)
        .expect("Bot");

    let error = gateway
        .remove_provider(removable.instance.clone())
        .await
        .expect_err("referenced provider");

    assert_eq!(error.code, "provider_in_use");
    assert!(error.message.contains(&format!("@{}", bot.handle)));
    assert!(
        gateway
            .state
            .lock()
            .await
            .config
            .lock()
            .expect("config")
            .configured_providers
            .contains_key(&removable.instance)
    );
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
        .create_bot("secondary", "Secondary", composition)
        .expect("Bot");
    let state_dir = root.path().join("state");
    let gateway_path = state_dir.join("gateway.toml");
    let bots_path = state_dir.join("bots.sqlite3");
    let gateway_before = std::fs::read(&gateway_path).expect("gateway config before");
    let bots_before = std::fs::read(&bots_path).expect("Bot state before");

    let error = gateway
        .register_provider(
            false,
            crate::config::ProviderRegistration {
                selection: replacement.clone(),
                label: Some("Secondary".into()),
                tint: Some(ProviderTint::default()),
                models: vec![crate::config::ConfiguredModel {
                    id: replacement.model.clone(),
                    reasoning_efforts: Some(Vec::new()),
                    default_reasoning: None,
                    ..Default::default()
                }],
                image_model_ids: Some(Vec::new()),
            },
            false,
            false,
        )
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
