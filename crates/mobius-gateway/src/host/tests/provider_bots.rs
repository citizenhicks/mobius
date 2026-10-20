use mobius::backend::model::provider::BaseUrl;
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
        base_url: Some(BaseUrl::new("https://gateway.example/v1").unwrap()),
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
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
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
    let config = config
        .registering_provider(
            primary.clone(),
            "Managed".into(),
            ProviderTint::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("provider");
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
            "managed::gpt-image-2.5-sunburst::high".into(),
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
    crate::assembly::prepare_bot(
        &config,
        Arc::new(bots.bot(&saved.id).expect("saved Bot")),
        &store,
        &credentials,
        mobius::backend::session_files::SessionFileStore::new(store.state_dir(), None).into(),
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
    drop(gateway);
    assert!(ready.models.iter().any(|model| *model.route == *route));
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
        crate::bots::BotIdentity {
            name: &saved.name,
            description: &saved.description,
            tint: saved.tint,
            shape: saved.shape,
        },
        Arc::new(crate::wire::VersionedAgentConfig {
            revision: saved.config.revision + 1,
            config: composition,
        }),
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
