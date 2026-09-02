use super::*;

fn test_bot() -> crate::wire::BotRecord {
    crate::wire::BotRecord {
        id: "bot-fixture".into(),
        handle: "fixture".into(),
        name: "Fixture".into(),
        description: "Own fixture work.".into(),
        tint: ProviderTint::default(),
        config: VersionedAgentConfig {
            revision: 1,
            config: AgentComposition::default(),
        },
    }
}

#[test]
fn gateway_config_is_machine_scoped() {
    let config = GatewayConfig::new(DEFAULT_LISTEN, None).expect("gateway config");
    let serialized = serde_json::to_value(config).expect("serialize gateway config");

    assert!(serialized.get("workspace").is_none());
    assert!(serialized["cloudflare"].is_null());
    assert!(serialized["bot_defaults"].is_null());
    assert_eq!(serialized["configured_providers"], serde_json::json!({}));
    assert!(serialized["usage"].get("sessions").is_none());
}

#[test]
fn cloudflare_config_normalizes_a_dns_hostname() {
    let config = CloudflareConfig::named("  mobius.example.com ").expect("Cloudflare config");

    assert_eq!(
        config.endpoint().as_deref(),
        Some("wss://mobius.example.com")
    );
}

#[test]
fn cloudflare_config_rejects_a_url_instead_of_a_hostname() {
    let error =
        CloudflareConfig::named("wss://mobius.example.com/path").expect_err("URL must be rejected");

    assert!(
        error
            .to_string()
            .contains("without a scheme, path, or port")
    );
}

#[test]
fn quick_cloudflare_config_round_trips_without_a_token() {
    let root = tempfile::tempdir().expect("temporary directory");
    let state = root.path().join("state");
    let (store, _) = ConfigStore::initialize_quick_cloudflare(state.clone(), DEFAULT_LISTEN)
        .expect("initialize quick tunnel");

    let contents = fs::read_to_string(state.join(CONFIG_FILE)).expect("gateway config");
    let (_, opened) = ConfigStore::open(state).expect("open quick tunnel config");

    assert!(
        contents.contains("mode = \"quick\"")
            && !store.cloudflare_token_path().exists()
            && opened.cloudflare == Some(CloudflareConfig::Quick)
    );
}

#[test]
fn opening_unmigratable_versions_never_rewrites_config() {
    for (version, invalid) in [
        (19, false),
        (20, false),
        (21, false),
        (22, false),
        (24, false),
        (23, true),
    ] {
        let root = tempfile::tempdir().expect("temporary directory");
        let state = root.path().join("state");
        ConfigStore::initialize(state.clone(), DEFAULT_LISTEN, None).expect("initialize gateway");
        let path = state.join(CONFIG_FILE);
        let mut contents = fs::read_to_string(&path)
            .expect("read gateway config")
            .replacen("version = 23", &format!("version = {version}"), 1);
        if invalid {
            contents = contents.replacen("127.0.0.1:8741", "127.0.0.1:0", 1);
        }
        fs::write(&path, &contents).expect("write incompatible config");

        ConfigStore::open(state).expect_err("config must be rejected");

        assert_eq!(fs::read_to_string(path).expect("read config"), contents);
    }
}

