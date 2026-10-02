use std::sync::Arc;

use mobius::backend::model::provider::HostedWebSearch;

use crate::bots::BotStore;
use crate::config::{ConfigStore, CredentialStore};
use crate::wire::{ProviderEndpointAuth, ProviderTint};

use super::*;

fn selection(instance: &str, provider: &str, model: &str) -> ProviderConfig {
    ProviderConfig {
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
            vec![primary.model.clone()],
            Vec::new(),
        )
        .expect("primary provider");
    if let Some(secondary) = secondary {
        config = config
            .registering_provider(
                secondary.clone(),
                "Secondary".into(),
                ProviderTint::default(),
                vec![secondary.model.clone()],
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
            replacement.clone(),
            "Secondary".into(),
            ProviderTint::default(),
            vec![replacement.model.clone()],
            Vec::new(),
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
