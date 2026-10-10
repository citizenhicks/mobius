use super::*;
use clap::CommandFactory as _;
use mobius::backend::model::provider::HostedWebSearch;

static BOOTSTRAP_TEST_CLIENT: std::sync::Mutex<Option<(Endpoint, String)>> =
    std::sync::Mutex::new(None);
static REGISTER_PROVIDER_TEST_CLIENT: std::sync::Mutex<Option<(Endpoint, String)>> =
    std::sync::Mutex::new(None);

fn save_bootstrap_test_client(endpoint: &Endpoint, token: String) -> Result<()> {
    *BOOTSTRAP_TEST_CLIENT
        .lock()
        .expect("bootstrap test client lock") = Some((endpoint.clone(), token));
    Ok(())
}

fn reject_bootstrap_test_client(_endpoint: &Endpoint, _token: String) -> Result<()> {
    Err(Error::Config("test token save failed".into()))
}

fn load_register_provider_test_client(endpoint: &Endpoint) -> Result<Option<String>> {
    Ok(REGISTER_PROVIDER_TEST_CLIENT
        .lock()
        .expect("register-provider test client lock")
        .as_ref()
        .filter(|(configured, _)| configured == endpoint)
        .map(|(_, token)| token.clone()))
}

#[cfg(target_os = "macos")]
#[test]
fn macos_background_gateway_path_preserves_custom_priority_and_supports_gui_launches() {
    for (inherited, expected) in [
        (
            None,
            "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin",
        ),
        (
            Some("/usr/bin:/bin:/usr/sbin:/sbin"),
            "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin",
        ),
        (
            Some("/custom/node/bin:/usr/bin:/opt/homebrew/bin"),
            "/custom/node/bin:/usr/bin:/opt/homebrew/bin:/opt/homebrew/bin:/usr/local/bin",
        ),
    ] {
        assert_eq!(
            macos_gateway_path(inherited.map(std::ffi::OsStr::new)),
            OsString::from(expected)
        );
    }
}

#[cfg(unix)]
#[test]
fn reset_gateway_state_removes_an_empty_directory() {
    let root = tempfile::tempdir().expect("state parent");
    let state = root.path().join("gateway");
    std::fs::create_dir(&state).expect("empty state");

    reset_gateway_state(state.clone()).expect("reset empty state");

    assert!(!state.exists());
}

#[cfg(unix)]
#[test]
fn reset_gateway_state_removes_incompatible_marked_state() {
    let root = tempfile::tempdir().expect("state parent");
    let state = root.path().join("gateway");
    std::fs::create_dir(&state).expect("gateway state");
    std::fs::write(state.join(STATE_MARKER_FILE), "version = 999\n").expect("incompatible marker");

    reset_gateway_state(state.clone()).expect("reset incompatible state");

    assert!(!state.exists());
}

#[cfg(unix)]
#[test]
fn reset_gateway_state_preserves_an_unrelated_nonempty_directory() {
    let root = tempfile::tempdir().expect("state parent");
    let state = root.path().join("not-mobius");
    std::fs::create_dir(&state).expect("unrelated directory");
    let unrelated = state.join("keep.txt");
    std::fs::write(&unrelated, "keep").expect("unrelated file");

    let error = reset_gateway_state(state).expect_err("unrelated state must be refused");

    assert!(error.to_string().contains("refusing to reset") && unrelated.exists());
}

#[cfg(unix)]
#[test]
fn reset_gateway_state_refuses_a_symlinked_directory() {
    let root = tempfile::tempdir().expect("state parent");
    let real = root.path().join("real");
    let link = root.path().join("gateway");
    std::fs::create_dir(&real).expect("real directory");
    std::fs::write(real.join(STATE_MARKER_FILE), "version = 999\n").expect("gateway marker");
    std::os::unix::fs::symlink(&real, &link).expect("gateway symlink");

    reset_gateway_state(link).expect_err("symlinked state must be refused");

    assert!(real.exists());
}

#[test]
fn failed_auth_initialization_removes_only_the_new_gateway_state() {
    let root = tempfile::tempdir().expect("state parent");
    let state = root.path().join("gateway");
    let sibling = root.path().join("keep");
    std::fs::write(&sibling, "keep").expect("sibling state");
    let (store, config) =
        ConfigStore::initialize(state.clone(), DEFAULT_LISTEN, None).expect("gateway config");
    std::fs::create_dir(store.auth_path()).expect("conflicting auth path");

    initialize_auth(&store, config.auth).expect_err("auth initialization must fail");

    assert_eq!((state.exists(), sibling.exists()), (false, true));
}

#[test]
fn default_initialization_enables_quick_cloudflare_and_loopback() {
    let directory = tempfile::tempdir().expect("gateway state parent");
    let state = directory.path().join("gateway");

    initialize(InitOptions {
        state_dir: state.clone(),
        listen: DEFAULT_LISTEN,
        tls: None,
        cloudflare: None,
    })
    .expect("initialize gateway");
    let (_, config) = ConfigStore::open(state).expect("open gateway config");

    assert_eq!(
        (config.cloudflare, config.listen),
        (Some(CloudflareConfig::Quick), DEFAULT_LISTEN)
    );
}

#[test]
fn bootstrap_is_direct_and_saves_an_authenticated_control_client() {
    let directory = tempfile::tempdir().expect("gateway state parent");
    let state = directory.path().join("gateway");
    *BOOTSTRAP_TEST_CLIENT
        .lock()
        .expect("bootstrap test client lock") = None;

    initialize_bootstrap(state.clone(), save_bootstrap_test_client)
        .expect("initialize gateway bootstrap");

    let (store, config) = ConfigStore::open(state).expect("open bootstrap config");
    assert!(config.listen.ip().is_loopback());
    assert!(config.tls.is_none() && config.cloudflare.is_none());
    let (endpoint, token) = BOOTSTRAP_TEST_CLIENT
        .lock()
        .expect("bootstrap test client lock")
        .take()
        .expect("saved bootstrap client");
    assert_eq!(endpoint.to_string(), "tcp://127.0.0.1:8741");
    assert!(
        AuthStore::open(store.auth_path(), config.auth)
            .expect("open hosted auth")
            .authenticate(&token)
            .is_ok()
    );
}

#[test]
fn bootstrap_cleans_state_when_the_control_token_cannot_be_saved() {
    let directory = tempfile::tempdir().expect("gateway state parent");
    let state = directory.path().join("gateway");
    let sibling = directory.path().join("keep");
    std::fs::write(&sibling, "keep").expect("sibling state");

    initialize_bootstrap(state.clone(), reject_bootstrap_test_client)
        .expect_err("token save must fail initialization");

    assert_eq!((state.exists(), sibling.exists()), (false, true));
}

