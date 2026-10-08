use super::*;

fn test_bot() -> crate::wire::BotRecord {
    crate::wire::BotRecord {
        id: "bot-fixture".into(),
        conversation_session_id: "persistent-test".into(),
        handle: "fixture".into(),
        name: "Fixture".into(),
        description: "Own fixture work.".into(),
        tint: ProviderTint::default(),
        shape: crate::wire::BotShape::Circle,
        config: VersionedAgentConfig {
            revision: 1,
            config: AgentComposition::default(),
        },
        accepts_file_attachments: false,
        routine_interaction_policy: RoutineInteractionPolicy::Unattended,
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

#[tokio::test]
async fn retired_hosted_search_does_not_prevent_saved_gateway_startup() {
    use mobius::backend::model::provider::HostedWebSearch;
    use std::sync::Arc;

    let root = tempfile::tempdir().expect("temporary directory");
    let state = root.path().join("state");
    let (store, config) =
        ConfigStore::initialize(state, DEFAULT_LISTEN, None).expect("initialize state");
    let definition = provider("deepseek").expect("DeepSeek");
    let selection = ProviderConfig {
        tool_discovery: None,
        instance: "deepseek".into(),
        provider: "deepseek".into(),
        model: definition.default_model().expect("default model").into(),
        base_url: None,
        endpoint_auth: ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: None,
        service_tier: None,
        web_search: HostedWebSearch::Off,
    };
    let mut config = config
        .registering_provider(
            selection,
            "DeepSeek".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("register supported settings");
    let configured = config.configured_providers.get_mut("deepseek").unwrap();
    configured.selection.web_search = HostedWebSearch::Live;
    config
        .bot_defaults
        .as_mut()
        .unwrap()
        .config
        .provider
        .web_search = HostedWebSearch::Live;
    store.save(&config).expect("preserve saved settings");
    let before = fs::read(store.state_dir().join(CONFIG_FILE)).expect("saved config");
    let (store, restored) =
        ConfigStore::open(store.state_dir().to_path_buf()).expect("open saved Live selection");
    assert_eq!(restored, config);
    assert_eq!(
        fs::read(store.state_dir().join(CONFIG_FILE)).expect("unchanged config"),
        before
    );
    let credentials = Arc::new(CredentialStore::open(store.credentials_path()).unwrap());
    let bots = Arc::new(crate::bots::BotStore::open(store.state_dir()).unwrap());
    let host = crate::host::GatewayHost::start(store, restored, credentials, bots)
        .await
        .expect("retired search selection must not stop the gateway");
    host.shutdown().await;

    let configured = config.configured_providers.remove("deepseek").unwrap();
    let configured = ProviderRegistration {
        models: configured
            .models
            .iter()
            .map(crate::provider_catalog::configured_model_input)
            .collect(),
        selection: configured.selection,
        label: Some(configured.label),
        tint: Some(configured.tint),
        image_model_ids: Some(configured.image_model_ids),
    };
    let error = config
        .registering_configured(configured, false)
        .expect_err("new registrations still validate hosted search support");
    assert!(
        error
            .to_string()
            .contains("does not support web search mode `live`")
    );
}

#[test]
fn computer_control_requires_full_access_or_no_network_only_when_enabled() {
    let mut composition = AgentComposition::default();
    for (policy, admitted) in [
        ("ask", false),
        ("allow_network", false),
        ("full_access", true),
        ("allow", true),
    ] {
        composition.middleware.set_setting(
            "sandbox",
            "approval_policy",
            Some(mobius::protocol::FrontendSettingValue::String(
                policy.into(),
            )),
        );
        composition
            .middleware
            .set_enabled("computer_control", false);
        assert!(
            validate_agent_composition(&composition).is_ok(),
            "{policy} without computer control"
        );
        composition.middleware.set_enabled("computer_control", true);
        assert_eq!(
            validate_agent_composition(&composition).is_ok(),
            admitted,
            "{policy}"
        );
    }
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
        (0, false),
        (CONFIG_VERSION - 1, false),
        (CONFIG_VERSION + 1, false),
        (CONFIG_VERSION, true),
    ] {
        let root = tempfile::tempdir().expect("temporary directory");
        let state = root.path().join("state");
        ConfigStore::initialize(state.clone(), DEFAULT_LISTEN, None).expect("initialize gateway");
        let path = state.join(CONFIG_FILE);
        let mut contents = fs::read_to_string(&path)
            .expect("read gateway config")
            .replacen(
                &format!("version = {CONFIG_VERSION}"),
                &format!("version = {version}"),
                1,
            );
        if invalid {
            contents = contents.replacen("127.0.0.1:8741", "127.0.0.1:0", 1);
        }
        fs::write(&path, &contents).expect("write incompatible config");

        ConfigStore::open(state).expect_err("config must be rejected");

        assert_eq!(fs::read_to_string(path).expect("read config"), contents);
    }
}

#[tokio::test]
async fn previous_storage_generation_is_rejected_without_initializing_a_new_catalog() {
    let root = tempfile::tempdir().expect("temporary directory");
    let state = root.path().join("state");
    let (_, mut config) =
        ConfigStore::initialize(state.clone(), DEFAULT_LISTEN, None).expect("initialize gateway");
    config.version = CONFIG_VERSION - 1;
    let contents = toml::to_string_pretty(&config).expect("previous configuration");
    fs::write(state.join(CONFIG_FILE), &contents).expect("save previous configuration");
    let previous_catalog = state.join("bots.json");
    fs::write(&previous_catalog, b"previous Bot state").expect("previous catalog");

    let error = match crate::server::GatewayServer::open(state.clone()).await {
        Ok(_) => panic!("previous storage generation must be rejected"),
        Err(error) => error,
    };

    assert!(
        error
            .to_string()
            .contains("unsupported gateway config version")
    );
    assert!(!state.join("bots.sqlite3").exists());
    assert_eq!(
        fs::read(previous_catalog).expect("unchanged catalog"),
        b"previous Bot state"
    );
    assert_eq!(
        fs::read_to_string(state.join(CONFIG_FILE)).expect("unchanged config"),
        contents
    );
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

    assert!(contents.starts_with(&format!("version = {CONFIG_VERSION}\n")));
    assert!(contents.contains("max_model_steps = 2042"));
    assert!(contents.contains("[bot_defaults.config.middleware.settings.compaction]"));
    assert!(contents.contains("allow_model_compaction = \"off\""));
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
fn provider_registration_defaults_metadata_and_allows_explicit_image_clear() {
    let mut selection = AgentComposition::default().provider;
    selection.instance = "openrouter".into();
    selection.provider = "openrouter".into();
    selection.model = "example/chat".into();
    selection.reasoning_effort = None;
    selection.base_url = Some("https://openrouter.ai/api/v1".into());
    selection.web_search = mobius::backend::model::provider::HostedWebSearch::Off;
    let registration = || ProviderRegistration {
        selection: selection.clone(),
        label: None,
        tint: None,
        models: Vec::new(),
        image_model_ids: None,
    };
    let config = GatewayConfig::new(DEFAULT_LISTEN, None)
        .unwrap()
        .registering_configured(registration(), false)
        .unwrap();
    let setup = &config.configured_providers["openrouter"];
    assert_eq!(setup.label, provider("openrouter").unwrap().label());
    assert_eq!(setup.tint, ProviderTint::default());
    assert!(setup.image_model_ids.is_empty());
    let config = config
        .registering_configured(
            ProviderRegistration {
                label: Some("Work".into()),
                tint: Some(ProviderTint::Purple),
                image_model_ids: Some(vec!["example/image".into()]),
                ..registration()
            },
            false,
        )
        .unwrap();
    let retained = config
        .registering_configured(registration(), false)
        .unwrap();
    assert_eq!(retained.configured_providers, config.configured_providers);
    let cleared = retained
        .registering_configured(
            ProviderRegistration {
                image_model_ids: Some(Vec::new()),
                ..registration()
            },
            false,
        )
        .unwrap();
    let setup = &cleared.configured_providers["openrouter"];
    assert_eq!(setup.label, "Work");
    assert_eq!(setup.tint, ProviderTint::Purple);
    assert!(setup.image_model_ids.is_empty());
    assert!(
        cleared
            .registering_configured(
                ProviderRegistration {
                    label: Some(String::new()),
                    ..registration()
                },
                false
            )
            .is_err()
    );
}

#[test]
fn provider_registration_never_silently_changes_existing_defaults() {
    let config = GatewayConfig::new(DEFAULT_LISTEN, None).expect("gateway config");
    let kimi = ProviderConfig {
        tool_discovery: None,
        instance: "kimi".into(),
        provider: "kimi".into(),
        model: "kimi-k3".into(),
        base_url: Some("https://api.moonshot.ai/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: Some("max".into()),
        service_tier: None,
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
        tool_discovery: None,
        instance: "openrouter".into(),
        provider: "openrouter".into(),
        model: "openrouter/pareto-code".into(),
        base_url: Some("https://openrouter.ai/api/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: None,
        service_tier: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let second = first
        .registering_provider(
            openrouter.clone(),
            "Test".into(),
            Default::default(),
            vec![
                crate::wire::ConfiguredModel {
                    id: openrouter.model.clone(),
                    reasoning_efforts: Some(Vec::new()),
                    default_reasoning: None,
                    ..Default::default()
                },
                crate::wire::ConfiguredModel {
                    id: "anthropic/claude-opus-4.1".into(),
                    reasoning_efforts: Some(Vec::new()),
                    default_reasoning: None,
                    ..Default::default()
                },
            ],
            Vec::new(),
        )
        .expect("register OpenRouter");

    assert_eq!(second.configured_providers["kimi"].selection, kimi);
    assert_eq!(
        second.configured_providers["openrouter"].selection,
        openrouter
    );
    assert_eq!(second.configured_providers["openrouter"].models.len(), 2);
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
            second.configured_providers["kimi"]
                .models
                .iter()
                .map(crate::provider_catalog::configured_model_input)
                .collect(),
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
        tool_discovery: None,
        instance: "responses".into(),
        provider: "responses".into(),
        model: "vendor/model-opaque".into(),
        base_url: Some("https://example.com/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: Some("provider-defined".into()),
        service_tier: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let config = GatewayConfig::new(DEFAULT_LISTEN, None)
        .expect("gateway config")
        .registering_provider(
            selection.clone(),
            "Test".into(),
            Default::default(),
            vec![crate::wire::ConfiguredModel {
                id: selection.model.clone(),
                reasoning_efforts: Some(vec!["provider-defined".into()]),
                default_reasoning: Some("provider-defined".into()),
                ..Default::default()
            }],
            Vec::new(),
        )
        .expect("register custom provider");

    assert_eq!(
        config.configured_providers["responses"].selection,
        selection
    );
    assert_eq!(
        config.configured_providers["responses"]
            .models
            .iter()
            .find(|model| model.id == selection.model)
            .unwrap()
            .reasoning
            .iter()
            .map(|effort| effort.id.as_str())
            .collect::<Vec<_>>(),
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
fn browser_authenticated_bot_selections_are_bound_to_the_registered_endpoint() {
    let definition = provider("openai_codex").expect("browser-auth provider");
    for endpoint in [None, Some("https://operator.example/codex")] {
        let selection = ProviderConfig {
            tool_discovery: None,
            instance: "codex".into(),
            provider: definition.id().into(),
            model: definition
                .models()
                .first()
                .expect("preset model")
                .id
                .clone(),
            base_url: endpoint.map(str::to_owned),
            endpoint_auth: ProviderEndpointAuth::ProviderDefault,
            reasoning_effort: None,
            service_tier: None,
            web_search: mobius::backend::model::provider::HostedWebSearch::Off,
        };
        let gateway = GatewayConfig::new(DEFAULT_LISTEN, None)
            .expect("gateway")
            .registering_provider(
                selection.clone(),
                "Codex".into(),
                Default::default(),
                Vec::new(),
                Vec::new(),
            )
            .expect("trusted operator registration");
        gateway
            .validate_provider_selection(&selection)
            .expect("registered endpoint is allowed");
        let mut bot = gateway
            .bot_defaults
            .as_ref()
            .expect("defaults")
            .config
            .clone();
        bot.provider.base_url = Some("https://attacker.example/codex".into());
        assert!(
            validate_agent_composition(&bot).is_ok(),
            "structural URL validation cannot authorize credential destinations"
        );
        assert!(
            validate_bot_compatibility(&gateway, &bot, Default::default())
                .expect_err("Bot cannot redirect browser credentials")
                .to_string()
                .contains("operator-registered")
        );
        bot.provider.base_url = Some(format!(
            "{}/",
            crate::provider_catalog::selected_base_url(definition, &selection)
                .expect("effective endpoint")
        ));
        validate_bot_compatibility(&gateway, &bot, Default::default())
            .expect("normalized registered endpoint");
        if endpoint.is_some() {
            bot.provider.base_url = None;
            assert!(
                validate_bot_compatibility(&gateway, &bot, Default::default()).is_err(),
                "implicit native endpoint cannot replace the registered proxy"
            );
        }
    }
}

#[test]
fn openrouter_accepts_a_credentialless_custom_https_endpoint() {
    let selection = ProviderConfig {
        tool_discovery: None,
        instance: "openrouter".into(),
        provider: "openrouter".into(),
        model: "openai/gpt-5".into(),
        base_url: Some("https://connector.example/v1".into()),
        endpoint_auth: ProviderEndpointAuth::Credentialless,
        reasoning_effort: None,
        service_tier: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };

    GatewayConfig::new(DEFAULT_LISTEN, None)
        .expect("gateway config")
        .registering_provider(
            selection.clone(),
            "Test".into(),
            Default::default(),
            vec![crate::wire::ConfiguredModel {
                id: selection.model,
                reasoning_efforts: Some(Vec::new()),
                default_reasoning: None,
                ..Default::default()
            }],
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
            tool_discovery: None,
            instance: "openrouter".into(),
            provider: "openrouter".into(),
            model: "openai/gpt-5".into(),
            base_url: Some(base_url.into()),
            endpoint_auth: ProviderEndpointAuth::Credentialless,
            reasoning_effort: None,
            service_tier: None,
            web_search: mobius::backend::model::provider::HostedWebSearch::Off,
        };

        let error = GatewayConfig::new(DEFAULT_LISTEN, None)
            .expect("gateway config")
            .registering_provider(
                selection.clone(),
                "Test".into(),
                Default::default(),
                vec![crate::wire::ConfiguredModel {
                    id: selection.model,
                    reasoning_efforts: Some(Vec::new()),
                    default_reasoning: None,
                    ..Default::default()
                }],
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
        tool_discovery: None,
        instance: "openrouter".into(),
        provider: "openrouter".into(),
        model: "openai/gpt-5".into(),
        base_url: Some("http://127.0.0.1:8080/v1".into()),
        endpoint_auth: ProviderEndpointAuth::Credentialless,
        reasoning_effort: None,
        service_tier: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };

    let error = GatewayConfig::new(DEFAULT_LISTEN, None)
        .expect("gateway config")
        .registering_provider(
            selection.clone(),
            "Test".into(),
            Default::default(),
            vec![crate::wire::ConfiguredModel {
                id: selection.model,
                reasoning_efforts: Some(Vec::new()),
                default_reasoning: None,
                ..Default::default()
            }],
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
            tool_discovery: None,
            instance: "openrouter".into(),
            provider: "openrouter".into(),
            model: "openai/gpt-5".into(),
            base_url: Some(base_url.into()),
            endpoint_auth: ProviderEndpointAuth::Credentialless,
            reasoning_effort: None,
            service_tier: None,
            web_search: mobius::backend::model::provider::HostedWebSearch::Off,
        };

        GatewayConfig::new(DEFAULT_LISTEN, None)
            .expect("gateway config")
            .registering_provider(
                selection.clone(),
                "Test".into(),
                Default::default(),
                vec![crate::wire::ConfiguredModel {
                    id: selection.model,
                    reasoning_efforts: Some(Vec::new()),
                    default_reasoning: None,
                    ..Default::default()
                }],
                Vec::new(),
            )
            .expect_err("secret-bearing endpoint must be rejected");
    }
}

#[test]
fn credentialless_endpoint_requires_provider_opt_in() {
    let selection = ProviderConfig {
        tool_discovery: None,
        instance: "openai_socket".into(),
        provider: "openai_socket".into(),
        model: "gpt-6-luna".into(),
        base_url: Some("https://connector.example/v1".into()),
        endpoint_auth: ProviderEndpointAuth::Credentialless,
        reasoning_effort: None,
        service_tier: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };

    let error = GatewayConfig::new(DEFAULT_LISTEN, None)
        .expect("gateway config")
        .registering_provider(
            selection.clone(),
            "Test".into(),
            Default::default(),
            vec![crate::wire::ConfiguredModel {
                id: selection.model,
                reasoning_efforts: Some(Vec::new()),
                default_reasoning: None,
                ..Default::default()
            }],
            Vec::new(),
        )
        .expect_err("native OpenAI endpoints require credentials");

    assert!(
        error
            .to_string()
            .contains("does not support credentialless")
    );
}

#[test]
fn custom_provider_registration_validates_its_model_catalog() {
    let selection = ProviderConfig {
        tool_discovery: None,
        instance: "openrouter".into(),
        provider: "openrouter".into(),
        model: "anthropic/claude-sonnet-4".into(),
        base_url: Some("https://openrouter.ai/api/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: None,
        service_tier: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let config = GatewayConfig::new(DEFAULT_LISTEN, None).expect("gateway config");

    let initialized = config
        .registering_provider(
            selection.clone(),
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("new catalog starts with the selected model");
    let duplicate = config
        .registering_provider(
            selection.clone(),
            "Test".into(),
            Default::default(),
            vec![
                crate::wire::ConfiguredModel {
                    id: selection.model.clone(),
                    reasoning_efforts: Some(Vec::new()),
                    default_reasoning: None,
                    ..Default::default()
                },
                crate::wire::ConfiguredModel {
                    id: selection.model.clone(),
                    reasoning_efforts: Some(Vec::new()),
                    default_reasoning: None,
                    ..Default::default()
                },
            ],
            Vec::new(),
        )
        .expect_err("custom catalog IDs must be unique");
    let padded = config
        .registering_provider(
            selection.clone(),
            "Test".into(),
            Default::default(),
            vec![crate::wire::ConfiguredModel {
                id: " anthropic/claude-sonnet-4".into(),
                reasoning_efforts: Some(Vec::new()),
                default_reasoning: None,
                ..Default::default()
            }],
            Vec::new(),
        )
        .expect_err("custom catalog IDs must be canonical");
    let duplicate_reasoning = config
        .registering_provider(
            selection.clone(),
            "Test".into(),
            Default::default(),
            vec![crate::wire::ConfiguredModel {
                id: selection.model.clone(),
                reasoning_efforts: Some(vec!["high".into(), "high".into()]),
                default_reasoning: Some("high".into()),
                ..Default::default()
            }],
            Vec::new(),
        )
        .expect_err("custom reasoning efforts must be unique");
    let mut missing_reasoning = selection;
    missing_reasoning.reasoning_effort = Some("high".into());
    let missing_reasoning = config
        .registering_provider(
            missing_reasoning,
            "Test".into(),
            Default::default(),
            vec![crate::wire::ConfiguredModel {
                id: "anthropic/claude-sonnet-4".into(),
                reasoning_efforts: Some(vec!["medium".into()]),
                default_reasoning: Some("medium".into()),
                ..Default::default()
            }],
            Vec::new(),
        )
        .expect_err("selected custom reasoning must be configured");

    assert_eq!(
        initialized.configured_providers["openrouter"].models.len(),
        1
    );
    assert_eq!(
        initialized.configured_providers["openrouter"].models[0].id,
        initialized.configured_providers["openrouter"]
            .selection
            .model
    );
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
                tool_discovery: None,
                instance: "openrouter".into(),
                provider: "openrouter".into(),
                model: "vendor::model".into(),
                base_url: Some("https://openrouter.ai/api/v1".into()),
                endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: None,
                service_tier: None,
                web_search: mobius::backend::model::provider::HostedWebSearch::Off,
            },
            "Test".into(),
            Default::default(),
            vec![crate::wire::ConfiguredModel {
                id: "vendor::model".into(),
                reasoning_efforts: Some(Vec::new()),
                default_reasoning: None,
                ..Default::default()
            }],
            Vec::new(),
        )
        .expect("opaque model ID");
    let collision = config
        .registering_provider(
            ProviderConfig {
                tool_discovery: None,
                instance: "openrouter".into(),
                provider: "openrouter".into(),
                model: "vendor:".into(),
                base_url: Some("https://openrouter.ai/api/v1".into()),
                endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: Some("high".into()),
                service_tier: None,
                web_search: mobius::backend::model::provider::HostedWebSearch::Off,
            },
            "Test".into(),
            Default::default(),
            vec![
                crate::wire::ConfiguredModel {
                    id: "vendor:".into(),
                    reasoning_efforts: Some(vec!["high".into(), ":high".into()]),
                    default_reasoning: Some("high".into()),
                    ..Default::default()
                },
                crate::wire::ConfiguredModel {
                    id: "vendor".into(),
                    reasoning_efforts: Some(vec!["high".into(), ":high".into()]),
                    default_reasoning: Some("high".into()),
                    ..Default::default()
                },
            ],
            Vec::new(),
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
                tool_discovery: None,
                instance: "openrouter".into(),
                provider: "openrouter".into(),
                model: models[0].clone(),
                base_url: Some("https://openrouter.ai/api/v1".into()),
                endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: Some(efforts[0].clone()),
                service_tier: None,
                web_search: mobius::backend::model::provider::HostedWebSearch::Off,
            },
            "Test".into(),
            Default::default(),
            {
                let reasoning_efforts: Vec<String> = efforts;
                (models)
                    .into_iter()
                    .map(|id| crate::wire::ConfiguredModel {
                        id,
                        default_reasoning: reasoning_efforts.first().cloned(),
                        reasoning_efforts: Some(reasoning_efforts.clone()),
                        ..Default::default()
                    })
                    .collect()
            },
            Vec::new(),
        )
        .expect("64 custom routes");

    let error = config
        .registering_provider(
            ProviderConfig {
                tool_discovery: None,
                instance: "responses".into(),
                provider: "responses".into(),
                model: "local-model".into(),
                base_url: Some("http://127.0.0.1:11434/v1".into()),
                endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: None,
                service_tier: None,
                web_search: mobius::backend::model::provider::HostedWebSearch::Off,
            },
            "Test".into(),
            Default::default(),
            vec![crate::wire::ConfiguredModel {
                id: "local-model".into(),
                reasoning_efforts: Some(Vec::new()),
                default_reasoning: None,
                ..Default::default()
            }],
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
                tool_discovery: None,
                instance: "openrouter".into(),
                provider: "openrouter".into(),
                model: model.clone(),
                base_url: Some("https://openrouter.ai/api/v1".into()),
                endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: Some("high".into()),
                service_tier: None,
                web_search: mobius::backend::model::provider::HostedWebSearch::Off,
            },
            "Test".into(),
            Default::default(),
            vec![crate::wire::ConfiguredModel {
                id: model.clone(),
                reasoning_efforts: Some(vec!["high".into(), "medium".into()]),
                default_reasoning: Some("high".into()),
                ..Default::default()
            }],
            Vec::new(),
        )
        .expect("register provider");

    let error = config
        .registering_provider(
            ProviderConfig {
                tool_discovery: None,
                instance: "openrouter".into(),
                provider: "openrouter".into(),
                model: model.clone(),
                base_url: Some("https://openrouter.ai/api/v1".into()),
                endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: Some("medium".into()),
                service_tier: None,
                web_search: mobius::backend::model::provider::HostedWebSearch::Off,
            },
            "Test".into(),
            Default::default(),
            vec![crate::wire::ConfiguredModel {
                id: model,
                reasoning_efforts: Some(vec!["medium".into()]),
                default_reasoning: Some("medium".into()),
                ..Default::default()
            }],
            Vec::new(),
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
                tool_discovery: None,
                instance: "openrouter".into(),
                provider: "openrouter".into(),
                model: model.clone(),
                base_url: Some("https://openrouter.ai/api/v1".into()),
                endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: Some("high".into()),
                service_tier: None,
                web_search: mobius::backend::model::provider::HostedWebSearch::Off,
            },
            "Test".into(),
            Default::default(),
            vec![crate::wire::ConfiguredModel {
                id: model,
                reasoning_efforts: Some(vec!["high".into(), "medium".into()]),
                default_reasoning: Some("high".into()),
                ..Default::default()
            }],
            Vec::new(),
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
                tool_discovery: None,
                instance: "openrouter".into(),
                provider: "openrouter".into(),
                model: model.clone(),
                base_url: Some("https://openrouter.ai/api/v1".into()),
                endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: Some("high".into()),
                service_tier: None,
                web_search: mobius::backend::model::provider::HostedWebSearch::Off,
            },
            "Test".into(),
            Default::default(),
            vec![crate::wire::ConfiguredModel {
                id: model,
                reasoning_efforts: Some(vec!["high".into(), "medium".into()]),
                default_reasoning: Some("high".into()),
                ..Default::default()
            }],
            Vec::new(),
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
        conversation_session_id: "persistent-test".into(),
        handle: "fixture".into(),
        name: "Fixture".into(),
        description: "Own fixture work.".into(),
        tint: ProviderTint::default(),
        shape: crate::wire::BotShape::Circle,
        config: registered.bot_defaults.clone().expect("Bot defaults"),
        accepts_file_attachments: false,
        routine_interaction_policy: RoutineInteractionPolicy::Unattended,
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
    assert_eq!(
        chat,
        ChatSpec::for_bot(workspace.path(), &bot, state.path(), None).expect("unchanged chat spec")
    );
    assert_eq!(bot.config.revision, 1);
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
fn invalid_configuration_explains_recovery_without_deleting_state() {
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

    assert!(error.to_string().contains("preserve the state directory"));
    assert!(
        error
            .to_string()
            .contains(&state.join(CONFIG_FILE).display().to_string())
    );
    assert_eq!(
        fs::read(state.join(CONFIG_FILE)).expect("saved config remains available"),
        serde_json::to_vec(&legacy).expect("original saved config"),
    );
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
        Some(fs::canonicalize(first).expect("first"))
    );
    assert_eq!(
        second_spec.workspace,
        Some(fs::canonicalize(second).expect("second"))
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
    let attached = root.path().join("attached");
    let state = root.path().join("state");
    fs::create_dir(&workspace).expect("workspace");
    fs::create_dir(&attached).expect("attached folder");
    fs::create_dir(&state).expect("state");
    let bots = crate::bots::BotStore::open(&state).expect("Bots");
    let bot = bots
        .create_bot("Fixture", "Own fixture work.", AgentComposition::default())
        .expect("Bot");
    let spec = ChatSpec::for_bot(&workspace, &bot, &state, None)
        .expect("chat spec")
        .with_attached_folder(&attached.join("..").join("attached"), &state, None)
        .expect("attach folder")
        .expect("changed chat spec");
    assert_eq!(
        spec.attached_folders,
        [fs::canonicalize(&attached).expect("canonical attached")]
    );
    assert!(
        spec.with_attached_folder(&attached, &state, None)
            .expect("duplicate attachment")
            .is_none()
    );
    let mut metadata = spec.metadata().expect("chat metadata");
    assert_eq!(metadata[CHAT_SPEC_METADATA_KEY]["version"], 15);

    assert_eq!(
        ChatSpec::from_metadata(&metadata, &bots, &state, None).expect("restore chat spec"),
        spec
    );
    fs::remove_dir(&attached).expect("remove attached folder");
    assert!(
        ChatSpec::from_metadata(&metadata, &bots, &state, None)
            .expect("restore without optional folder")
            .attached_folders
            .is_empty()
    );
    fs::create_dir(&attached).expect("restore attached folder");
    for version in [10, 12, 13, 14] {
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
    let mut attached_tampering = metadata.clone();
    attached_tampering
        .get_mut(CHAT_SPEC_METADATA_KEY)
        .and_then(Value::as_object_mut)
        .expect("chat metadata object")
        .insert(
            "attached_folders".into(),
            serde_json::json!([fs::canonicalize(&state).expect("canonical state")]),
        );
    assert!(
        ChatSpec::from_metadata(&attached_tampering, &bots, &state, None)
            .expect_err("tampered attached folder must be revalidated")
            .to_string()
            .contains("must not overlap")
    );
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
fn realtime_voice_route_persists_and_is_optional() {
    let mut config = AgentComposition {
        realtime_voice: Some("openai_codex::gpt-live-1-codex::cove".into()),
        ..AgentComposition::default()
    };
    validate_agent_composition(&config).expect("voice route");
    let stored = toml::to_string(&config).expect("encode Bot configuration");
    assert_eq!(
        toml::from_str::<AgentComposition>(&stored).expect("decode"),
        config
    );
    config.realtime_voice = None;
    validate_agent_composition(&config).expect("voice selection is optional");
}

#[test]
fn bot_compatibility_checks_policies_route_and_voice_without_credentials() {
    let selection = ProviderConfig {
        tool_discovery: None,
        instance: "responses".into(),
        provider: "responses".into(),
        model: "custom-model".into(),
        base_url: Some("https://api.openai.com/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: None,
        service_tier: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let gateway = GatewayConfig::new(DEFAULT_LISTEN, None)
        .expect("gateway config")
        .registering_provider(
            selection,
            "Test".into(),
            Default::default(),
            vec![crate::wire::ConfiguredModel {
                id: "custom-model".into(),
                reasoning_efforts: Some(vec!["high".into(), "medium".into()]),
                default_reasoning: Some("high".into()),
                ..Default::default()
            }],
            Vec::new(),
        )
        .expect("register provider");
    let mut bot = gateway
        .bot_defaults
        .as_ref()
        .expect("defaults")
        .config
        .clone();
    bot.middleware.set_enabled("image_generation", true);
    bot.realtime_voice = Some("openai_socket::gpt-live-1::cedar".into());
    validate_bot_compatibility(&gateway, &bot, Default::default())
        .expect("image generation and voice routes validate without credentials");

    bot.middleware.set_setting(
        "compaction",
        "allow_model_compaction",
        Some(mobius::protocol::FrontendSettingValue::String("on".into())),
    );
    bot.middleware.set_enabled("tasks", true);
    validate_bot_compatibility(&gateway, &bot, Default::default())
        .expect("model-requested compaction and tasks are independent");

    bot.realtime_voice = Some(String::new());
    assert!(
        validate_bot_compatibility(&gateway, &bot, Default::default())
            .expect_err("empty voice route")
            .to_string()
            .contains("voice")
    );
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
            Some(2_000_000_000),
        )
        .expect("store credential");
    let error = credentials
        .set(
            "openrouter",
            "responses",
            "replacement-secret",
            Some("https://example.com/v1"),
            None,
        )
        .expect_err("an existing instance must keep its provider");

    let reopened = CredentialStore::open(path.clone()).expect("reopen expiring credential");
    assert_eq!(
        reopened
            .get(
                "openrouter",
                "openrouter",
                Some("https://openrouter.ai/api/v1")
            )
            .unwrap()
            .and_then(|credential| credential.lifetime.expires_at),
        Some(UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000)),
    );
    assert_eq!(
        reopened
            .get("openrouter", "openrouter", Some("https://other.example/v1"))
            .unwrap()
            .map(|credential| credential.api_key),
        None
    );

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
            .expect("original credential")
            .map(|credential| credential.api_key),
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
            None,
        )
        .expect("trim pasted credential");
    let error = credentials
        .set(
            "openrouter-prose",
            "openrouter",
            "not an api key",
            Some(base_url),
            None,
        )
        .expect_err("credential prose must fail");

    assert_eq!(
        credentials
            .get("openrouter", "openrouter", Some(base_url))
            .expect("stored credential")
            .map(|credential| credential.api_key),
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
    let api_key = "x".repeat(MAX_PROVIDER_API_KEY_BYTES);
    let mut accepted = Vec::new();
    let rejected = (0..64)
        .find_map(|index| {
            let instance = format!("openrouter-{index}");
            match credentials.set(
                &instance,
                "openrouter",
                &api_key,
                Some("https://openrouter.ai/api/v1"),
                None,
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
            None,
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
            .expect("read accepted credential")
            .map(|credential| credential.api_key),
        Some(api_key)
    );
    assert_eq!(
        reopened
            .get(
                &rejected.0,
                "openrouter",
                Some("https://openrouter.ai/api/v1"),
            )
            .expect("read rejected credential")
            .map(|credential| credential.api_key),
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
    fs::set_permissions(&target, mobius::owner_only::file()).expect("token permissions");
    std::os::unix::fs::symlink(target, &link).expect("token symlink");

    let error = load_secret_file(&link).expect_err("symlink must fail");

    assert!(error.to_string().contains("regular file"));
}

#[test]
fn cloudflare_token_loader_rejects_a_nonregular_file() {
    let directory = tempfile::tempdir().expect("token directory");

    let error = load_secret_file(directory.path()).expect_err("directory must fail");

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

#[tokio::test]
async fn replacing_and_removing_credentials_revokes_resolved_routes() {
    let directory = tempfile::tempdir().unwrap();
    let store = CredentialStore::open(directory.path().join("credentials.json")).unwrap();
    let base_url = Some("https://api.openai.com/v1");
    store
        .set(
            "openai_socket",
            "openai_socket",
            "secret-one",
            base_url,
            None,
        )
        .unwrap();
    let mut first = store
        .get("openai_socket", "openai_socket", base_url)
        .unwrap()
        .unwrap()
        .lifetime
        .revoked
        .unwrap();
    store
        .set(
            "openai_socket",
            "openai_socket",
            "secret-one",
            base_url,
            None,
        )
        .unwrap();
    assert!(!first.has_changed().unwrap());
    store
        .set(
            "openai_socket",
            "openai_socket",
            "secret-two",
            base_url,
            Some(2_000_000_000),
        )
        .unwrap();
    assert!(first.changed().await.is_err());
    let second = store
        .get("openai_socket", "openai_socket", base_url)
        .unwrap()
        .unwrap();
    assert_eq!(second.api_key, "secret-two");
    assert_eq!(
        second.lifetime.expires_at,
        Some(UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000))
    );
    let mut revoked = second.lifetime.revoked.unwrap();
    store.remove("openai_socket").unwrap();
    assert!(revoked.changed().await.is_err());
}

#[test]
fn new_gateway_bot_defaults_use_full_access() {
    assert_eq!(
        AgentComposition::default()
            .middleware
            .setting("sandbox", "approval_policy"),
        Some(&mobius::protocol::FrontendSettingValue::String(
            "full_access".into()
        ))
    );
}

#[test]
fn runtime_storage_allowance_preserves_the_64_mib_floor() {
    let mut config = GatewayConfig::new(DEFAULT_LISTEN, None).unwrap();
    for limit in [None, Some(64 * 1024 * 1024), Some(5 * 1024 * 1024 * 1024)] {
        config.runtime.storage_limit_bytes = limit;
        config.validate().unwrap();
    }
    for limit in [0, 64 * 1024 * 1024 - 1] {
        config.runtime.storage_limit_bytes = Some(limit);
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("at least 64 MiB")
        );
    }
}

#[test]
fn additive_operator_sections_preserve_existing_version_and_reject_typos() {
    let config = GatewayConfig::new(DEFAULT_LISTEN, None).unwrap();
    let mut value = toml::Value::try_from(&config).unwrap();
    for section in [
        "connections",
        "auth",
        "computer",
        "execution",
        "model_transport",
        "runtime",
        "telemetry",
    ] {
        value.as_table_mut().unwrap().remove(section);
    }
    let old = toml::to_string(&value).unwrap();
    let loaded: GatewayConfig = toml::from_str(&old).unwrap();
    loaded.validate().unwrap();
    assert_eq!(loaded.version, CONFIG_VERSION);
    assert_eq!(
        loaded.connections,
        crate::server::ConnectionPolicy::default()
    );
    assert_eq!(loaded.execution, crate::sandbox::ExecutionConfig::default());
    let partial: GatewayConfig = toml::from_str(&format!("{old}\n[execution]\ncommand_timeout_seconds = 900\n[connections]\nauthenticated = 96\n[computer.browser]\nviewport = [1920, 1080]\n")).unwrap();
    partial.validate().unwrap();
    assert_eq!(partial.execution.command_timeout_seconds, 900);
    assert_eq!(partial.connections.authenticated, 96);
    assert_eq!(partial.computer.browser.viewport, [1920, 1080]);
    assert!(
        toml::from_str::<GatewayConfig>(&(old + "\n[execution]\ncommand_timeout_second = 900\n"))
            .is_err()
    );
}

#[test]
fn explicit_operator_policy_allows_private_http_collectors() {
    let mut config = GatewayConfig::new(DEFAULT_LISTEN, None).unwrap();
    let mut sink = serde_json::from_value::<crate::telemetry::TelemetrySink>(serde_json::json!({"id": "private", "url": "http://10.0.0.4:9000/collect", "every_seconds": 15, "sections": ["activity"]})).unwrap();
    sink.bearer_env = Some("MOBIUS_COLLECTOR_TOKEN".into());
    config.telemetry.sinks.push(sink);
    assert!(config.validate().is_err());
    config.telemetry.policy.allow_insecure_http = true;
    config.validate().unwrap();
}

#[test]
fn telemetry_credentials_require_a_dedicated_gateway_namespace() {
    let mut config = GatewayConfig::new(DEFAULT_LISTEN, None).unwrap();
    config.telemetry.sinks.push(
        serde_json::from_value(serde_json::json!({
            "id": "collector", "url": "https://collector.example/ingest", "every_seconds": 15
        }))
        .unwrap(),
    );
    for name in [
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "AWS_SECRET_ACCESS_KEY",
        "MOBIUS_",
        "MOBIUS_GATEWAY_TOKEN",
    ] {
        config.telemetry.sinks[0].bearer_env = Some(name.into());
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("bearer_env")
        );
    }
    config.telemetry.sinks[0].bearer_env = Some("MOBIUS_COLLECTOR_TOKEN".into());
    config.validate().unwrap();
}

#[test]
fn shipped_gateway_policy_defaults_preserve_existing_behavior() {
    let config = GatewayConfig::new(DEFAULT_LISTEN, None).unwrap();
    assert_eq!(config.connections.authenticated, 32);
    assert_eq!(config.connections.pre_authentication, 8);
    assert_eq!(config.connections.authentication_timeout_seconds, 5);
    assert_eq!(config.connections.active_sessions, 32);
    assert_eq!(config.connections.pending_uploads, 8);
    assert_eq!(config.auth.paired_clients, 32);
    assert_eq!(config.auth.pairing_lifetime_seconds, 600);
    assert_eq!(config.runtime.idle_exit_seconds, 259_200);
    assert!(!config.runtime.require_access_lease);
    assert_eq!(config.runtime.access_grace_seconds, 0);
    assert!(!config.telemetry.policy.allow_insecure_http);
    assert_eq!(config.telemetry.policy.request_timeout_seconds, 10);
    assert_eq!(config.telemetry.policy.upload_admission_timeout_seconds, 10);
}

#[test]
fn listed_image_model_ids_are_validated_before_saving() {
    let register = |provider: &str, image_ids: Vec<String>| {
        let mut selection = AgentComposition::default().provider;
        selection.instance = provider.into();
        selection.provider = provider.into();
        let catalog = provider == "openai_socket";
        selection.model = if catalog { "gpt-6.1-sol" } else { "chat-model" }.into();
        selection.reasoning_effort = None;
        GatewayConfig::new(DEFAULT_LISTEN, None)
            .expect("gateway config")
            .registering_provider(
                selection,
                "Test".into(),
                Default::default(),
                (if catalog {
                    Vec::new()
                } else {
                    vec!["chat-model".into()]
                })
                .into_iter()
                .map(|id| crate::wire::ConfiguredModel {
                    id,
                    reasoning_efforts: Some(Vec::new()),
                    default_reasoning: None,
                    ..Default::default()
                })
                .collect(),
                image_ids,
            )
    };
    register("openrouter", vec!["black-forest-labs/flux".into()]).expect("listed image model");
    for image_ids in [vec!["flux".into(), "flux".into()], vec![String::new()]] {
        assert!(register("openrouter", image_ids).is_err());
    }
    assert!(
        register("openai_socket", vec!["gpt-image-2.5-sunburst".into()])
            .expect_err("catalog provider")
            .to_string()
            .contains("image model IDs")
    );
}

#[test]
fn failed_credential_publication_preserves_secrets_and_revocation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credentials.json");
    let store = CredentialStore::open(path.clone()).unwrap();
    store
        .set("openai_socket", "openai_socket", "before", None, None)
        .unwrap();
    let resolved = store
        .get("openai_socket", "openai_socket", None)
        .unwrap()
        .unwrap();
    let revoked = resolved.lifetime.revoked.unwrap();
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();

    assert!(
        store
            .set("openai_socket", "openai_socket", "after", None, None)
            .is_err()
    );
    assert!(store.remove("openai_socket").is_err());
    assert_eq!(
        store
            .get("openai_socket", "openai_socket", None)
            .unwrap()
            .unwrap()
            .api_key,
        "before"
    );
    assert!(!revoked.has_changed().unwrap());
    assert!(
        store
            .set("other", "openai_socket", "new", None, None)
            .is_err()
    );
    assert!(store.get("other", "openai_socket", None).unwrap().is_none());
}

#[test]
fn persisted_api_keys_reject_non_token_bytes_without_exposing_them() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credentials.json");
    for key in [
        "secret token",
        "secret\nheader",
        " secret",
        "secret\u{7f}",
        "secreté",
    ] {
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "fixture": {"provider": "openrouter", "api_key": key}
            }))
            .unwrap(),
        )
        .unwrap();
        let Err(error) = CredentialStore::open(path.clone()) else {
            panic!("persisted malformed key must fail");
        };
        assert!(error.to_string().contains("visible ASCII"));
        assert!(!error.to_string().contains(key));
    }
}

#[test]
fn persisted_config_and_credentials_reject_oversized_files() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("gateway");
    let (store, _) =
        ConfigStore::initialize(state.clone(), "127.0.0.1:8741".parse().unwrap(), None).unwrap();
    fs::File::create(store.state_dir().join("gateway.toml"))
        .unwrap()
        .set_len(MAX_CONFIG_BYTES + 1)
        .unwrap();
    let Err(error) = ConfigStore::open(state) else {
        panic!("oversized config must fail")
    };
    assert!(error.to_string().contains("too large"));
    let path = directory.path().join("credentials.json");
    fs::File::create(&path)
        .unwrap()
        .set_len(MAX_CREDENTIAL_STATE_BYTES as u64 + 1)
        .unwrap();
    let Err(error) = CredentialStore::open(path) else {
        panic!("oversized credentials must fail")
    };
    assert!(error.to_string().contains("too large"));
}

#[cfg(unix)]
#[test]
fn persisted_config_and_credentials_reject_fifos_without_a_writer() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("gateway");
    let (store, _) =
        ConfigStore::initialize(state.clone(), "127.0.0.1:8741".parse().unwrap(), None).unwrap();
    let config_path = store.state_dir().join("gateway.toml");
    fs::remove_file(&config_path).unwrap();
    let credentials = directory.path().join("credentials.json");
    for path in [config_path.as_path(), credentials.as_path()] {
        assert!(
            std::process::Command::new("mkfifo")
                .arg(path)
                .status()
                .unwrap()
                .success()
        );
    }
    let Err(error) = ConfigStore::open(state) else {
        panic!("FIFO config must fail")
    };
    assert!(error.to_string().contains("regular file"));
    let Err(error) = CredentialStore::open(credentials) else {
        panic!("FIFO credentials must fail")
    };
    assert!(error.to_string().contains("regular file"));
}

#[test]
fn applied_credential_publication_keeps_new_bytes_and_revokes_old_lifetime() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("credentials.json");
    let store = CredentialStore::open(path.clone()).unwrap();
    store
        .set("openai_socket", "openai_socket", "before", None, None)
        .unwrap();
    let revoked = store
        .get("openai_socket", "openai_socket", None)
        .unwrap()
        .unwrap()
        .lifetime
        .revoked
        .unwrap();
    crate::publication::fail_next_directory_sync(&path);
    assert!(matches!(
        store.set("openai_socket", "openai_socket", "after", None, None),
        Err(Error::PublicationApplied { .. })
    ));
    assert!(revoked.has_changed().is_err());
    for owner in [&store, &CredentialStore::open(path.clone()).unwrap()] {
        assert_eq!(
            owner
                .get("openai_socket", "openai_socket", None)
                .unwrap()
                .unwrap()
                .api_key,
            "after"
        );
    }
    let revoked = store
        .get("openai_socket", "openai_socket", None)
        .unwrap()
        .unwrap()
        .lifetime
        .revoked
        .unwrap();
    crate::publication::fail_next_directory_sync(&path);
    assert!(matches!(
        store.remove("openai_socket"),
        Err(Error::PublicationApplied { .. })
    ));
    assert!(revoked.has_changed().is_err());
    assert!(
        store
            .get("openai_socket", "openai_socket", None)
            .unwrap()
            .is_none()
    );
    assert!(
        CredentialStore::open(path)
            .unwrap()
            .get("openai_socket", "openai_socket", None)
            .unwrap()
            .is_none()
    );
}

#[test]
fn usage_updates_do_not_reopen_unchanged_cloudflare_credentials() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut config) = ConfigStore::initialize_named_cloudflare(
        root.path().join("state"),
        DEFAULT_LISTEN,
        "mobius.example.com",
        "secret-tunnel-token",
    )
    .unwrap();
    fs::remove_file(store.cloudflare_token_path()).unwrap();
    let usage = TokenUsage {
        input_tokens: 7,
        total_tokens: 7,
        ..TokenUsage::default()
    };
    store
        .record_usage(&mut config, "openai_socket", &usage)
        .unwrap();
    let persisted: GatewayConfig =
        toml::from_str(&fs::read_to_string(store.state_dir().join(CONFIG_FILE)).unwrap()).unwrap();
    assert_eq!(persisted.usage, config.usage);
    assert_ne!(config.usage, Default::default());
    assert!(store.save(&config).is_err());
}