#[test]
fn generated_toml_round_trips_manifest_settings() {
    let root = tempfile::tempdir().expect("temporary directory");
    let state = root.path().join("state");
    let (store, config) =
        ConfigStore::initialize(state.clone(), DEFAULT_LISTEN, None).expect("initialize state");
    let mut config = config
        .registering_provider(
            AgentComposition::default().provider,
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("register provider");
    let usage = TokenUsage {
        input_tokens: 7,
        total_tokens: 7,
        ..TokenUsage::default()
    };
    config
        .usage
        .observe(
            "openai_socket",
            &usage,
            UNIX_EPOCH + std::time::Duration::from_secs(2 * SECONDS_PER_DAY),
        )
        .expect("record usage");
    store.save(&config).expect("save config");

    let contents = fs::read_to_string(state.join(CONFIG_FILE)).expect("read config");
    let (_, restored) = ConfigStore::open(state).expect("open config");

    assert!(contents.starts_with("version = 23"));
    assert!(contents.contains("max_model_steps = 2042"));
    assert!(contents.contains("[bot_defaults.config.middleware.settings.context_offloading]"));
    assert!(contents.contains("[bot_defaults.config.middleware.settings.sessions]"));
    assert!(contents.contains("[bot_defaults.config.middleware.settings.messages]"));
    assert!(contents.contains("delivery = \"steer\""));
    assert_eq!(restored, config);
}

#[test]
fn opening_config_rejects_removed_automatic_approval_settings_without_rewrite() {
    let root = tempfile::tempdir().expect("temporary directory");
    let state = root.path().join("state");
    let (store, config) =
        ConfigStore::initialize(state.clone(), DEFAULT_LISTEN, None).expect("initialize state");
    let mut config = config
        .registering_provider(
            AgentComposition::default().provider,
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("register provider");
    let middleware = &mut config
        .bot_defaults
        .as_mut()
        .expect("Bot defaults")
        .config
        .middleware;
    middleware.set_setting(
        "sandbox",
        "approval_policy",
        Some(mobius::protocol::FrontendSettingValue::String(
            "auto_approve".into(),
        )),
    );
    middleware.set_setting(
        "sandbox",
        "reviewer_model_route",
        Some(mobius::protocol::FrontendSettingValue::String(
            "reviewer".into(),
        )),
    );
    middleware.set_setting(
        "sandbox",
        "reviewer_strictness",
        Some(mobius::protocol::FrontendSettingValue::String(
            "strict".into(),
        )),
    );
    fs::write(
        state.join(CONFIG_FILE),
        toml::to_string_pretty(&config).expect("encode incompatible config"),
    )
    .expect("write incompatible config");
    drop(store);

    let before = fs::read_to_string(state.join(CONFIG_FILE)).expect("config");
    let error = match ConfigStore::open(state.clone()) {
        Ok(_) => panic!("removed settings must be rejected"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("reviewer_model_route"));
    assert_eq!(
        fs::read_to_string(state.join(CONFIG_FILE)).expect("unchanged config"),
        before
    );
}

#[test]
fn extension_selection_is_a_stable_optional_reference() {
    use crate::extensions::{ExtensionSource, InstalledExtension};
    use crate::wire::{ExtensionHookRecord, ExtensionKind};

    let mut config = GatewayConfig::new(DEFAULT_LISTEN, None)
        .expect("gateway config")
        .registering_provider(
            AgentComposition::default().provider,
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("register provider");
    let id = "plugin:ponytail".to_string();
    config
        .bot_defaults
        .as_mut()
        .expect("default")
        .config
        .extensions
        .insert(id.clone());
    config.validate().expect("missing extension is disabled");

    let digest = "a".repeat(64);
    config.installed_extensions.insert(
        id.clone(),
        InstalledExtension {
            kind: ExtensionKind::Plugin,
            name: "ponytail".into(),
            description: "Minimal coding workflows".into(),
            version: Some("4.9.0".into()),
            source: ExtensionSource {
                url: "https://github.com/DietrichGebert/ponytail".into(),
                reference: Some("main".into()),
                subdirectory: None,
            },
            resolved_revision: "b".repeat(40),
            digest: digest.clone(),
            skills: vec!["ponytail:ponytail".into()],
            hooks: vec![ExtensionHookRecord {
                event: "SessionStart".into(),
                matcher: Some("startup".into()),
                command: "node hooks/activate.js".into(),
                timeout_seconds: 5,
            }],
            trusted_hook_digest: None,
        },
    );
    config.validate().expect("untrusted extension is disabled");

    config
        .installed_extensions
        .get_mut(&id)
        .expect("installed extension")
        .trusted_hook_digest = Some(digest);
    config.validate().expect("trusted extension selection");
}

#[test]
fn provider_registration_never_silently_changes_existing_defaults() {
    let config = GatewayConfig::new(DEFAULT_LISTEN, None).expect("gateway config");
    let kimi = ProviderConfig {
        instance: "kimi".into(),
        provider: "kimi".into(),
        model: "kimi-k3".into(),
        base_url: Some("https://api.moonshot.ai/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: Some("max".into()),
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let first = config
        .registering_provider(
            kimi.clone(),
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("register Kimi");
    let openrouter = ProviderConfig {
        instance: "openrouter".into(),
        provider: "openrouter".into(),
        model: "openrouter/pareto-code".into(),
        base_url: Some("https://openrouter.ai/api/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let second = first
        .registering_provider(
            openrouter.clone(),
            "Test".into(),
            Default::default(),
            vec![openrouter.model.clone(), "anthropic/claude-opus-4.1".into()],
            Vec::new(),
        )
        .expect("register OpenRouter");

    assert_eq!(second.configured_providers["kimi"].selection, kimi);
    assert_eq!(
        second.configured_providers["openrouter"].selection,
        openrouter
    );
    assert_eq!(second.configured_providers["openrouter"].model_ids.len(), 2);
    assert_eq!(
        second
            .bot_defaults
            .as_ref()
            .expect("gateway default")
            .config
            .provider
            .provider,
        "kimi"
    );

    let rebound = ProviderConfig {
        instance: "openrouter".into(),
        ..AgentComposition::default().provider
    };
    let error = second
        .registering_provider(
            rebound,
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect_err("an existing instance must keep its provider");
    assert!(
        error
            .to_string()
            .contains("already belongs to `openrouter`")
    );

    let mut updated = kimi.clone();
    updated.model = "kimi-k2.7-code".into();
    updated.reasoning_effort = None;
    let third = second
        .registering_provider(
            updated.clone(),
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("update registered provider");
    assert_eq!(third.configured_providers["kimi"].selection, updated);
    let default = third.bot_defaults.expect("preserved Bot defaults");
    assert_eq!(default.revision, 1);
    assert_eq!(default.config.provider, kimi);
}

#[test]
fn configured_custom_provider_keeps_its_endpoint_and_model() {
    let selection = ProviderConfig {
        instance: "responses".into(),
        provider: "responses".into(),
        model: "vendor/model-opaque".into(),
        base_url: Some("https://example.com/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: Some("provider-defined".into()),
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let config = GatewayConfig::new(DEFAULT_LISTEN, None)
        .expect("gateway config")
        .registering_provider(
            selection.clone(),
            "Test".into(),
            Default::default(),
            vec![selection.model.clone()],
            vec!["provider-defined".into()],
        )
        .expect("register custom provider");

    assert_eq!(
        config.configured_providers["responses"].selection,
        selection
    );
    assert_eq!(
        config.configured_providers["responses"].reasoning_efforts,
        ["provider-defined"]
    );
    assert_eq!(
        config
            .bot_defaults
            .expect("gateway default")
            .config
            .provider,
        selection
    );
}

#[test]
fn openrouter_accepts_a_credentialless_custom_https_endpoint() {
    let selection = ProviderConfig {
        instance: "openrouter".into(),
        provider: "openrouter".into(),
        model: "openai/gpt-5".into(),
        base_url: Some("https://connector.example/v1".into()),
        endpoint_auth: ProviderEndpointAuth::Credentialless,
        reasoning_effort: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };

    GatewayConfig::new(DEFAULT_LISTEN, None)
        .expect("gateway config")
        .registering_provider(
            selection.clone(),
            "Test".into(),
            Default::default(),
            vec![selection.model],
            Vec::new(),
        )
        .expect("credentialless OpenRouter endpoint");
}

#[test]
fn credentialless_endpoint_rejects_openrouter_default_aliases() {
    for base_url in [
        "https://OPENROUTER.AI:443/api/v1/",
        "https://openrouter.ai/alternate-path",
    ] {
        let selection = ProviderConfig {
            instance: "openrouter".into(),
            provider: "openrouter".into(),
            model: "openai/gpt-5".into(),
            base_url: Some(base_url.into()),
            endpoint_auth: ProviderEndpointAuth::Credentialless,
            reasoning_effort: None,
            web_search: mobius::backend::model::provider::HostedWebSearch::Off,
        };

        let error = GatewayConfig::new(DEFAULT_LISTEN, None)
            .expect("gateway config")
            .registering_provider(
                selection.clone(),
                "Test".into(),
                Default::default(),
                vec![selection.model],
                Vec::new(),
            )
            .expect_err("default endpoint origin must require provider authentication");

        assert!(
            error
                .to_string()
                .contains("requires provider authentication")
        );
    }
}

#[test]
fn credentialless_endpoint_requires_https() {
    let selection = ProviderConfig {
        instance: "openrouter".into(),
        provider: "openrouter".into(),
        model: "openai/gpt-5".into(),
        base_url: Some("http://127.0.0.1:8080/v1".into()),
        endpoint_auth: ProviderEndpointAuth::Credentialless,
        reasoning_effort: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };

    let error = GatewayConfig::new(DEFAULT_LISTEN, None)
        .expect("gateway config")
        .registering_provider(
            selection.clone(),
            "Test".into(),
            Default::default(),
            vec![selection.model],
            Vec::new(),
        )
        .expect_err("credentialless endpoint must require HTTPS");

    assert!(error.to_string().contains("must use HTTPS"));
}

#[test]
fn credentialless_endpoint_rejects_secret_bearing_url_components() {
    for base_url in [
        "https://secret@connector.example/v1",
        "https://connector.example/v1?token=secret",
        "https://connector.example/v1#secret",
    ] {
        let selection = ProviderConfig {
            instance: "openrouter".into(),
            provider: "openrouter".into(),
            model: "openai/gpt-5".into(),
            base_url: Some(base_url.into()),
            endpoint_auth: ProviderEndpointAuth::Credentialless,
            reasoning_effort: None,
            web_search: mobius::backend::model::provider::HostedWebSearch::Off,
        };

        GatewayConfig::new(DEFAULT_LISTEN, None)
            .expect("gateway config")
            .registering_provider(
                selection.clone(),
                "Test".into(),
                Default::default(),
                vec![selection.model],
                Vec::new(),
            )
            .expect_err("secret-bearing endpoint must be rejected");
    }
}

#[test]
fn credentialless_endpoint_requires_a_configurable_provider() {
    let selection = ProviderConfig {
        instance: "openai_socket".into(),
        provider: "openai_socket".into(),
        model: "gpt-5.6-luna".into(),
        base_url: Some("https://connector.example/v1".into()),
        endpoint_auth: ProviderEndpointAuth::Credentialless,
        reasoning_effort: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };

    let error = GatewayConfig::new(DEFAULT_LISTEN, None)
        .expect("gateway config")
        .registering_provider(
            selection.clone(),
            "Test".into(),
            Default::default(),
            vec![selection.model],
            Vec::new(),
        )
        .expect_err("a fixed-endpoint provider cannot take a credentialless endpoint");

    assert!(error.to_string().contains("fixed API endpoint"));
}

#[test]
fn custom_provider_registration_validates_its_model_catalog() {
    let selection = ProviderConfig {
        instance: "openrouter".into(),
        provider: "openrouter".into(),
        model: "anthropic/claude-sonnet-4".into(),
        base_url: Some("https://openrouter.ai/api/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let config = GatewayConfig::new(DEFAULT_LISTEN, None).expect("gateway config");

    let missing = config
        .registering_provider(
            selection.clone(),
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect_err("custom catalog must not be empty");
    let duplicate = config
        .registering_provider(
            selection.clone(),
            "Test".into(),
            Default::default(),
            vec![selection.model.clone(), selection.model.clone()],
            Vec::new(),
        )
        .expect_err("custom catalog IDs must be unique");
    let padded = config
        .registering_provider(
            selection.clone(),
            "Test".into(),
            Default::default(),
            vec![" anthropic/claude-sonnet-4".into()],
            Vec::new(),
        )
        .expect_err("custom catalog IDs must be canonical");
    let duplicate_reasoning = config
        .registering_provider(
            selection.clone(),
            "Test".into(),
            Default::default(),
            vec![selection.model.clone()],
            vec!["high".into(), "high".into()],
        )
        .expect_err("custom reasoning efforts must be unique");
    let mut missing_reasoning = selection;
    missing_reasoning.reasoning_effort = Some("high".into());
    let missing_reasoning = config
        .registering_provider(
            missing_reasoning,
            "Test".into(),
            Default::default(),
            vec!["anthropic/claude-sonnet-4".into()],
            vec!["medium".into()],
        )
        .expect_err("selected custom reasoning must be configured");

    assert!(missing.to_string().contains("1–64 entries"));
    assert!(duplicate.to_string().contains("duplicate model ID"));
    assert!(padded.to_string().contains("must be canonical"));
    assert!(
        duplicate_reasoning
            .to_string()
            .contains("duplicate reasoning effort")
    );
    assert!(
        missing_reasoning
            .to_string()
            .contains("configured reasoning catalog")
    );
}

#[test]
fn custom_provider_catalogs_accept_opaque_ids_but_reject_ambiguous_routes() {
    let config = GatewayConfig::new(DEFAULT_LISTEN, None).expect("gateway config");
    config
        .registering_provider(
            ProviderConfig {
                instance: "openrouter".into(),
                provider: "openrouter".into(),
                model: "vendor::model".into(),
                base_url: Some("https://openrouter.ai/api/v1".into()),
                endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: None,
                web_search: mobius::backend::model::provider::HostedWebSearch::Off,
            },
            "Test".into(),
            Default::default(),
            vec!["vendor::model".into()],
            Vec::new(),
        )
        .expect("opaque model ID");
    let collision = config
        .registering_provider(
            ProviderConfig {
                instance: "openrouter".into(),
                provider: "openrouter".into(),
                model: "vendor:".into(),
                base_url: Some("https://openrouter.ai/api/v1".into()),
                endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: Some("high".into()),
                web_search: mobius::backend::model::provider::HostedWebSearch::Off,
            },
            "Test".into(),
            Default::default(),
            vec!["vendor:".into(), "vendor".into()],
            vec!["high".into(), ":high".into()],
        )
        .expect_err("distinct catalog pairs must not share a route");

    assert!(collision.to_string().contains("ambiguous route"));
}

#[test]
fn custom_provider_catalogs_bound_the_total_generated_routes() {
    let models = (0..8)
        .map(|index| format!("vendor/model-{index}"))
        .collect::<Vec<_>>();
    let efforts = (0..8)
        .map(|index| format!("effort-{index}"))
        .collect::<Vec<_>>();
    let config = GatewayConfig::new(DEFAULT_LISTEN, None)
        .expect("gateway config")
        .registering_provider(
            ProviderConfig {
                instance: "openrouter".into(),
                provider: "openrouter".into(),
                model: models[0].clone(),
                base_url: Some("https://openrouter.ai/api/v1".into()),
                endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: Some(efforts[0].clone()),
                web_search: mobius::backend::model::provider::HostedWebSearch::Off,
            },
            "Test".into(),
            Default::default(),
            models,
            efforts,
        )
        .expect("64 custom routes");

    let error = config
        .registering_provider(
            ProviderConfig {
                instance: "responses".into(),
                provider: "responses".into(),
                model: "local-model".into(),
                base_url: Some("http://127.0.0.1:11434/v1".into()),
                endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: None,
                web_search: mobius::backend::model::provider::HostedWebSearch::Off,
            },
            "Test".into(),
            Default::default(),
            vec!["local-model".into()],
            Vec::new(),
        )
        .expect_err("65 total custom routes must fail");

    assert!(error.to_string().contains("at most 64 model routes"));
}

#[test]
fn provider_registration_rejects_a_catalog_that_invalidates_the_current_default() {
    let model = "vendor/model".to_string();
    let config = GatewayConfig::new(DEFAULT_LISTEN, None)
        .expect("gateway config")
        .registering_provider(
            ProviderConfig {
                instance: "openrouter".into(),
                provider: "openrouter".into(),
                model: model.clone(),
                base_url: Some("https://openrouter.ai/api/v1".into()),
                endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: Some("high".into()),
                web_search: mobius::backend::model::provider::HostedWebSearch::Off,
            },
            "Test".into(),
            Default::default(),
            vec![model.clone()],
            vec!["high".into(), "medium".into()],
        )
        .expect("register provider");

    let error = config
        .registering_provider(
            ProviderConfig {
                instance: "openrouter".into(),
                provider: "openrouter".into(),
                model: model.clone(),
                base_url: Some("https://openrouter.ai/api/v1".into()),
                endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: Some("medium".into()),
                web_search: mobius::backend::model::provider::HostedWebSearch::Off,
            },
            "Test".into(),
            Default::default(),
            vec![model],
            vec!["medium".into()],
        )
        .expect_err("updated catalog must preserve current default membership");

    assert!(
        error
            .to_string()
            .contains("selection reasoning effort is not in its configured reasoning catalog")
    );
}

#[test]
fn default_and_persisted_config_validate_custom_reasoning_membership() {
    let model = "vendor/model".to_string();
    let config = GatewayConfig::new(DEFAULT_LISTEN, None)
        .expect("gateway config")
        .registering_provider(
            ProviderConfig {
                instance: "openrouter".into(),
                provider: "openrouter".into(),
                model: model.clone(),
                base_url: Some("https://openrouter.ai/api/v1".into()),
                endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: Some("high".into()),
                web_search: mobius::backend::model::provider::HostedWebSearch::Off,
            },
            "Test".into(),
            Default::default(),
            vec![model],
            vec!["high".into(), "medium".into()],
        )
        .expect("register provider");
    let mut replacement = config
        .bot_defaults
        .as_ref()
        .expect("default")
        .config
        .clone();
    replacement.provider.reasoning_effort = Some("low".into());

    let replace_error = config
        .replacing_bot_defaults(1, replacement)
        .expect_err("default reasoning must be in the catalog");
    let mut persisted = config;
    persisted
        .bot_defaults
        .as_mut()
        .expect("default")
        .config
        .provider
        .reasoning_effort = Some("low".into());
    let persisted_error = persisted
        .validate()
        .expect_err("persisted default reasoning must be in the catalog");

    assert!(replace_error.to_string().contains("reasoning effort"));
    assert!(persisted_error.to_string().contains("reasoning effort"));
}

#[test]
fn provider_catalog_rejects_out_of_catalog_model_and_reasoning() {
    let model = "vendor/model".to_string();
    let gateway = GatewayConfig::new(DEFAULT_LISTEN, None)
        .expect("gateway config")
        .registering_provider(
            ProviderConfig {
                instance: "openrouter".into(),
                provider: "openrouter".into(),
                model: model.clone(),
                base_url: Some("https://openrouter.ai/api/v1".into()),
                endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: Some("high".into()),
                web_search: mobius::backend::model::provider::HostedWebSearch::Off,
            },
            "Test".into(),
            Default::default(),
            vec![model],
            vec!["high".into(), "medium".into()],
        )
        .expect("register provider");
    let mut invalid_model = gateway
        .bot_defaults
        .as_ref()
        .expect("default")
        .config
        .clone();
    invalid_model.provider.model = "vendor/unknown".into();
    let mut invalid_reasoning = gateway
        .bot_defaults
        .as_ref()
        .expect("default")
        .config
        .clone();
    invalid_reasoning.provider.reasoning_effort = Some("low".into());

    let model_error = gateway
        .validate_provider_selection(&invalid_model.provider)
        .expect_err("chat model must be in the catalog");
    let reasoning_error = gateway
        .validate_provider_selection(&invalid_reasoning.provider)
        .expect_err("chat reasoning must be in the catalog");

    assert!(model_error.to_string().contains("selection model"));
    assert!(reasoning_error.to_string().contains("reasoning effort"));
}

#[test]
fn saving_defaults_is_revisioned_and_does_not_change_existing_chat_specs() {
    let registered = GatewayConfig::new(DEFAULT_LISTEN, None)
        .expect("gateway config")
        .registering_provider(
            AgentComposition::default().provider,
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("register provider");
    let workspace = tempfile::tempdir().expect("workspace");
    let state = tempfile::tempdir().expect("state");
    let bot = crate::wire::BotRecord {
        id: "bot-fixture".into(),
        handle: "fixture".into(),
        name: "Fixture".into(),
        description: "Own fixture work.".into(),
        tint: ProviderTint::default(),
        config: registered.bot_defaults.clone().expect("Bot defaults"),
    };
    let chat = ChatSpec::for_bot(workspace.path(), &bot, state.path(), None).expect("chat spec");
    let mut replacement = registered
        .bot_defaults
        .as_ref()
        .expect("default")
        .config
        .clone();
    replacement.middleware.set_enabled("tasks", true);

    let updated = registered
        .replacing_bot_defaults(1, replacement.clone())
        .expect("replace defaults");

    assert_eq!(
        updated
            .bot_defaults
            .as_ref()
            .expect("Bot defaults")
            .revision,
        2
    );
    assert_eq!(
        updated.bot_defaults.as_ref().expect("Bot defaults").config,
        replacement
    );
    assert_eq!(chat.agent.revision, 1);
    assert!(
        registered
            .replacing_bot_defaults(2, AgentComposition::default())
            .expect_err("stale revision")
            .to_string()
            .contains("revision changed")
    );
}

#[test]
fn non_loopback_listener_requires_tls() {
    let listen = "0.0.0.0:8741".parse().expect("listen address");

    let error = GatewayConfig::new(listen, None).expect_err("remote plaintext must fail");

    assert!(error.to_string().contains("require a TLS certificate"));
}

#[test]
fn listener_rejects_port_zero() {
    let listen = "127.0.0.1:0".parse().expect("listen address");

    let error = GatewayConfig::new(listen, None).expect_err("port zero must fail");

    assert!(error.to_string().contains("greater than zero"));
}

#[test]
fn invalid_configuration_does_not_create_gateway_state() {
    let root = tempfile::tempdir().expect("temporary directory");
    let state = root.path().join("state");
    let listen = "127.0.0.1:0".parse().expect("listen address");

    let error =
        ConfigStore::initialize(state.clone(), listen, None).expect_err("invalid config must fail");

    assert!(error.to_string().contains("greater than zero"));
    assert!(!state.exists());
}

#[test]
fn incompatible_state_explains_the_required_reset() {
    let root = tempfile::tempdir().expect("temporary directory");
    let state = root.path().join("state");
    let (_, config) =
        ConfigStore::initialize(state.clone(), DEFAULT_LISTEN, None).expect("initialize state");
    let mut legacy = serde_json::to_value(config).expect("serialize config");
    legacy
        .as_object_mut()
        .expect("config object")
        .insert("workspace".into(), serde_json::json!(root.path()));
    fs::write(
        state.join(CONFIG_FILE),
        serde_json::to_vec(&legacy).expect("encode legacy config"),
    )
    .expect("write legacy config");

    let error = ConfigStore::open(state.clone()).expect_err("legacy state must fail");

    assert!(error.to_string().contains("incompatible with this release"));
    assert!(error.to_string().contains(&state.display().to_string()));
}

#[test]
fn chats_keep_canonical_specs_for_different_worktrees() {
    let root = tempfile::tempdir().expect("root");
    let state = root.path().join("state");
    let worktrees = root.path().join("worktrees");
    let first = worktrees.join("first");
    let second = worktrees.join("second");
    fs::create_dir(&state).expect("state");
    fs::create_dir_all(&first).expect("first worktree");
    fs::create_dir(&second).expect("second worktree");
    let bot = test_bot();

    let first_spec = ChatSpec::for_bot(&first.join("..").join("first"), &bot, &state, None)
        .expect("first chat spec");
    let second_spec = ChatSpec::for_bot(&second, &bot, &state, None).expect("second chat spec");

    assert_eq!(
        first_spec.workspace,
        fs::canonicalize(first).expect("first")
    );
    assert_eq!(
        second_spec.workspace,
        fs::canonicalize(second).expect("second")
    );
    assert_ne!(first_spec.workspace_info(), second_spec.workspace_info());
}

#[test]
fn chat_specs_reject_both_state_overlap_directions() {
    let root = tempfile::tempdir().expect("root");
    let workspace_parent = root.path().join("workspace-parent");
    let state_inside = workspace_parent.join("state");
    let state_parent = root.path().join("state-parent");
    let workspace_inside = state_parent.join("workspace");
    fs::create_dir_all(&state_inside).expect("nested state");
    fs::create_dir_all(&workspace_inside).expect("nested workspace");
    let bot = test_bot();

    let state_inside_error = ChatSpec::for_bot(&workspace_parent, &bot, &state_inside, None)
        .expect_err("state inside workspace must fail");
    let workspace_inside_error = ChatSpec::for_bot(&workspace_inside, &bot, &state_parent, None)
        .expect_err("workspace inside state must fail");

    assert!(state_inside_error.to_string().contains("must not overlap"));
    assert!(
        workspace_inside_error
            .to_string()
            .contains("must not overlap")
    );
}

#[test]
fn workspace_directory_creation_creates_one_canonical_git_workspace() {
    let root = tempfile::tempdir().expect("root");
    let parent = root.path().join("parent");
    let state = root.path().join("state");
    fs::create_dir(&parent).expect("parent");
    fs::create_dir(&state).expect("state");

    let created = create_workspace_directory(&parent, "new workspace", &state, None)
        .expect("create workspace directory");

    assert_eq!(
        created,
        fs::canonicalize(parent.join("new workspace")).expect("created")
    );
    assert!(created.is_dir());
    assert!(created.join(".git").is_dir());
    assert_eq!(
        fs::read_to_string(created.join(".git/HEAD")).expect("Git HEAD"),
        "ref: refs/heads/main\n"
    );
    assert!(!created.join("nested").exists());
}

#[test]
fn background_workspace_is_private_stable_and_outside_gateway_state() {
    let root = tempfile::tempdir().expect("root");
    let state = root.path().join("gateway");
    let (store, _) =
        ConfigStore::initialize(state, DEFAULT_LISTEN, None).expect("initialize gateway");

    let first = prepare_background_workspace(store.state_dir(), None)
        .expect("prepare background workspace");
    let second =
        prepare_background_workspace(store.state_dir(), None).expect("reopen background workspace");

    assert_eq!(first, second);
    assert!(!first.starts_with(store.state_dir()));
    assert!(first.join(".git").is_dir());
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(first)
            .expect("background metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[test]
fn workspace_directory_creation_rejects_invalid_names_and_existing_targets() {
    let root = tempfile::tempdir().expect("root");
    let parent = root.path().join("parent");
    let state = root.path().join("state");
    fs::create_dir(&parent).expect("parent");
    fs::create_dir(&state).expect("state");
    fs::create_dir(parent.join("existing")).expect("existing");

    for name in ["", ".", "..", "../escape", "nested/name", "nested\\name"] {
        assert!(
            create_workspace_directory(&parent, name, &state, None).is_err(),
            "invalid name should be rejected: {name:?}"
        );
    }
    assert!(
        create_workspace_directory(&parent, "existing", &state, None).is_err(),
        "existing target should be rejected"
    );
    assert!(!root.path().join("escape").exists());
}

#[test]
fn workspace_directory_creation_rejects_gateway_state_overlap() {
    let root = tempfile::tempdir().expect("root");
    let parent = root.path().join("parent");
    let state = root.path().join("state");
    fs::create_dir(&parent).expect("parent");
    fs::create_dir(&state).expect("state");

    let error = create_workspace_directory(&state, "workspace", &state, None)
        .expect_err("workspace inside gateway state must fail");

    assert!(error.to_string().contains("must not overlap"));
    assert!(!state.join("workspace").exists());
}

#[test]
fn chat_spec_rejects_a_tls_private_key_inside_its_workspace() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    fs::create_dir(&workspace).expect("workspace");
    fs::create_dir(&state).expect("state");
    let certificate = root.path().join("certificate.pem");
    let private_key = workspace.join("private-key.pem");
    fs::write(&certificate, "certificate").expect("certificate");
    fs::write(&private_key, "private key").expect("private key");
    let tls = TlsConfig {
        certificate,
        private_key,
    };
    let bot = test_bot();

    let error = ChatSpec::for_bot(&workspace, &bot, &state, Some(&tls))
        .expect_err("workspace TLS key must fail");

    assert!(error.to_string().contains("outside every chat workspace"));
}

#[test]
fn chat_spec_metadata_round_trips_and_revalidates_tampering() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    fs::create_dir(&workspace).expect("workspace");
    fs::create_dir(&state).expect("state");
    let bots = crate::bots::BotStore::open(&state).expect("Bots");
    let bot = bots
        .create_bot("Fixture", "Own fixture work.", AgentComposition::default())
        .expect("Bot");
    let spec = ChatSpec::for_bot(&workspace, &bot, &state, None).expect("chat spec");
    let mut metadata = spec.metadata().expect("chat metadata");
    assert_eq!(metadata[CHAT_SPEC_METADATA_KEY]["version"], 14);

    assert_eq!(
        ChatSpec::from_metadata(&metadata, &bots, &state, None).expect("restore chat spec"),
        spec
    );
    for version in [10, 12, 13] {
        let mut previous = metadata.clone();
        previous
            .get_mut(CHAT_SPEC_METADATA_KEY)
            .and_then(Value::as_object_mut)
            .expect("chat metadata object")
            .insert("version".into(), Value::from(version));
        let unchanged = previous.clone();
        assert!(
            ChatSpec::from_metadata(&previous, &bots, &state, None)
                .expect_err("older chat specification must be rejected")
                .to_string()
                .contains(&format!("unsupported chat configuration version {version}"))
        );
        assert_eq!(previous, unchanged);
    }
    metadata
        .get_mut(CHAT_SPEC_METADATA_KEY)
        .and_then(Value::as_object_mut)
        .expect("chat metadata object")
        .insert(
            "workspace".into(),
            serde_json::to_value(fs::canonicalize(&state).expect("canonical state"))
                .expect("state path value"),
        );

    let error = ChatSpec::from_metadata(&metadata, &bots, &state, None)
        .expect_err("tampered workspace must be revalidated");

    assert!(error.to_string().contains("must not overlap"));
}

#[test]
fn usage_history_aggregates_live_increments() {
    let now = UNIX_EPOCH + std::time::Duration::from_secs(2 * SECONDS_PER_DAY);
    let usage = |tokens| TokenUsage {
        input_tokens: tokens,
        total_tokens: tokens,
        ..TokenUsage::default()
    };
    let mut history = UsageHistory::default();

    assert!(
        history
            .observe("openai_socket", &usage(30), now)
            .expect("observe first")
    );
    assert!(
        history
            .observe("openai_socket", &usage(40), now)
            .expect("observe second")
    );
    assert!(
        history
            .observe("kimi", &usage(5), now)
            .expect("observe other provider")
    );

    assert_eq!(
        history.days.get(&2),
        Some(&BTreeMap::from([
            ("kimi".into(), usage(5)),
            ("openai_socket".into(), usage(70)),
        ]))
    );

    let mut config = GatewayConfig::new(DEFAULT_LISTEN, None).expect("gateway config");
    config.usage = history;
    assert_eq!(
        config.profile().daily_usage,
        [
            DailyUsage {
                unix_day: 2,
                provider: "kimi".into(),
                usage: usage(5),
            },
            DailyUsage {
                unix_day: 2,
                provider: "openai_socket".into(),
                usage: usage(70),
            },
        ]
    );
}

#[test]
fn config_rejects_an_empty_system_prompt() {
    let mut config = AgentComposition::default();
    config.system_prompt.clear();

    let error = validate_agent_composition(&config).expect_err("empty prompt must fail");

    assert!(error.to_string().contains("system prompt"));
}

#[test]
fn agent_composition_requires_a_positive_model_step_limit() {
    let config = AgentComposition {
        max_model_steps: 0,
        ..AgentComposition::default()
    };

    let error = validate_agent_composition(&config).expect_err("zero limit must fail");

    assert!(error.to_string().contains("maximum model steps"));
}

#[test]
fn agent_composition_has_no_policy_upper_model_step_limit() {
    let config = AgentComposition {
        max_model_steps: u64::MAX,
        ..AgentComposition::default()
    };

    validate_agent_composition(&config).expect("platform maximum must be accepted");
}

#[cfg(unix)]
#[test]
fn provider_credentials_are_owner_only_and_absent_from_agent_snapshots() {
    let directory = tempfile::tempdir().expect("state directory");
    let path = directory.path().join("credentials.json");
    let credentials = CredentialStore::open(path.clone()).expect("credential store");

    credentials
        .set(
            "openrouter",
            "openrouter",
            "write-only-secret",
            Some("https://openrouter.ai/api/v1"),
        )
        .expect("store credential");
    let error = credentials
        .set(
            "openrouter",
            "responses",
            "replacement-secret",
            Some("https://example.com/v1"),
        )
        .expect_err("an existing instance must keep its provider");

    let mode = fs::metadata(path)
        .expect("credential metadata")
        .permissions()
        .mode()
        & 0o777;
    let snapshot = serde_json::to_string(&AgentComposition::default()).expect("snapshot");
    assert_eq!(mode, 0o600);
    assert!(
        error
            .to_string()
            .contains("already belongs to `openrouter`")
    );
    assert_eq!(
        credentials
            .get(
                "openrouter",
                "openrouter",
                Some("https://openrouter.ai/api/v1")
            )
            .expect("original credential"),
        Some("write-only-secret".into())
    );
    assert!(!snapshot.contains("write-only-secret"));
}

#[test]
fn provider_credentials_normalize_paste_noise_and_reject_non_tokens() {
    let directory = tempfile::tempdir().expect("state directory");
    let credentials =
        CredentialStore::open(directory.path().join("credentials.json")).expect("credential store");
    let base_url = "https://openrouter.ai/api/v1";

    credentials
        .set(
            "openrouter",
            "openrouter",
            " \nvalid-token-1234\t",
            Some(base_url),
        )
        .expect("trim pasted credential");
    let error = credentials
        .set(
            "openrouter-prose",
            "openrouter",
            "not an api key",
            Some(base_url),
        )
        .expect_err("credential prose must fail");

    assert_eq!(
        credentials
            .get("openrouter", "openrouter", Some(base_url))
            .expect("stored credential"),
        Some("valid-token-1234".into())
    );
    assert_eq!(
        credentials
            .hint("openrouter", "openrouter", Some(base_url))
            .expect("credential hint"),
        Some("1234".into())
    );
    assert!(error.to_string().contains("visible ASCII"));
}

#[test]
fn provider_credential_write_limit_is_atomic_and_reopenable() {
    let directory = tempfile::tempdir().expect("state directory");
    let path = directory.path().join("credentials.json");
    let credentials = CredentialStore::open(path.clone()).expect("credential store");
    let api_key = "x".repeat(MAX_API_KEY_BYTES);
    let mut accepted = Vec::new();
    let rejected = (0..64)
        .find_map(|index| {
            let instance = format!("openrouter-{index}");
            match credentials.set(
                &instance,
                "openrouter",
                &api_key,
                Some("https://openrouter.ai/api/v1"),
            ) {
                Ok(()) => {
                    accepted.push(instance);
                    None
                }
                Err(error) => Some((instance, error)),
            }
        })
        .expect("aggregate credential limit");
    let before = fs::read(&path).expect("credential state before rejection");

    let retry = credentials
        .set(
            &rejected.0,
            "openrouter",
            &api_key,
            Some("https://openrouter.ai/api/v1"),
        )
        .expect_err("oversized candidate must remain rejected");
    let reopened = CredentialStore::open(path.clone()).expect("reopen credential store");

    assert!(rejected.1.to_string().contains("state is too large"));
    assert!(retry.to_string().contains("state is too large"));
    assert_eq!(
        fs::read(path).expect("credential state after rejection"),
        before
    );
    assert_eq!(
        reopened
            .get(
                accepted.last().expect("accepted credential"),
                "openrouter",
                Some("https://openrouter.ai/api/v1"),
            )
            .expect("read accepted credential"),
        Some(api_key)
    );
    assert_eq!(
        reopened
            .get(
                &rejected.0,
                "openrouter",
                Some("https://openrouter.ai/api/v1"),
            )
            .expect("read rejected credential"),
        None
    );
}

#[test]
fn provider_credential_open_revalidates_stored_entries() {
    let directory = tempfile::tempdir().expect("state directory");
    let path = directory.path().join("credentials.json");
    fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "not canonical": {
                "provider": "openrouter",
                "api_key": "secret",
                "base_url": "https://openrouter.ai/api/v1"
            }
        }))
        .expect("encode invalid credential state"),
    )
    .expect("write invalid credential state");

    let error = match CredentialStore::open(path) {
        Ok(_) => panic!("invalid stored entry must fail"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("provider instance ID"));
}

#[cfg(unix)]
#[test]
fn initialized_state_and_config_are_owner_only() {
    let state_parent = tempfile::tempdir().expect("state parent");
    let state = state_parent.path().join("gateway");
    let listen = "127.0.0.1:8741".parse().expect("listen address");

    let (store, _) =
        ConfigStore::initialize(state.clone(), listen, None).expect("initialize config");

    let directory_mode = fs::metadata(store.state_dir())
        .expect("state metadata")
        .permissions()
        .mode()
        & 0o777;
    let file_mode = fs::metadata(state.join(CONFIG_FILE))
        .expect("config metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!((directory_mode, file_mode), (0o700, 0o600));
}

#[cfg(unix)]
#[test]
fn cloudflare_token_is_owner_only_and_absent_from_gateway_config() {
    let state_parent = tempfile::tempdir().expect("state parent");
    let state = state_parent.path().join("gateway");
    let (store, _) = ConfigStore::initialize_named_cloudflare(
        state.clone(),
        DEFAULT_LISTEN,
        "mobius.example.com",
        "secret-tunnel-token",
    )
    .expect("initialize Cloudflare config");

    let mode = fs::metadata(store.cloudflare_token_path())
        .expect("token metadata")
        .permissions()
        .mode()
        & 0o777;
    let config = fs::read_to_string(state.join(CONFIG_FILE)).expect("gateway config");

    assert_eq!(mode, 0o600);
    assert!(!config.contains("secret-tunnel-token"));
}

#[cfg(unix)]
#[test]
fn cloudflare_token_loader_rejects_a_symlink() {
    let directory = tempfile::tempdir().expect("token directory");
    let target = directory.path().join("target");
    let link = directory.path().join("token");
    fs::write(&target, "secret-tunnel-token").expect("token");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("token permissions");
    std::os::unix::fs::symlink(target, &link).expect("token symlink");

    let error = load_cloudflare_token(&link).expect_err("symlink must fail");

    assert!(error.to_string().contains("regular file"));
}

#[test]
fn cloudflare_token_loader_rejects_a_nonregular_file() {
    let directory = tempfile::tempdir().expect("token directory");

    let error = load_cloudflare_token(directory.path()).expect_err("directory must fail");

    assert!(error.to_string().contains("regular file"));
}

#[cfg(unix)]
#[test]
fn opening_cloudflare_state_rejects_a_public_token_file() {
    let state_parent = tempfile::tempdir().expect("state parent");
    let state = state_parent.path().join("gateway");
    let (store, _) = ConfigStore::initialize_named_cloudflare(
        state.clone(),
        DEFAULT_LISTEN,
        "mobius.example.com",
        "secret-tunnel-token",
    )
    .expect("initialize Cloudflare config");
    fs::set_permissions(
        store.cloudflare_token_path(),
        fs::Permissions::from_mode(0o644),
    )
    .expect("loosen token permissions");

    let error = ConfigStore::open(state).expect_err("public token file must fail");

    assert!(error.to_string().contains("mode 0600"));
}

#[cfg(unix)]
#[test]
fn opening_state_rejects_a_public_state_directory() {
    let state_parent = tempfile::tempdir().expect("state parent");
    let state = state_parent.path().join("gateway");
    ConfigStore::initialize(state.clone(), DEFAULT_LISTEN, None).expect("initialize state");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755))
        .expect("loosen state permissions");

    let error = ConfigStore::open(state).expect_err("public state directory must fail");

    assert!(error.to_string().contains("mode 0700"));
}

#[cfg(unix)]
#[test]
fn initialization_does_not_repermission_an_existing_directory() {
    let state = tempfile::tempdir().expect("existing state directory");
    fs::set_permissions(state.path(), fs::Permissions::from_mode(0o755))
        .expect("state permissions");
    let listen = "127.0.0.1:8741".parse().expect("listen address");

    let error = ConfigStore::initialize(state.path().to_path_buf(), listen, None)
        .expect_err("existing state directory must fail");
    let mode = fs::metadata(state.path())
        .expect("state metadata")
        .permissions()
        .mode()
        & 0o777;

    assert!(error.to_string().contains("already exists"));
    assert_eq!(mode, 0o755);
}