#[cfg(unix)]
#[test]
fn reset_bot_defaults_reapplies_operator_bounded_defaults_without_changing_other_gateway_state() {
    let directory = tempfile::tempdir().expect("gateway state parent");
    let state = directory.path().join("gateway");
    let (store, mut config) = ConfigStore::initialize(state.clone(), DEFAULT_LISTEN, None)
        .expect("initialize gateway config");
    config.execution.subagent_ceilings =
        mobius::middleware::subagents::SubagentCeilings::new(1, 2, 3).expect("operator ceilings");
    let provider = crate::wire::AgentComposition::default().provider;
    let config = config
        .registering_provider(
            provider.clone(),
            "Primary".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("register provider");
    let current = config.bot_defaults.as_ref().expect("Bot defaults");
    let mut custom = current.config.clone();
    custom.system_prompt = "custom prompt".into();
    custom.max_model_steps = 3;
    custom.middleware.set_enabled("tasks", true);
    let config = config
        .replacing_bot_defaults(current.revision, custom)
        .expect("customize defaults");
    store.save(&config).expect("save customized defaults");
    let current = config.bot_defaults.as_ref().expect("custom Bot defaults");
    let expected = config
        .replacing_bot_defaults(
            current.revision,
            crate::wire::AgentComposition {
                provider,
                ..crate::wire::AgentComposition::defaults_with_ceilings(
                    config.execution.subagent_ceilings,
                )
            },
        )
        .expect("expected reset");

    reset_bot_defaults(state.clone()).expect("reset Bot defaults");
    let (_, actual) = ConfigStore::open(state).expect("open reset config");

    assert_eq!(actual, expected);
}

#[test]
fn bootstrap_commands_reject_tunnel_configuration() {
    let config = GatewayConfig::new_cloudflare(DEFAULT_LISTEN, CloudflareConfig::Quick)
        .expect("Cloudflare config");

    let error = direct_loopback_endpoint(&config).expect_err("tunnel config must fail");

    assert!(error.to_string().contains("direct plaintext loopback"));
}

#[test]
fn cloudflare_local_client_uses_the_authenticated_loopback_endpoint() {
    let directory = tempfile::tempdir().expect("gateway state");
    let path = directory.path().join("auth.json");
    let (auth, _) =
        AuthStore::initialize(path, crate::auth::AuthConfig::default()).expect("initialize auth");
    let config = GatewayConfig::new_cloudflare(DEFAULT_LISTEN, CloudflareConfig::Quick)
        .expect("Cloudflare config");

    let (endpoint, token) = provision_cloudflare_local_client(&auth, &config)
        .expect("provision local client")
        .expect("Cloudflare local client");

    assert_eq!(endpoint.to_string(), "tcp://127.0.0.1:8741");
    assert!(auth.authenticate(&token).is_ok());
}

#[cfg(unix)]
#[tokio::test]
async fn serving_a_tunnel_respects_configured_client_capacity_before_provisioning() {
    let directory = tempfile::tempdir().expect("gateway state");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve loopback port");
    let (store, mut config) = ConfigStore::initialize_quick_cloudflare(
        directory.path().join("gateway"),
        listener.local_addr().expect("loopback listener"),
    )
    .expect("gateway config");
    config.auth.paired_clients = 1;
    store.save(&config).expect("save client capacity");
    let (auth, grant) =
        AuthStore::initialize(store.auth_path(), config.auth).expect("initialize auth");
    let paired = auth
        .pair(&grant.code, "Existing client")
        .expect("pair client");
    drop(listener);

    let error = tokio::time::timeout(
        Duration::from_secs(5),
        serve(
            store.state_dir().to_path_buf(),
            true,
            reject_bootstrap_test_client,
            |_| Ok(None),
        ),
    )
    .await
    .expect("capacity rejection precedes tunnel startup")
    .expect_err("local operator cannot exceed client capacity");
    assert!(error.to_string().contains("paired client limit reached"));
    let reopened = AuthStore::open(store.auth_path(), config.auth).expect("auth store");
    assert!(reopened.authenticate(&paired.token).is_ok());
}

#[test]
fn pairing_setup_payload_formats_a_wss_endpoint() {
    let endpoint = "wss://mobius.example.com".parse().expect("WSS endpoint");

    assert_eq!(
        pairing_setup_payload(&endpoint, "one-time-code"),
        "mobius-pair:v1|wss://mobius.example.com|one-time-code"
    );
}

#[test]
fn cloudflare_connection_advertises_public_and_local_endpoints_with_one_code() {
    let public_endpoint = "wss://mobius.example.com".parse().expect("WSS endpoint");
    let local_endpoint = "tcp://127.0.0.1:8741".parse().expect("TCP endpoint");
    let mut output = Vec::new();

    write_connection(
        &mut output,
        &public_endpoint,
        Some(&local_endpoint),
        "one-time-code",
    )
    .expect("write connection");

    assert_eq!(
        String::from_utf8(output).expect("UTF-8 output"),
        "public endpoint: wss://mobius.example.com\n\
             local endpoint: tcp://127.0.0.1:8741\n\
             one-time code: one-time-code\n\
             setup code: mobius-pair:v1|wss://mobius.example.com|one-time-code\n\
             copy the setup code into möbius\n\
             another terminal: mobius pair wss://mobius.example.com one-time-code\n\
             local terminal: mobius pair tcp://127.0.0.1:8741 one-time-code\n"
    );
}

#[test]
fn command_definition_is_valid() {
    GatewayCli::command().debug_assert();
}

#[test]
fn parse_serve_accepts_an_explicit_state_directory() {
    let command = parse(vec![
        "serve".into(),
        "--state-dir".into(),
        "/tmp/mobius".into(),
    ])
    .expect("parse serve");

    assert!(matches!(
        command,
        Command::Serve {
            state_dir,
            background: false,
        } if state_dir == std::path::Path::new("/tmp/mobius")
    ));
}

#[test]
fn parse_connect_accepts_a_public_endpoint_and_state_directory() {
    let command = parse(vec![
        "connect".into(),
        "--endpoint".into(),
        "tls://gateway.example:443".into(),
        "--state-dir".into(),
        "/tmp/mobius".into(),
    ])
    .expect("parse connect");

    assert!(matches!(
        command,
        Command::Connect(ConnectOptions { state_dir, endpoint: Some(endpoint) })
            if state_dir == std::path::Path::new("/tmp/mobius")
                && endpoint.to_string() == "tls://gateway.example:443"
    ));
}

#[test]
fn parse_bootstrap_commands_accept_only_their_machine_interface() {
    let init = parse(vec![
        "bootstrap".into(),
        "--state-dir".into(),
        "/tmp/mobius".into(),
    ])
    .expect("parse bootstrap");
    assert!(matches!(
        init,
        Command::Bootstrap { state_dir } if state_dir == std::path::Path::new("/tmp/mobius")
    ));

    let pair = parse(vec![
        "pairing-code".into(),
        "--state-dir".into(),
        "/tmp/mobius".into(),
        "--json".into(),
    ])
    .expect("parse pairing code");
    assert!(matches!(
        pair,
        Command::PairingCode { state_dir } if state_dir == std::path::Path::new("/tmp/mobius")
    ));

    assert!(parse(vec!["pairing-code".into()]).is_err());
}

#[test]
fn parse_reset_bot_defaults_accepts_an_explicit_state_directory() {
    let command = parse(vec![
        "reset-bot-defaults".into(),
        "--state-dir".into(),
        "/tmp/mobius".into(),
    ])
    .expect("parse default reset");

    assert!(matches!(
        command,
        Command::ResetBotDefaults { state_dir }
            if state_dir == std::path::Path::new("/tmp/mobius")
    ));
}

#[test]
fn parse_register_provider_accepts_credentialless_endpoint_configuration() {
    let command = parse(vec![
        "register-provider".into(),
        "--state-dir".into(),
        "/tmp/mobius".into(),
        "--provider".into(),
        "openrouter".into(),
        "--model".into(),
        "openai/gpt-5".into(),
        "--web-search".into(),
        "live".into(),
        "--base-url".into(),
        "https://connector.example/v1".into(),
        "--credentialless".into(),
    ])
    .expect("parse provider registration");

    assert!(matches!(
           command,
           Command::RegisterProvider(RegisterProviderOptions {
    preserve_selection: false,
    if_configured: false,
               state_dir,
               provider,
               instance: None,
               label: None,
               model,
               model_ids: None,
               models: None,
               image_model_ids: None,
               tool_discovery: None,
               service_tier: None,
               web_search: HostedWebSearch::Live,
               base_url: Some(base_url),
               credentialless: true,
               credential_stdin: false,
               credential_expires_at: None,
           }) if state_dir == std::path::Path::new("/tmp/mobius")
               && provider == "openrouter"
               && model == "openai/gpt-5"
               && base_url == "https://connector.example/v1"
       ));
}

#[test]
fn parse_register_provider_accepts_repeated_model_ids() {
    let command = parse(
        [
            "register-provider",
            "--provider",
            "openrouter",
            "--model",
            "chat-a",
            "--model-id",
            "chat-a",
            "--model-id",
            "chat-b",
            "--image-model-id",
            "image-a",
            "--image-model-id",
            "image-b",
        ]
        .into_iter()
        .map(Into::into)
        .collect(),
    )
    .expect("parse model lists");
    let Command::RegisterProvider(options) = command else {
        panic!("expected provider registration");
    };
    assert_eq!(options.model_ids.expect("chat IDs"), ["chat-a", "chat-b"]);
    assert_eq!(
        options.image_model_ids.expect("image IDs"),
        ["image-a", "image-b"]
    );
}

#[test]
fn parse_register_provider_accepts_a_piped_credential() {
    let command = parse(vec![
        "register-provider".into(),
        "--provider".into(),
        "openrouter".into(),
        "--model".into(),
        "openai/gpt-5".into(),
        "--credential-stdin".into(),
        "--service-tier".into(),
        "priority".into(),
    ])
    .expect("parse provider credential input");

    assert!(matches!(
        command,
        Command::RegisterProvider(RegisterProviderOptions {
            credentialless: false,
            credential_stdin: true,
            credential_expires_at: None,
            service_tier: Some(tier),
            ..
        }) if tier == "priority"
    ));
    assert!(
        parse(vec![
            "register-provider".into(),
            "--provider".into(),
            "openrouter".into(),
            "--model".into(),
            "openai/gpt-5".into(),
            "--credentialless".into(),
            "--credential-stdin".into(),
        ])
        .is_err()
    );
}

#[test]
fn provider_credential_stdin_is_bounded() {
    assert_eq!(
        read_provider_credential(std::io::Cursor::new(b"secret\n"))
            .expect("read provider credential"),
        "secret\n"
    );
    assert!(
        read_provider_credential(std::io::Cursor::new(vec![
            b'x';
            crate::config::MAX_PROVIDER_API_KEY_BYTES
                + 1
        ]))
        .expect_err("oversized provider credential")
        .to_string()
        .contains("API key must be")
    );
}

#[test]
fn parse_register_provider_model_json_rejects_mixed_or_invalid_catalogs() {
    assert!(
        parse(
            [
                "register-provider",
                "--provider",
                "openrouter",
                "--model",
                "foo",
                "--models-json",
                "[]",
                "--model-id",
                "foo",
            ]
            .into_iter()
            .map(Into::into)
            .collect()
        )
        .is_err()
    );
    assert!(
        parse(
            [
                "register-provider",
                "--provider",
                "openrouter",
                "--model",
                "foo",
                "--models-json",
                "not json"
            ]
            .into_iter()
            .map(Into::into)
            .collect()
        )
        .is_err()
    );
}

#[test]
fn register_provider_success_json_is_stable() {
    assert_eq!(
        register_provider_json("openrouter").expect("provider registration JSON"),
        r#"{"provider":"openrouter"}"#
    );
}

#[tokio::test]
async fn register_provider_command_is_idempotent() {
    let directory = tempfile::tempdir().expect("gateway state");
    let state = directory.path().join("gateway");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = listener.local_addr().unwrap();
    let (store, config) = ConfigStore::initialize(state.clone(), listen, None).unwrap();
    let (auth, grant) = AuthStore::initialize(store.auth_path(), config.auth).unwrap();
    let operator = auth.provision_local_client().unwrap();
    drop(listener);
    let server = GatewayServer::open(state.clone()).await.unwrap();
    let endpoint: Endpoint = format!("tcp://{}", server.listen_addr())
        .parse()
        .expect("gateway endpoint");
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        let _ = signal.await;
    }));
    let (dashboard, _identity) = GatewayClient::pair(
        &endpoint,
        grant.code,
        "provider setup",
        ClientKind::GatewayDashboard,
    )
    .await
    .expect("pair provider setup client");
    *REGISTER_PROVIDER_TEST_CLIENT
        .lock()
        .expect("register-provider test client lock") = Some((endpoint, operator.token));

    register_provider_command(
        RegisterProviderOptions {
 preserve_selection: false,
 if_configured: false,
            state_dir: state.clone(),
            provider: "openrouter".into(),
            instance: None,
            label: Some("Work".into()),
            model: "openai/gpt-5".into(),
            models: Some(serde_json::from_str(r#"[{"id":"openai/gpt-5","reasoning_efforts":["medium","high"],"default_reasoning":"medium"},{"id":"anthropic/claude-sonnet-4","reasoning_efforts":["medium","high"],"default_reasoning":"medium"}]"#).unwrap()),
            model_ids: None,
            image_model_ids: Some(vec![
                "google/gemini-image".into(),
                "openai/gpt-image".into(),
            ]),
            tool_discovery: Some(mobius::protocol::ToolDiscoveryMode::Native),
            service_tier: None,
            web_search: HostedWebSearch::Live,
            base_url: Some("https://connector.example/v1".into()),
            credentialless: true,
            credential_stdin: false,
            credential_expires_at: None,
        },
        load_register_provider_test_client,
    )
    .await
    .expect("register provider");
    let (_, persisted) = ConfigStore::open(state.clone()).expect("registered provider");
    let models = &persisted.configured_providers["openrouter"].models;
    assert_eq!(
        models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["openai/gpt-5", "anthropic/claude-sonnet-4"]
    );
    for model in models {
        assert_eq!(
            model
                .reasoning
                .iter()
                .map(|effort| effort.id.as_str())
                .collect::<Vec<_>>(),
            ["medium", "high"]
        );
        assert_eq!(model.default_reasoning.as_deref(), Some("medium"));
    }
    let default = persisted.bot_defaults.expect("Bot defaults");
    let mut selected = default.config;
    selected.provider.reasoning_effort = Some("high".into());
    let request_id = Uuid::new_v4().to_string();
    let (sender, mut events) = dashboard.into_parts();
    let metadata_request = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::RegisterProvider {
            request_id: metadata_request.clone(),
            config: selected.provider.clone(),
            preserve_selection: false,
            if_configured: false,
            label: Some("Unauthorized metadata".into()),
            tint: Some(Default::default()),
            models: vec![crate::wire::ConfiguredModel {
                id: selected.provider.model.clone(),
                context_window: Some(64000),
                ..Default::default()
            }],
            image_model_ids: Some(Vec::new()),
        })
        .await
        .unwrap();
    loop {
        let frame = events.next().await.unwrap().unwrap();
        if let ServerMessage::Rejected {
            request_id: actual,
            code,
            ..
        } = frame.message
            && actual == metadata_request
        {
            assert_eq!(code, "operator_required");
            break;
        }
    }
    sender
        .send(ClientMessage::ConfigureBotDefaults {
            request_id: request_id.clone(),
            expected_revision: default.revision,
            config: selected,
        })
        .await
        .expect("select high reasoning");
    let mut saved = false;
    for _ in 0..MAX_PENDING_FRAMES {
        let frame = events
            .next()
            .await
            .expect("Bot-default response")
            .expect("gateway connection");
        match frame.message {
            ServerMessage::GatewayConfigured {
                request_id: actual, ..
            } if actual == request_id => {
                saved = true;
                break;
            }
            ServerMessage::Rejected {
                request_id: actual,
                message,
                ..
            } if actual == request_id => panic!("Bot-default selection rejected: {message}"),
            _ => {}
        }
    }
    assert!(saved, "gateway did not confirm the Bot-default selection");

    let (store, mut persisted) = ConfigStore::open(state.clone()).expect("configured default");
    // Stale disk metadata must not replace the running gateway's setup.
    let stale = persisted
        .configured_providers
        .get_mut("openrouter")
        .unwrap();
    stale.label = "Stale name".into();
    stale.tint = crate::wire::ProviderTint::Purple;
    stale.image_model_ids = vec!["stale-image".into()];
    let model = &mut persisted
        .configured_providers
        .get_mut("openrouter")
        .unwrap()
        .models[1];
    model.reasoning = ["low", "high"]
        .into_iter()
        .map(|id| mobius::backend::model::provider::ReasoningPreset {
            id: id.into(),
            label: id.into(),
            description: String::new(),
        })
        .collect();
    model.default_reasoning = Some("high".into());
    store.save(&persisted).expect("custom provider tint");

    register_provider_command(
        RegisterProviderOptions {
            preserve_selection: false,
            if_configured: false,
            state_dir: state.clone(),
            provider: "openrouter".into(),
            instance: None,
            label: None,
            model: "openai/gpt-5".into(),
            model_ids: None,
            models: None,
            image_model_ids: None,
            tool_discovery: None,
            service_tier: None,
            web_search: HostedWebSearch::Off,
            base_url: Some("https://connector.example/v1".into()),
            credentialless: true,
            credential_stdin: false,
            credential_expires_at: None,
        },
        load_register_provider_test_client,
    )
    .await
    .expect("register provider again");

    let (_, config) = ConfigStore::open(state.clone()).expect("persisted gateway config");
    let configured = &config.configured_providers["openrouter"];
    assert_eq!(
        (
            config.configured_providers.len(),
            configured.label.as_str(),
            configured.tint,
            configured.selection.endpoint_auth,
            configured.selection.web_search,
            configured.models[0].id.as_str(),
            configured.models[0]
                .reasoning
                .iter()
                .map(|effort| effort.id.as_str())
                .collect::<Vec<_>>(),
            config
                .bot_defaults
                .as_ref()
                .expect("Bot defaults")
                .config
                .provider
                .reasoning_effort
                .as_deref(),
        ),
        (
            1,
            "Work",
            crate::wire::ProviderTint::default(),
            crate::wire::ProviderEndpointAuth::Credentialless,
            HostedWebSearch::Off,
            "openai/gpt-5",
            vec!["medium", "high"],
            Some("high"),
        )
    );

    assert_eq!(
        configured.selection.tool_discovery,
        Some(mobius::protocol::ToolDiscoveryMode::Native)
    );
    assert_eq!(configured.models.len(), 2);
    assert_eq!(configured.models[1].id, "anthropic/claude-sonnet-4");
    assert_eq!(
        configured.models[1]
            .reasoning
            .iter()
            .map(|effort| effort.id.as_str())
            .collect::<Vec<_>>(),
        ["medium", "high"]
    );
    assert_eq!(
        configured.models[1].default_reasoning.as_deref(),
        Some("medium")
    );
    assert_eq!(
        configured.models[0].default_reasoning.as_deref(),
        Some("medium")
    );
    assert_eq!(
        configured.image_model_ids,
        ["google/gemini-image", "openai/gpt-image"]
    );

    let api_key = "sk-or-v1-aaaaaaaaaaaaaaaa";
    register_provider_with_credential(
        RegisterProviderOptions {
 preserve_selection: false,
 if_configured: false,
            state_dir: state.clone(),
            provider: "openrouter".into(),
            instance: Some("hosted-proxy".into()),
            label: Some("Hosted proxy".into()),
            model: "openai/gpt-5.6-luna".into(),
            model_ids: None,
            models: Some(serde_json::from_str(r#"[{"id":"openai/gpt-5.6-luna","reasoning_efforts":["medium"],"default_reasoning":"medium"}]"#).unwrap()),
            image_model_ids: None,
            tool_discovery: None,
            service_tier: None,
            web_search: HostedWebSearch::Live,
            base_url: None,
            credentialless: false,
            credential_stdin: true,
            credential_expires_at: Some(2_000_000_000),
        },
        Some(api_key.into()),
        load_register_provider_test_client,
    )
    .await
    .expect("register provider with piped credential");
    let (store, config) = ConfigStore::open(state.clone()).expect("direct provider config");
    let configured = &config.configured_providers["hosted-proxy"];
    assert_eq!(
        (
            configured.selection.endpoint_auth,
            configured.selection.base_url.as_deref(),
            crate::config::CredentialStore::open(store.credentials_path())
                .expect("direct credential store")
                .get(
                    "hosted-proxy",
                    "openrouter",
                    Some("https://openrouter.ai/api/v1")
                )
                .expect("direct credential")
                .map(|credential| credential.api_key),
        ),
        (
            crate::wire::ProviderEndpointAuth::ProviderDefault,
            Some("https://openrouter.ai/api/v1"),
            Some(api_key.into()),
        )
    );

    let stored = crate::config::CredentialStore::open(store.credentials_path()).unwrap();
    let resolved = stored
        .get(
            "hosted-proxy",
            "openrouter",
            Some("https://openrouter.ai/api/v1"),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        resolved.lifetime.expires_at,
        Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000))
    );
    register_provider_command(
        RegisterProviderOptions {
            preserve_selection: false,
            if_configured: false,
            state_dir: state.clone(),
            provider: "openrouter".into(),
            instance: Some("hosted-proxy".into()),
            label: None,
            model: "anthropic/claude-sonnet-4".into(),
            model_ids: Some(vec![
                "openai/gpt-5.6-luna".into(),
                "anthropic/claude-sonnet-4".into(),
            ]),
            models: None,
            image_model_ids: None,
            tool_discovery: None,
            service_tier: None,
            web_search: HostedWebSearch::Live,
            base_url: None,
            credentialless: false,
            credential_stdin: false,
            credential_expires_at: None,
        },
        load_register_provider_test_client,
    )
    .await
    .expect("switch to a previously unlisted model");
    let (_, config) = ConfigStore::open(state.clone()).expect("changed model");
    assert_eq!(
        config.configured_providers["hosted-proxy"]
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["openai/gpt-5.6-luna", "anthropic/claude-sonnet-4"]
    );
    for (catalog, expected_efforts, expected_default) in [
        (
            r#"[{"id":"openai/gpt-5.6-luna","reasoning_efforts":["medium","high"],"default_reasoning":"high"},{"id":"anthropic/claude-sonnet-4","reasoning_efforts":["low"],"default_reasoning":"low"}]"#,
            vec!["low"],
            Some("low"),
        ),
        (
            r#"[{"id":"openai/gpt-5.6-luna","reasoning_efforts":["medium","high"],"default_reasoning":"high"},{"id":"anthropic/claude-sonnet-4","reasoning_efforts":[]}]"#,
            vec![],
            None,
        ),
    ] {
        let Command::RegisterProvider(options) = parse(vec![
            "register-provider".into(),
            "--state-dir".into(),
            state.as_os_str().to_owned(),
            "--provider".into(),
            "openrouter".into(),
            "--instance".into(),
            "hosted-proxy".into(),
            "--model".into(),
            "anthropic/claude-sonnet-4".into(),
            "--models-json".into(),
            catalog.into(),
            "--base-url".into(),
            "https://connector.example/v1".into(),
            "--service-tier".into(),
            "priority".into(),
            "--tool-discovery".into(),
            "rebuild".into(),
            "--credential-stdin".into(),
        ])
        .expect("parse Cloud catalog") else {
            panic!("registration command")
        };
        register_provider_with_credential(
            options,
            Some(api_key.into()),
            load_register_provider_test_client,
        )
        .await
        .expect("register Cloud per-model catalog");
        let (_, persisted) = ConfigStore::open(state.clone()).unwrap();
        let registered = &persisted.configured_providers["hosted-proxy"];
        assert_eq!(
            registered.selection.tool_discovery,
            Some(mobius::protocol::ToolDiscoveryMode::Rebuild)
        );
        assert_eq!(registered.models.len(), 2);
        assert_eq!(
            registered.models[0]
                .reasoning
                .iter()
                .map(|effort| effort.id.as_str())
                .collect::<Vec<_>>(),
            ["medium", "high"]
        );
        assert_eq!(
            registered.models[0].default_reasoning.as_deref(),
            Some("high")
        );
        assert_eq!(
            registered.models[1]
                .reasoning
                .iter()
                .map(|effort| effort.id.as_str())
                .collect::<Vec<_>>(),
            expected_efforts
        );
        assert_eq!(
            registered.models[1].default_reasoning.as_deref(),
            expected_default
        );
        assert_eq!(
            registered.selection.service_tier.as_deref(),
            Some("priority")
        );
    }
    let (_, config) = ConfigStore::open(state.clone()).unwrap();
    provider::clear_provider_credential(
        state.clone(),
        "hosted-proxy".into(),
        load_register_provider_test_client,
    )
    .await
    .unwrap();
    let stored = crate::config::CredentialStore::open(store.credentials_path()).unwrap();
    assert!(
        stored
            .get(
                "hosted-proxy",
                "openrouter",
                Some("https://openrouter.ai/api/v1")
            )
            .unwrap()
            .is_none()
    );
    let (_, after_clear) = ConfigStore::open(state.clone()).unwrap();
    assert_eq!(
        after_clear.configured_providers,
        config.configured_providers
    );

    for provider_id in ["anthropic", "kimi", "deepseek"] {
        let Command::RegisterProvider(options) = parse(vec![
            "register-provider".into(),
            "--state-dir".into(),
            state.as_os_str().to_owned(),
            "--provider".into(),
            provider_id.into(),
            "--instance".into(),
            format!("cloud-{provider_id}").into(),
            "--model".into(),
            "cloud-custom-model".into(),
            "--models-json".into(),
            r#"[{"id":"cloud-custom-model","reasoning_efforts":[],"label":"Cloud custom","description":"Operator model","context_window":64000}]"#.into(),
            "--base-url".into(),
            "https://connector.example/v1".into(),
            "--credential-stdin".into(),
        ])
        .expect("parse editable backend catalog") else {
            panic!("registration command")
        };
        register_provider_with_credential(
            options,
            Some(api_key.into()),
            load_register_provider_test_client,
        )
        .await
        .expect("register editable backend catalog");
        let (_, persisted) = ConfigStore::open(state.clone()).unwrap();
        let configured = &persisted.configured_providers[&format!("cloud-{provider_id}")];
        assert_eq!(configured.selection.provider, provider_id);
        assert_eq!(
            configured.selection.base_url.as_deref(),
            Some("https://connector.example/v1")
        );
        assert_eq!(configured.models.len(), 1);
        assert_eq!(configured.models[0].id, "cloud-custom-model");
        assert_eq!(configured.models[0].label, "Cloud custom");
        assert_eq!(configured.models[0].description, "Operator model");
        assert_eq!(configured.models[0].context_window, 64000);
        assert!(configured.models[0].reasoning.is_empty());
    }

    let (_, before_refresh) = ConfigStore::open(state.clone()).unwrap();
    let before_selection = before_refresh.configured_providers["hosted-proxy"]
        .selection
        .clone();
    for instance in ["hosted-proxy", "absent-optional-provider"] {
        let Command::RegisterProvider(options) = parse(
            [
                "register-provider",
                "--state-dir",
                state.to_str().unwrap(),
                "--provider",
                "openrouter",
                "--instance",
                instance,
                "--model",
                "ignored-refresh-model",
                "--model-id",
                "replacement-chat",
                "--image-model-id",
                "replacement-image",
                "--label",
                "Replacement label",
                "--web-search",
                "live",
                "--base-url",
                "https://connector.example/v1",
                "--credentialless",
                "--preserve-selection",
                "--if-configured",
            ]
            .into_iter()
            .map(Into::into)
            .collect(),
        )
        .unwrap() else {
            panic!("registration command")
        };
        assert!(options.preserve_selection && options.if_configured);
        register_provider_command(options, load_register_provider_test_client)
            .await
            .unwrap();
    }
    let (_, after_refresh) = ConfigStore::open(state.clone()).unwrap();
    let before_provider = &before_refresh.configured_providers["hosted-proxy"];
    let after_provider = &after_refresh.configured_providers["hosted-proxy"];
    assert_eq!(after_provider.models, before_provider.models);
    assert_eq!(
        after_provider.image_model_ids,
        before_provider.image_model_ids
    );
    assert_eq!(after_provider.label, before_provider.label);
    assert_eq!(after_provider.tint, before_provider.tint);
    assert_eq!(
        after_refresh.configured_providers["hosted-proxy"]
            .selection
            .model,
        before_selection.model
    );
    assert_eq!(
        after_refresh.configured_providers["hosted-proxy"]
            .selection
            .reasoning_effort,
        before_selection.reasoning_effort
    );
    assert_eq!(
        after_refresh.configured_providers["hosted-proxy"]
            .selection
            .web_search,
        before_selection.web_search
    );
    assert_eq!(
        after_refresh.configured_providers["hosted-proxy"]
            .selection
            .tool_discovery,
        before_selection.tool_discovery
    );
    assert!(
        !after_refresh
            .configured_providers
            .contains_key("absent-optional-provider")
    );

    shutdown.send(()).expect("stop gateway");
    serving.await.expect("gateway task").expect("stop gateway");
}