#[test]
fn applied_usage_publication_keeps_visible_totals_but_returns_uncertainty() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let (store, mut config) =
        ConfigStore::initialize(state.clone(), "127.0.0.1:8741".parse().unwrap(), None).unwrap();
    crate::publication::fail_next_directory_sync(&state.join("gateway.toml"));
    let usage = TokenUsage {
        input_tokens: 7,
        total_tokens: 7,
        ..TokenUsage::default()
    };
    assert!(matches!(
        store.record_usage(&mut config, "openai_socket", &usage),
        Err(Error::PublicationApplied { .. })
    ));
    assert_eq!(
        serde_json::to_value(&config).unwrap(),
        serde_json::to_value(ConfigStore::open(state).unwrap().1).unwrap()
    );
    assert_ne!(config.usage, Default::default());
}

#[test]
fn configured_models_keep_independent_efforts_and_explicit_defaults() {
    let mut selection = AgentComposition::default().provider;
    selection.instance = "responses".into();
    selection.provider = "responses".into();
    selection.model = "reasoner".into();
    selection.reasoning_effort = None;
    selection.base_url = Some("http://127.0.0.1:11434/v1".into());
    selection.web_search = mobius::backend::model::provider::HostedWebSearch::Off;
    let mut configured = ConfiguredProvider {
        selection,
        label: "Custom".into(),
        tint: Default::default(),
        models: crate::provider_catalog::prepare_configured_models(
            provider("responses").unwrap(),
            vec![
                ConfiguredModel {
                    id: "reasoner".into(),
                    reasoning_efforts: Some(vec!["low".into(), "high".into()]),
                    default_reasoning: Some("high".into()),
                    ..Default::default()
                },
                ConfiguredModel {
                    id: "fast".into(),
                    reasoning_efforts: Some(vec!["brief".into()]),
                    default_reasoning: Some("brief".into()),
                    ..Default::default()
                },
                ConfiguredModel {
                    id: "plain".into(),
                    reasoning_efforts: Some(Vec::new()),
                    default_reasoning: None,
                    ..Default::default()
                },
            ],
            Vec::new(),
        ),
        image_model_ids: Vec::new(),
    };
    validation::validate_configured_provider(&configured).expect("heterogeneous catalog");
    let definition = provider("responses").unwrap();
    assert_eq!(
        effective_reasoning_effort(definition, &configured, &configured.selection),
        Some("high")
    );
    configured.selection.model = "fast".into();
    assert_eq!(
        effective_reasoning_effort(definition, &configured, &configured.selection),
        Some("brief")
    );
    configured.selection.reasoning_effort = Some("high".into());
    assert!(validation::validate_configured_provider(&configured).is_err());
    configured.selection.reasoning_effort = None;
    let encoded = toml::to_string(&configured).unwrap();
    assert_eq!(
        toml::from_str::<ConfiguredProvider>(&encoded).unwrap(),
        configured
    );
    assert!(encoded.contains("[[models]]"));
    for default in [None, Some("absent".into())] {
        configured.models[0].default_reasoning = default;
        assert!(validation::validate_configured_provider(&configured).is_err());
    }
    configured.models[0].default_reasoning = Some("high".into());
    configured.models[2].default_reasoning = Some("high".into());
    assert!(validation::validate_configured_provider(&configured).is_err());
    assert!(toml::from_str::<ConfiguredModel>("id = 'plain'\nunknown = true").is_err());
}

