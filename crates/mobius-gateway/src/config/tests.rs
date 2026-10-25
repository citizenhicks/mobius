use super::*;

fn test_bot() -> crate::wire::BotRecord {
    crate::wire::BotRecord {
        id: "bot-fixture".into(),
        conversation_session_id: mobius::SessionId::new("persistent-test").unwrap(),
        handle: "fixture".into(),
        name: "Fixture".into(),
        description: "Own fixture work.".into(),
        tint: ProviderTint::default(),
        shape: crate::wire::BotShape::Circle,
        config: Arc::new(VersionedAgentConfig {
            revision: 1,
            config: AgentComposition::default(),
        }),
        capabilities: crate::wire::BotCapabilities {
            attachments: false,
            computer: false,
            voice: crate::wire::VoiceCapability::Off,
        },
        routine_interaction_policy: RoutineInteractionPolicy::Unattended,
    }
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

#[test]
fn provider_registration_never_silently_changes_existing_defaults() {
    let config = GatewayConfig::new(DEFAULT_LISTEN, None).expect("gateway config");
    let kimi = ProviderConfig {
        tool_discovery: None,
        instance: "kimi".into(),
        provider: "kimi".into(),
        model: "kimi-k3".into(),
        base_url: Some(BaseUrl::new("https://api.moonshot.ai/v1").unwrap()),
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
        base_url: Some(BaseUrl::new("https://openrouter.ai/api/v1/").unwrap()),
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
fn saved_search_options_do_not_prevent_loading_but_fail_when_built() {
    let selection = ProviderConfig {
        instance: "deepseek".into(),
        provider: "deepseek".into(),
        model: "deepseek-flash".into(),
        base_url: None,
        reasoning_effort: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Live,
        ..AgentComposition::default().provider
    };
    validation::validate_provider_config(&selection).expect("saved selection remains readable");
    assert!(
        provider(&selection.provider)
            .unwrap()
            .build_config_is_valid(
                &selection.model,
                selection.base_url.as_ref(),
                selection.reasoning_effort.as_deref(),
                selection.web_search,
            )
            .is_err()
    );
}

#[test]
fn credentialless_endpoint_requires_https() {
    let selection = ProviderConfig {
        tool_discovery: None,
        instance: "openrouter".into(),
        provider: "openrouter".into(),
        model: "openai/gpt-5".into(),
        base_url: Some(BaseUrl::new("http://127.0.0.1:8080/v1").unwrap()),
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
        conversation_session_id: mobius::SessionId::new("persistent-test").unwrap(),
        handle: "fixture".into(),
        name: "Fixture".into(),
        description: "Own fixture work.".into(),
        tint: ProviderTint::default(),
        shape: crate::wire::BotShape::Circle,
        config: Arc::new(registered.bot_defaults.clone().expect("Bot defaults")),
        capabilities: crate::wire::BotCapabilities {
            attachments: false,
            computer: false,
            voice: crate::wire::VoiceCapability::Off,
        },
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
        .create_bot(
            "Fixture",
            "Own fixture work.",
            AgentComposition::default().into(),
        )
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
            Some(&BaseUrl::new("https://openrouter.ai/api/v1/").unwrap()),
            Some(2_000_000_000),
        )
        .expect("store credential");
    let error = credentials
        .set(
            "openrouter",
            "responses",
            "replacement-secret",
            Some(&BaseUrl::new("https://example.com/v1").unwrap()),
            None,
        )
        .expect_err("an existing instance must keep its provider");

    let reopened = CredentialStore::open(path.clone()).expect("reopen expiring credential");
    assert_eq!(
        reopened
            .get(
                "openrouter",
                "openrouter",
                Some(&BaseUrl::new("https://openrouter.ai/api/v1/").unwrap())
            )
            .unwrap()
            .and_then(|credential| credential.lifetime.expires_at),
        Some(UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000)),
    );
    assert_eq!(
        reopened
            .get(
                "openrouter",
                "openrouter",
                Some(&BaseUrl::new("https://other.example/v1").unwrap())
            )
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
                Some(&BaseUrl::new("https://openrouter.ai/api/v1/").unwrap())
            )
            .expect("original credential")
            .map(|credential| credential.api_key),
        Some("write-only-secret".into())
    );
    assert!(!snapshot.contains("write-only-secret"));
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
                Some(&BaseUrl::new("https://openrouter.ai/api/v1/").unwrap()),
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
            Some(&BaseUrl::new("https://openrouter.ai/api/v1/").unwrap()),
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
                Some(&BaseUrl::new("https://openrouter.ai/api/v1/").unwrap()),
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
                Some(&BaseUrl::new("https://openrouter.ai/api/v1/").unwrap()),
            )
            .expect("read rejected credential")
            .map(|credential| credential.api_key),
        None
    );
}

#[cfg(unix)]
#[test]
fn cloudflare_token_is_owner_only_and_absent_from_gateway_config() {
    let state_parent = tempfile::tempdir().expect("state parent");
    let state = state_parent.path().join("gateway");
    let (store, _) = ConfigStore::initialize_named_cloudflare(
        state.clone(),
        DEFAULT_LISTEN,
        NamedTunnel::new("mobius.example.com", "secret-tunnel-token").expect("tunnel"),
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

#[tokio::test]
async fn replacing_and_removing_credentials_revokes_resolved_routes() {
    let directory = tempfile::tempdir().unwrap();
    let store = CredentialStore::open(directory.path().join("credentials.json")).unwrap();
    let url = BaseUrl::new("https://api.openai.com/v1").unwrap();
    let base_url = Some(&url);
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