#[test]
fn pairing_code_json_contains_the_code_and_expiry_only() {
    let grant = PairingGrant {
        code: "one-time-code".into(),
        expires_at: 1_234_567_890,
    };

    assert_eq!(
        pairing_code_json(&grant).expect("pairing code JSON"),
        r#"{"code":"one-time-code","expires_at":1234567890}"#
    );
}

#[test]
fn connection_endpoint_requires_an_explicit_tls_hostname() {
    let certificate = tempfile::NamedTempFile::new().expect("certificate");
    let private_key = tempfile::NamedTempFile::new().expect("private key");
    let config = GatewayConfig::new(
        "0.0.0.0:8741".parse().expect("listen"),
        Some(TlsConfig {
            certificate: certificate.path().to_path_buf(),
            private_key: private_key.path().to_path_buf(),
        }),
    )
    .expect("TLS config");

    let error = connection_endpoint(&config, None).expect_err("endpoint must be explicit");

    assert!(error.to_string().contains("--endpoint tls://HOST:PORT"));
}

#[test]
fn cloudflare_connection_uses_the_configured_wss_endpoint() {
    let config = GatewayConfig::new_cloudflare(
        DEFAULT_LISTEN,
        CloudflareConfig::named("mobius.example.com").expect("hostname"),
    )
    .expect("Cloudflare config");

    let endpoint = connection_endpoint(&config, None)
        .expect("Cloudflare endpoint")
        .expect("named endpoint");

    assert_eq!(endpoint.to_string(), "wss://mobius.example.com");
    assert!(endpoint.is_websocket());
}