#[test]
fn heterogeneous_model_route_budget_counts_each_models_variants() {
    let mut selection = AgentComposition::default().provider;
    selection.instance = "responses".into();
    selection.provider = "responses".into();
    let mut configured = ConfiguredProvider {
        selection,
        label: "Custom".into(),
        tint: Default::default(),
        models: crate::provider_catalog::prepare_configured_models(
            provider("responses").unwrap(),
            vec![
                ConfiguredModel {
                    id: "reasoner".into(),
                    reasoning_efforts: Some(
                        (0..63).map(|index| format!("effort-{index}")).collect(),
                    ),
                    default_reasoning: Some("effort-0".into()),
                    ..Default::default()
                },
                ConfiguredModel {
                    id: "plain".into(),
                    reasoning_efforts: Some(Vec::new()),
                    default_reasoning: None,
                    ..Default::default()
                },
            ],
            Vec::new(),
        ),
        image_model_ids: Vec::new(),
    };
    let mut providers = BTreeMap::new();
    providers.insert("responses".into(), configured.clone());
    validation::validate_custom_model_route_count(&providers)
        .expect("63 variants plus one plain route");
    configured.models[0]
        .reasoning
        .push(mobius::backend::model::provider::ReasoningPreset {
            id: "effort-63".into(),
            label: "effort-63".into(),
            description: String::new(),
        });
    providers.insert("responses".into(), configured);
    assert!(
        validation::validate_custom_model_route_count(&providers)
            .unwrap_err()
            .to_string()
            .contains("at most 64 model routes")
    );
}

#[test]
fn custom_routes_reject_plain_and_reasoning_collision_in_either_order() {
    let mut selection = AgentComposition::default().provider;
    selection.instance = "responses".into();
    selection.provider = "responses".into();
    let mut configured = ConfiguredProvider {
        selection,
        label: "Custom".into(),
        tint: Default::default(),
        models: crate::provider_catalog::prepare_configured_models(
            provider("responses").unwrap(),
            vec![
                ConfiguredModel {
                    id: "foo".into(),
                    reasoning_efforts: Some(vec!["bar::default".into()]),
                    default_reasoning: Some("bar::default".into()),
                    ..Default::default()
                },
                ConfiguredModel {
                    id: "foo::bar".into(),
                    reasoning_efforts: Some(Vec::new()),
                    default_reasoning: None,
                    ..Default::default()
                },
            ],
            Vec::new(),
        ),
        image_model_ids: Vec::new(),
    };
    for _ in 0..2 {
        let providers = BTreeMap::from([("responses".into(), configured.clone())]);
        assert!(
            validation::validate_custom_model_route_count(&providers)
                .unwrap_err()
                .to_string()
                .contains("ambiguous route")
        );
        configured.models.reverse();
    }
}