#[test]
fn quick_cloudflare_connection_waits_for_the_runtime_endpoint() {
    let config = GatewayConfig::new_cloudflare(DEFAULT_LISTEN, CloudflareConfig::Quick)
        .expect("Cloudflare config");

    let endpoint = connection_endpoint(&config, None).expect("Cloudflare endpoint");

    assert!(endpoint.is_none());
}

#[test]
fn parse_serve_accepts_background_with_an_explicit_state_directory() {
    let command = parse(vec![
        "serve".into(),
        "--background".into(),
        "--state-dir".into(),
        "/tmp/mobius".into(),
    ])
    .expect("parse background serve");

    assert!(matches!(
        command,
        Command::Serve {
            state_dir,
            background: true,
        } if state_dir == std::path::Path::new("/tmp/mobius")
    ));
}

#[test]
fn parse_serve_rejects_duplicate_background_flags() {
    let error = parse(vec![
        "serve".into(),
        "--background".into(),
        "--background".into(),
    ])
    .expect_err("duplicate background flag must fail");

    assert!(error.to_string().contains("cannot be used multiple times"));
}

#[test]
fn parse_init_uses_machine_state_without_a_workspace() {
    let command = parse(vec![
        "init".into(),
        "--state-dir".into(),
        "/tmp/mobius".into(),
        "--listen".into(),
        "127.0.0.1:9000".into(),
    ])
    .expect("parse init");

    assert!(matches!(
        command,
        Command::Init(InitOptions { state_dir, listen, tls, cloudflare })
            if state_dir == std::path::Path::new("/tmp/mobius")
                && listen == "127.0.0.1:9000".parse().expect("listen")
                && tls.is_none()
                && cloudflare.is_none()
    ));
}

#[cfg(unix)]
#[test]
fn parse_init_loads_a_private_cloudflare_token_without_debugging_it() {
    let token = tempfile::NamedTempFile::new().expect("token file");
    std::fs::write(token.path(), "secret-tunnel-token").expect("write token");
    token
        .as_file()
        .set_permissions(mobius::owner_only::file())
        .expect("secure token");
    let command = parse(vec![
        "init".into(),
        "--cloudflare-hostname".into(),
        "mobius.example.com".into(),
        "--cloudflare-token-file".into(),
        token.path().into(),
    ])
    .expect("parse Cloudflare init");

    assert!(!format!("{command:?}").contains("secret-tunnel-token"));
}

#[test]
fn init_rejects_the_removed_workspace_flag() {
    let error = parse(vec![
        "init".into(),
        "--workspace".into(),
        "/tmp/workspace".into(),
    ])
    .expect_err("workspace flag must be rejected");

    assert!(
        error
            .to_string()
            .contains("unexpected argument '--workspace'")
    );
}

#[test]
fn parse_rejects_the_removed_status_command() {
    let error = parse(vec!["status".into()]).expect_err("status must be removed");

    assert!(
        error
            .to_string()
            .contains("unrecognized subcommand 'status'")
    );
}

#[test]
fn parse_exit_accepts_an_explicit_state_directory() {
    let command = parse(vec![
        "exit".into(),
        "--state-dir".into(),
        "/tmp/mobius".into(),
    ])
    .expect("parse exit");

    assert!(matches!(
        command,
        Command::Exit { state_dir } if state_dir == std::path::Path::new("/tmp/mobius")
    ));
}

#[test]
fn process_record_rejects_an_invalid_pid() {
    let directory = tempfile::tempdir().expect("process record directory");
    let path = directory.path().join(PROCESS_FILE);
    std::fs::write(&path, r#"{"pid":0,"endpoint":null}"#).expect("write process record");

    let error = open_process_record(&path).expect_err("invalid PID must fail");

    assert!(error.to_string().contains("invalid gateway process record"));
}

#[test]
fn process_record_rejects_a_non_websocket_runtime_endpoint() {
    let directory = tempfile::tempdir().expect("process record directory");
    let path = directory.path().join(PROCESS_FILE);
    std::fs::write(&path, r#"{"pid":1,"endpoint":"tcp://127.0.0.1:8741"}"#)
        .expect("write process record");

    let error = open_process_record(&path).expect_err("plaintext endpoint must fail");

    assert!(error.to_string().contains("must use wss://"));
}

#[test]
fn gateway_versions_follow_semantic_precedence() {
    for (running, starting, older) in [
        ("0.9.9", "0.10.0", Some(true)),
        ("1.0.0", "1.0.0", Some(false)),
        ("2.0.0", "1.9.9", Some(false)),
        ("1.0.0-rc.1", "1.0.0", Some(true)),
        ("1.0.0-rc.2", "1.0.0-rc.10", Some(true)),
        ("1.0.0", "1.0.0-rc.1", Some(false)),
        ("1.0.0+old", "1.0.0+new", Some(false)),
        ("unknown", "1.0.0", None),
        ("1.0.0", "unknown", None),
    ] {
        assert_eq!(
            gateway_version_is_older(running, starting).ok(),
            older,
            "running {running}, starting {starting}"
        );
    }
}

#[cfg(unix)]
async fn report_gateway_version(
    listener: tokio::net::TcpListener,
    version: Option<&str>,
    protocol: u16,
) {
    use crate::wire::{
        ClientFrame, FrameReader, PROTOCOL_VERSION, ServerFrame, read_frame, write_frame,
    };

    for expected_protocol in [PROTOCOL_VERSION, protocol] {
        let (stream, _) = listener.accept().await.expect("accept version check");
        let (reader, mut writer) = tokio::io::split(stream);
        let frame = read_frame::<ClientFrame>(&mut FrameReader::new(reader))
            .await
            .expect("read authentication")
            .expect("authentication frame");
        assert_eq!(frame.version, expected_protocol);
        assert_eq!(
            frame.message,
            ClientMessage::Authenticate {
                token: "version-check-token".into(),
                client_kind: ClientKind::GatewayDashboard,
                catalog: Default::default(),
            }
        );
        if frame.version != protocol {
            write_frame(
                &mut writer,
                &ServerFrame {
                    version: protocol,
                    message: ServerMessage::Error {
                        code: "protocol_version".into(),
                        message: "unsupported protocol version".into(),
                        fatal: true,
                    },
                },
            )
            .await
            .expect("reject newer protocol");
            continue;
        }
        write_frame(
            &mut writer,
            &ServerFrame {
                version: protocol,
                message: ServerMessage::Authenticated,
            },
        )
        .await
        .expect("authenticate client");
        if let Some(version) = version {
            write_frame(
                &mut writer,
                &serde_json::json!({
                    "version": protocol,
                    "type": "ready",
                    "payload": { "gateway_version": version },
                }),
            )
            .await
            .expect("report gateway version");
        }
        return;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn gateway_reuse_evicts_only_a_confirmed_older_release() {
    use crate::wire::PROTOCOL_VERSION;

    for (version, protocol, expected_reuse) in [
        (Some("0.0.0"), PROTOCOL_VERSION, Some(false)),
        (Some("0.15.46"), 84, Some(false)),
        (
            Some(env!("CARGO_PKG_VERSION")),
            PROTOCOL_VERSION,
            Some(true),
        ),
        (Some("999.0.0"), PROTOCOL_VERSION, Some(true)),
        (Some("unknown"), PROTOCOL_VERSION, None),
        (None, PROTOCOL_VERSION, None),
    ] {
        let directory = tempfile::tempdir().expect("gateway state parent");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind version peer");
        let (store, config) = ConfigStore::initialize(
            directory.path().join("gateway"),
            listener.local_addr().expect("version peer address"),
            None,
        )
        .expect("gateway config");
        let path = store.state_dir().join(PROCESS_FILE);
        let mut file = File::create(&path).expect("process record");
        file.lock().expect("lock process record");
        let mut child = TokioCommand::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .expect("disposable gateway process");
        let pid = child.id().expect("disposable process ID");
        serde_json::to_writer(
            &mut file,
            &ProcessRecord {
                pid,
                endpoint: None,
            },
        )
        .expect("write disposable process record");
        file.flush().expect("flush process record");
        let (cleanup, cleanup_requested) = tokio::sync::oneshot::channel();
        let stopped = tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status.map(|_| ()),
                _ = cleanup_requested => child.kill().await,
            };
            drop(file);
            status
        });
        let peer = tokio::spawn(async move {
            tokio::time::timeout(
                Duration::from_secs(5),
                report_gateway_version(listener, version, protocol),
            )
            .await
            .expect("version peer completes");
        });

        let result = reuse_current_gateway(&store, &config, None, |_| {
            Ok(Some("version-check-token".into()))
        })
        .await;
        let running = running_process_pid(&path);
        let _ = cleanup.send(());
        stopped.await.expect("process task").expect("reap process");
        peer.await.expect("version peer");

        assert_eq!(
            result.map(|process| process.is_some()).ok(),
            expected_reuse,
            "reported version {version:?}, protocol {protocol}"
        );
        assert_eq!(
            running.expect("check process lifetime"),
            (expected_reuse != Some(false)).then_some(pid),
            "reported version {version:?}, protocol {protocol}"
        );
    }
}

#[cfg(unix)]
#[test]
fn process_record_carries_the_quick_tunnel_endpoint() {
    let directory = tempfile::tempdir().expect("process record directory");
    let endpoint: Endpoint = "wss://bright-river.trycloudflare.com"
        .parse()
        .expect("endpoint");
    let guard =
        ProcessRecordGuard::create(directory.path(), Some(&endpoint)).expect("process record");
    let (record, _) = open_process_record(&guard.path)
        .expect("read process record")
        .expect("process record");

    assert_eq!(record.endpoint().expect("valid endpoint"), Some(endpoint));
}

#[cfg(unix)]
#[test]
fn running_connect_controls_quick_tunnel_over_loopback() {
    let directory = tempfile::tempdir().expect("gateway state parent");
    let state = directory.path().join("gateway");
    let (store, config) =
        ConfigStore::initialize_quick_cloudflare(state, DEFAULT_LISTEN).expect("gateway config");
    let public_endpoint: Endpoint = "wss://bright-river.trycloudflare.com"
        .parse()
        .expect("public endpoint");
    let _process = ProcessRecordGuard::create(store.state_dir(), Some(&public_endpoint))
        .expect("running process");

    let (client_endpoint, pairing_endpoint) = running_connection_endpoints(&store, &config, None)
        .expect("running connect")
        .expect("running gateway");

    assert_eq!(client_endpoint.to_string(), "tcp://127.0.0.1:8741");
    assert_eq!(pairing_endpoint, public_endpoint);
}

#[cfg(unix)]
#[tokio::test]
async fn running_gateway_issues_a_code_for_another_client() {
    let directory = tempfile::tempdir().expect("gateway state");
    let (server, grant) = GatewayServer::bootstrap(
        directory.path().join("gateway"),
        "127.0.0.1:0".parse().expect("listen address"),
    )
    .await
    .expect("bootstrap gateway");
    let endpoint: Endpoint = format!("tcp://{}", server.listen_addr())
        .parse()
        .expect("gateway endpoint");
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        let _ = signal.await;
    }));
    let (_first, identity) = GatewayClient::pair(&endpoint, grant.code, "first", ClientKind::Cli)
        .await
        .expect("pair first client");

    let grant = request_running_pairing_code(&endpoint, &identity.token)
        .await
        .expect("request another code");
    assert!(grant.expires_at > 0);
    let (_second, _) = GatewayClient::pair(&endpoint, grant.code, "second", ClientKind::Ios)
        .await
        .expect("pair second client");

    assert!(!serving.is_finished());
    shutdown.send(()).expect("stop gateway");
    serving.await.expect("gateway task").expect("stop gateway");
}

#[cfg(unix)]
#[test]
fn startup_cleanup_removes_only_an_unlocked_process_record() {
    let directory = tempfile::tempdir().expect("process record directory");
    let path = directory.path().join(PROCESS_FILE);
    std::fs::write(&path, b"{").expect("partial process record");

    remove_unlocked_process_record(&path);

    assert!(!path.exists());
    let guard = ProcessRecordGuard::create(directory.path(), None).expect("locked process record");
    remove_unlocked_process_record(&path);
    assert!(path.exists());
    drop(guard);
}

#[cfg(unix)]
#[test]
fn process_record_lock_tracks_the_gateway_lifetime() {
    let directory = tempfile::tempdir().expect("process record directory");
    let guard = ProcessRecordGuard::create(directory.path(), None).expect("process record");
    let (_, file) = open_process_record(&guard.path)
        .expect("read process record")
        .expect("process record");

    assert!(process_is_running(&file).expect("locked process record"));
    assert_eq!(
        running_process_pid(&guard.path).expect("running process ID"),
        Some(std::process::id())
    );
    drop(guard);
    assert!(!directory.path().join(PROCESS_FILE).exists());
}

#[cfg(unix)]
#[test]
fn startup_lock_allows_only_one_lifecycle_operation() {
    let directory = tempfile::tempdir().expect("startup directory");
    let guard = StartupGuard::create(directory.path()).expect("startup lock");

    let error = StartupGuard::create(directory.path()).expect_err("competing startup");

    assert!(error.to_string().contains("already in progress"));
    drop(guard);
    StartupGuard::create(directory.path()).expect("released startup lock");
}

#[cfg(unix)]
#[tokio::test]
async fn autostart_process_group_cleanup_kills_descendants() {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let directory = tempfile::tempdir().expect("process group directory");
    let mut command = TokioCommand::new("sh");
    command
        .arg("-c")
        .arg("sleep 30 & echo $!; wait")
        .stdout(std::process::Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().expect("process group child");
    let mut output = BufReader::new(child.stdout.take().expect("child stdout"));
    let mut pid = String::new();
    let ready = tokio::time::timeout(Duration::from_secs(5), output.read_line(&mut pid)).await;
    stop_background_child(&mut child, &directory.path().join(PROCESS_FILE)).await;
    ready.expect("descendant ready").expect("descendant PID");
    let descendant = pid.trim().parse::<u32>().expect("valid descendant PID");

    for _ in 0..20 {
        let alive = std::process::Command::new("kill")
            .args(["-0", &descendant.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .expect("probe descendant")
            .success();
        if !alive {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("process-group descendant remained alive");
}

#[cfg(unix)]
#[test]
fn process_group_termination_ignores_reserved_pids() {
    terminate_process_group(0);
    terminate_process_group(1);
}

#[test]
fn parse_init_requires_both_tls_paths() {
    let error = parse(vec![
        "init".into(),
        "--tls-cert".into(),
        "/tmp/certificate.pem".into(),
    ])
    .expect_err("partial TLS config must fail");

    assert!(error.to_string().contains("--tls-key <PATH>"));
}

#[cfg(unix)]
#[tokio::test]
async fn set_runtime_preserves_omitted_policy_fields() {
    let root = tempfile::tempdir().unwrap();
    let state_dir = root.path().join("state");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = listener.local_addr().unwrap();
    drop(listener);
    let (store, mut config) = ConfigStore::initialize(state_dir.clone(), listen, None).unwrap();
    config.runtime.ingress = Some("0.0.0.0:8742".parse().unwrap());
    config.runtime.require_access_lease = true;
    config.runtime.access_grace_seconds = 30;
    config.runtime.storage_limit_bytes = Some(5368709120);
    store.save(&config).unwrap();
    run(
        vec![
            "--state-dir".into(),
            state_dir.into_os_string(),
            "set-runtime".into(),
            "--idle-exit-seconds".into(),
            "300".into(),
        ],
        |_, _| Ok(()),
        |_| Ok(None),
    )
    .await
    .unwrap();
    let (_, updated) = ConfigStore::open(store.state_dir().to_path_buf()).unwrap();
    assert_eq!(updated.runtime.idle_exit_seconds, 300);
    assert_eq!(updated.runtime.ingress, config.runtime.ingress);
    assert!(updated.runtime.require_access_lease);
    assert_eq!(updated.runtime.access_grace_seconds, 30);
    assert_eq!(
        updated.runtime.storage_limit_bytes,
        config.runtime.storage_limit_bytes
    );
    run(
        vec![
            "--state-dir".into(),
            store.state_dir().as_os_str().to_owned(),
            "set-runtime".into(),
            "--clear-ingress".into(),
            "--clear-storage-limit".into(),
        ],
        |_, _| Ok(()),
        |_| Ok(None),
    )
    .await
    .unwrap();
    let (_, cleared) = ConfigStore::open(store.state_dir().to_path_buf()).unwrap();
    assert_eq!(cleared.runtime.idle_exit_seconds, 300);
    assert_eq!(cleared.runtime.ingress, None);
    assert!(cleared.runtime.require_access_lease);
    assert_eq!(cleared.runtime.access_grace_seconds, 30);
    assert_eq!(cleared.runtime.storage_limit_bytes, None);
}

#[test]
fn set_runtime_rejects_setting_and_clearing_the_same_field() {
    for (clear, set, value) in [
        ("--clear-ingress", "--ingress", "0.0.0.0:8742"),
        ("--clear-storage-limit", "--storage-limit-bytes", "1024"),
    ] {
        assert!(
            parse(vec![
                "set-runtime".into(),
                clear.into(),
                set.into(),
                value.into()
            ])
            .is_err()
        );
    }
}
