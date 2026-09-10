use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use mobius::protocol::TokenUsage;
use tokio::sync::{broadcast, mpsc};

use crate::bots::BotStore;
use crate::config::{ConfigStore, CredentialStore};
use crate::host::session::{
    HostCommand, HostInner, ProviderCutoverStatus, ProviderRefresh, provider_refresh_matches,
};
use crate::host::tests::create_test_session;

use super::*;

async fn provider_removal_gateway(
    root: &tempfile::TempDir,
) -> (
    GatewayHost,
    Arc<CredentialStore>,
    ProviderConfig,
    ProviderConfig,
) {
    let state_dir = root.path().join("state");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) = ConfigStore::initialize(state_dir, listen, None).expect("config");
    let primary = ProviderConfig {
        instance: "openrouter".into(),
        provider: "openrouter".into(),
        model: "openai/gpt-5".into(),
        base_url: Some("https://connector.example/v1".into()),
        endpoint_auth: ProviderEndpointAuth::Credentialless,
        reasoning_effort: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let removable = ProviderConfig {
        instance: "kimi-unused".into(),
        provider: "kimi".into(),
        model: "kimi-k3".into(),
        base_url: Some("https://api.moonshot.ai/v1".into()),
        endpoint_auth: ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: Some("max".into()),
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let config = config
        .registering_provider(
            primary.clone(),
            "Primary".into(),
            Default::default(),
            vec![primary.model.clone()],
            Vec::new(),
        )
        .and_then(|config| {
            config.registering_provider(
                removable.clone(),
                "Unused".into(),
                Default::default(),
                Vec::new(),
                Vec::new(),
            )
        })
        .expect("provider catalog");
    store.save(&config).expect("save provider catalog");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    credentials
        .set(
            &removable.instance,
            &removable.provider,
            "unused-secret",
            removable.base_url.as_deref(),
            None,
        )
        .expect("removable credential");
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, Arc::clone(&credentials), bots)
        .await
        .expect("gateway");
    (gateway, credentials, primary, removable)
}

#[tokio::test]
async fn provider_removal_rejects_bot_defaults_without_changes() {
    let root = tempfile::tempdir().expect("root");
    let (gateway, credentials, primary, secondary) = provider_removal_gateway(&root).await;
    let config_path = root.path().join("state").join("gateway.toml");
    let before_file = std::fs::read(&config_path).expect("gateway config");
    let before = gateway
        .state
        .lock()
        .await
        .config
        .lock()
        .expect("gateway config")
        .clone();

    let rejection = gateway
        .remove_provider(primary.instance)
        .await
        .expect_err("Bot defaults must keep their provider");

    assert_eq!(rejection.code, "invalid_config");
    assert_eq!(
        *gateway
            .state
            .lock()
            .await
            .config
            .lock()
            .expect("gateway config"),
        before
    );
    assert_eq!(
        std::fs::read(config_path).expect("unchanged gateway config"),
        before_file
    );
    assert_eq!(
        credentials
            .get(
                &secondary.instance,
                &secondary.provider,
                secondary.base_url.as_deref(),
            )
            .expect("secondary credential")
            .map(|credential| credential.api_key),
        Some("unused-secret".into())
    );
}

#[tokio::test]
async fn provider_removal_reloads_idle_bot_chat_and_deletes_credential() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let (gateway, credentials, primary, removable) = provider_removal_gateway(&root).await;
    let host = create_test_session(&gateway, &workspace)
        .await
        .expect("chat");
    let session_id = host.session_id().to_owned();
    let mut events = host.subscribe();
    let mut gateway_events = gateway.subscribe();

    let ready = gateway
        .remove_provider(removable.instance.clone())
        .await
        .expect("remove unused provider");

    assert!(host.is_alive());
    assert!(Arc::ptr_eq(
        &gateway.state.lock().await.sessions[&session_id].inner,
        &host.inner
    ));
    assert_eq!(
        ready
            .bots
            .iter()
            .find(|bot| bot.id == host.bot_id())
            .expect("chat Bot")
            .config
            .config
            .provider,
        primary
    );
    assert!(
        ready
            .provider_instances
            .iter()
            .all(|provider| provider.selection.instance != removable.instance)
    );
    assert_eq!(
        credentials
            .get(
                &removable.instance,
                &removable.provider,
                removable.base_url.as_deref(),
            )
            .expect("credential")
            .map(|credential| credential.api_key),
        None
    );
    assert!(matches!(
        events.recv().await.expect("reload event").message,
        ServerMessage::SessionChanged { .. }
    ));
    assert!(matches!(
        gateway_events.recv().await.expect("ready event").message,
        ServerMessage::Ready { payload } if payload == ready
    ));
}

#[tokio::test]
async fn busy_bot_chat_blocks_provider_removal_without_mutation() {
    let root = tempfile::tempdir().expect("root");
    let (gateway, credentials, _, removable) = provider_removal_gateway(&root).await;
    let before = gateway
        .state
        .lock()
        .await
        .config
        .lock()
        .expect("gateway config")
        .clone();
    let (commands, mut receiver) = mpsc::channel(1);
    tokio::spawn(async move {
        if let Some(HostCommand::ProviderCutoverStatus { reply }) = receiver.recv().await {
            let _ = reply.send(ProviderCutoverStatus { idle: false });
        }
    });
    let (events, _) = broadcast::channel(1);
    gateway.state.lock().await.sessions.insert(
        "busy".into(),
        super::super::HostHandle {
            inner: Arc::new(HostInner {
                session_id: Arc::from("busy"),
                bot_id: Arc::from("busy-bot"),
                commands,
                events,
                alive: Arc::new(AtomicBool::new(true)),
                terminated: Arc::new(AtomicBool::new(true)),
                termination: Arc::new(tokio::sync::Notify::new()),
                session_mutations: Arc::new(tokio::sync::RwLock::new(())),
                realtime_voice: Arc::new(tokio::sync::Mutex::new(())),
            }),
        },
    );

    let error = gateway
        .remove_provider(removable.instance.clone())
        .await
        .expect_err("busy chat must block provider removal");

    assert_eq!(error.code, "agent_busy");
    assert_eq!(
        *gateway
            .state
            .lock()
            .await
            .config
            .lock()
            .expect("gateway config"),
        before
    );
    assert_eq!(
        credentials
            .get(
                &removable.instance,
                &removable.provider,
                removable.base_url.as_deref(),
            )
            .expect("credential")
            .map(|credential| credential.api_key),
        Some("unused-secret".into())
    );
}

#[tokio::test]
async fn provider_removal_save_failure_keeps_idle_resident_and_config_usable() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let (gateway, credentials, _, removable) = provider_removal_gateway(&root).await;
    let host = create_test_session(&gateway, &workspace)
        .await
        .expect("chat");
    let session_id = host.session_id().to_owned();
    let config_path = root.path().join("state").join("gateway.toml");
    std::fs::remove_file(&config_path).expect("remove gateway config");
    std::fs::create_dir(&config_path).expect("block gateway config save");

    gateway
        .remove_provider(removable.instance.clone())
        .await
        .expect_err("gateway config save must fail");

    assert!(host.is_alive());
    assert!(Arc::ptr_eq(
        &gateway.state.lock().await.sessions[&session_id].inner,
        &host.inner
    ));
    assert!(
        gateway
            .state
            .lock()
            .await
            .config
            .lock()
            .expect("gateway config")
            .configured_providers
            .contains_key(&removable.instance)
    );
    assert_eq!(
        credentials
            .get(
                &removable.instance,
                &removable.provider,
                removable.base_url.as_deref(),
            )
            .expect("credential")
            .map(|credential| credential.api_key),
        Some("unused-secret".into())
    );
}

#[tokio::test]
async fn provider_registration_commits_against_latest_usage() {
    let root = tempfile::tempdir().expect("root");
    let state_dir = root.path().join("state");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) = ConfigStore::initialize(state_dir.clone(), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
    let selection = ProviderConfig {
        instance: "openrouter".into(),
        provider: "openrouter".into(),
        model: "openai/gpt-5".into(),
        base_url: Some("https://connector.example/v1".into()),
        endpoint_auth: ProviderEndpointAuth::Credentialless,
        reasoning_effort: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let usage = TokenUsage {
        input_tokens: 13,
        total_tokens: 13,
        ..TokenUsage::default()
    };
    let state = gateway.state.lock().await;
    {
        let mut latest = state.config.lock().expect("gateway config");
        assert!(
            latest
                .observe_usage("openrouter", &usage)
                .expect("observe usage")
        );
        state.store.save(&latest).expect("persist usage");
    }

    commit_provider_registration(
        &state,
        &selection,
        "Test",
        &ProviderTint::default(),
        std::slice::from_ref(&selection.model),
        &[],
    )
    .expect("commit registration");

    assert_eq!(
        state
            .config
            .lock()
            .expect("gateway config")
            .profile()
            .daily_usage[0]
            .usage,
        usage
    );
    drop(state);
    assert_eq!(
        ConfigStore::open(state_dir)
            .expect("persisted gateway")
            .1
            .profile()
            .daily_usage[0]
            .usage,
        usage
    );
}

#[tokio::test]
async fn credential_endpoints_are_validated_and_persisted() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    std::fs::create_dir(&workspace).expect("workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) = ConfigStore::initialize(state, listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credential store"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, Arc::clone(&credentials), bots)
        .await
        .expect("gateway");
    create_test_session(&gateway, &workspace)
        .await
        .expect("chat");
    let custom_endpoint = "https://example.com/v1";

    gateway
        .set_credential(
            "responses".into(),
            "responses".into(),
            "custom-secret".into(),
            Some(custom_endpoint.into()),
            None,
        )
        .await
        .expect("store custom credential");
    let error = gateway
        .set_credential(
            "openai_socket".into(),
            "openai_socket".into(),
            "fixed-secret".into(),
            Some(custom_endpoint.into()),
            None,
        )
        .await
        .expect_err("fixed provider endpoint must be rejected");

    assert_eq!(
        credentials
            .get("responses", "responses", Some(custom_endpoint))
            .expect("custom credential")
            .map(|credential| credential.api_key),
        Some("custom-secret".into())
    );
    assert_eq!(
        credentials
            .get("responses", "openrouter", Some(custom_endpoint))
            .expect("different provider")
            .map(|credential| credential.api_key),
        None
    );
    assert_eq!(error.code, "invalid_config");
    assert!(error.message.contains("fixed API endpoint"));
    assert_eq!(
        credentials
            .get("openai_socket", "openai_socket", None)
            .expect("fixed credential")
            .map(|credential| credential.api_key),
        None
    );
}

#[tokio::test]
async fn explicit_key_replaces_credentialless_endpoint_auth() {
    let root = tempfile::tempdir().expect("root");
    let state = root.path().join("state");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) = ConfigStore::initialize(state.clone(), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credential store"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, Arc::clone(&credentials), bots)
        .await
        .expect("gateway");
    let base_url = "https://connector.example/v1";
    let model = "openai/gpt-5.6-luna";

    gateway
        .register_provider(
            ProviderConfig {
                instance: "openrouter-managed".into(),
                provider: "openrouter".into(),
                model: model.into(),
                base_url: Some(base_url.into()),
                endpoint_auth: ProviderEndpointAuth::Credentialless,
                reasoning_effort: None,
                web_search: mobius::backend::model::provider::HostedWebSearch::Off,
            },
            "Managed".into(),
            Default::default(),
            vec![model.into()],
            Vec::new(),
        )
        .await
        .expect("register credentialless provider");
    gateway
        .set_credential(
            "openrouter-managed".into(),
            "openrouter".into(),
            "user-secret".into(),
            Some(base_url.into()),
            None,
        )
        .await
        .expect("store explicit key");

    let (_, persisted) = ConfigStore::open(state).expect("persisted config");
    assert_eq!(
        persisted
            .configured_providers
            .get("openrouter-managed")
            .expect("configured provider")
            .selection
            .endpoint_auth,
        ProviderEndpointAuth::ProviderDefault
    );
    assert_eq!(
        credentials
            .get("openrouter-managed", "openrouter", Some(base_url))
            .expect("credential")
            .map(|credential| credential.api_key),
        Some("user-secret".into())
    );
}

#[tokio::test]
async fn credential_update_refreshes_every_matching_resident_chat() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    std::fs::create_dir(&workspace).expect("workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) = ConfigStore::initialize(state, listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credential store"));
    credentials
        .set(
            "kimi",
            "kimi",
            "old-secret",
            Some("https://api.moonshot.ai/v1"),
            None,
        )
        .expect("initial Kimi credential");
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
    gateway
        .register_provider(
            ProviderConfig {
                instance: "kimi".into(),
                provider: "kimi".into(),
                model: "kimi-k3".into(),
                base_url: Some("https://api.moonshot.ai/v1".into()),
                endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: Some("max".into()),
                web_search: mobius::backend::model::provider::HostedWebSearch::Off,
            },
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .await
        .expect("register Kimi");
    let first = create_test_session(&gateway, &workspace)
        .await
        .expect("first chat");
    let second = create_test_session(&gateway, &workspace)
        .await
        .expect("second chat");
    let mut first_events = first.subscribe();
    let mut second_events = second.subscribe();

    gateway
        .set_credential(
            "kimi".into(),
            "kimi".into(),
            "new-secret".into(),
            Some("https://api.moonshot.ai/v1".into()),
            None,
        )
        .await
        .expect("replace Kimi credential");

    for events in [&mut first_events, &mut second_events] {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    events.recv().await.expect("chat event").message,
                    ServerMessage::SessionChanged { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .expect("matching chat refresh");
    }

    assert!(first.stop_if_idle().await);
    gateway
        .set_credential(
            "kimi".into(),
            "kimi".into(),
            "latest-secret".into(),
            Some("https://api.moonshot.ai/v1".into()),
            None,
        )
        .await
        .expect("replace Kimi credential with stopped cached chat");
}

#[test]
fn credential_refresh_separates_instances_but_shares_a_browser_login() {
    let selection = ProviderConfig {
        instance: "responses-work".into(),
        provider: "responses".into(),
        model: "custom-model".into(),
        base_url: Some("https://first.example/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };

    // An API key belongs to one instance; a sibling instance keeps its own.
    assert!(
        provider_refresh_matches(
            &selection,
            &ProviderRefresh::Instance {
                instance: "responses-work".into(),
                base_url: Some("https://first.example/v1".into()),
            }
        )
        .expect("matching instance and endpoint")
    );
    assert!(
        !provider_refresh_matches(
            &selection,
            &ProviderRefresh::Instance {
                instance: "responses-personal".into(),
                base_url: Some("https://first.example/v1".into()),
            }
        )
        .expect("different instance")
    );
    assert!(
        !provider_refresh_matches(
            &selection,
            &ProviderRefresh::Instance {
                instance: "responses-work".into(),
                base_url: Some("https://second.example/v1".into()),
            }
        )
        .expect("different endpoint")
    );

    // A browser login is stored per provider, so every instance of it refreshes.
    assert!(
        provider_refresh_matches(&selection, &ProviderRefresh::Provider("responses".into()))
            .expect("matching provider")
    );
    assert!(
        !provider_refresh_matches(&selection, &ProviderRefresh::Provider("anthropic".into()))
            .expect("different provider")
    );
}

fn provider_login_started() -> ServerMessage {
    ServerMessage::ProviderLoginStarted {
        request_id: "request-a".into(),
        login_id: "login-a".into(),
        provider: "openai_codex".into(),
        verification_url: "https://example.com/device".into(),
        user_code: "ABCD-1234".into(),
    }
}

async fn reserve_test_login(
    gateway: &GatewayHost,
    suffix: &str,
) -> std::result::Result<bool, Rejection> {
    let state = gateway.state.lock().await;
    let mut logins = state.provider_login.lock().expect("login state");
    reserve_provider_login(
        &mut logins,
        &format!("login-{suffix}"),
        &format!("client-{suffix}"),
        &format!("request-{suffix}"),
        "openai_codex",
    )
}

#[tokio::test]
async fn provider_login_retry_waits_for_code_and_reconnect_replays_pending_code() {
    let root = tempfile::tempdir().expect("root");
    let (gateway, _, _, _) = provider_removal_gateway(&root).await;
    assert!(
        reserve_test_login(&gateway, "a")
            .await
            .expect("reserve login")
    );
    let mut events = gateway.subscribe();
    gateway
        .start_provider_login("request-a".into(), "openai_codex".into(), "client-a")
        .await
        .expect("retry during code acquisition");
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    assert!(
        !reserve_test_login(&gateway, "a")
            .await
            .expect("same polling slot")
    );
    gateway
        .publish_provider_login("login-a", provider_login_started())
        .await
        .expect("code");
    assert_eq!(
        events.recv().await.expect("code event").message,
        provider_login_started()
    );
    drop(events);

    let mut events = gateway.subscribe();
    assert_eq!(
        gateway
            .start_provider_login("request-a".into(), "openai_codex".into(), "client-a")
            .await
            .expect("retry after reconnect"),
        Some(provider_login_started())
    );
    assert!(
        matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ),
        "a replay is returned only to the requesting connection"
    );
    gateway
        .finish_provider_login(
            "request-a".into(),
            "login-a".into(),
            "openai_codex".into(),
            Ok(()),
        )
        .await;
    assert!(matches!(events.recv().await.expect("completion").message,
        ServerMessage::ProviderLoginFinished { request_id, login_id, .. }
            if request_id == "request-a" && login_id == "login-a"));
}

#[tokio::test]
async fn provider_login_retry_recovers_success_and_failure_after_another_client_starts() {
    for result in [Ok(()), Err(String::from("device login timed out"))] {
        let root = tempfile::tempdir().expect("root");
        let (gateway, _, _, _) = provider_removal_gateway(&root).await;
        reserve_test_login(&gateway, "a")
            .await
            .expect("reserve login");
        gateway
            .publish_provider_login("login-a", provider_login_started())
            .await
            .expect("code");
        let expected = match &result {
            Ok(()) => ServerMessage::ProviderLoginFinished {
                request_id: "request-a".into(),
                login_id: "login-a".into(),
                provider: "openai_codex".into(),
            },
            Err(message) => ServerMessage::Rejected {
                request_id: "request-a".into(),
                code: "provider_login_failed".into(),
                message: message.clone(),
                fatal: false,
            },
        };
        gateway
            .finish_provider_login(
                "request-a".into(),
                "login-a".into(),
                "openai_codex".into(),
                result,
            )
            .await;
        reserve_test_login(&gateway, "b")
            .await
            .expect("another client's login");
        let mut events = gateway.subscribe();
        assert!(
            matches!(
                events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "authentication alone must not replay old login errors"
        );
        assert_eq!(
            gateway
                .start_provider_login("request-a".into(), "openai_codex".into(), "client-a")
                .await
                .expect("retry after reconnect"),
            Some(expected)
        );
        assert!(
            matches!(
                events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "terminal replay must not be followed by the older code"
        );
        assert_eq!(
            gateway
                .start_provider_login("request-a".into(), "openai_codex".into(), "client-c")
                .await
                .expect_err("another client cannot recover this request")
                .code,
            "provider_login_in_progress"
        );
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }
}

#[tokio::test]
async fn active_provider_login_reserves_the_only_polling_slot_and_ignores_stale_completions() {
    let root = tempfile::tempdir().expect("root");
    let (gateway, _, _, _) = provider_removal_gateway(&root).await;
    reserve_test_login(&gateway, "a")
        .await
        .expect("reserve first login");
    assert_eq!(
        reserve_test_login(&gateway, "b")
            .await
            .expect_err("only one login")
            .code,
        "provider_login_in_progress"
    );
    gateway
        .finish_provider_login(
            "request-other".into(),
            "another-login".into(),
            "openai_codex".into(),
            Err("stale".into()),
        )
        .await;
    assert!(reserve_test_login(&gateway, "b").await.is_err());
    // Failures acquiring a code also release the slot and remain replayable.
    let failure = ServerMessage::Rejected {
        request_id: "request-a".into(),
        code: "internal".into(),
        message: "code request failed".into(),
        fatal: false,
    };
    gateway
        .publish_provider_login("login-a", failure.clone())
        .await
        .expect("start failure");
    reserve_test_login(&gateway, "b").await.expect("next login");
    gateway
        .finish_provider_login(
            "request-a".into(),
            "login-a".into(),
            "openai_codex".into(),
            Err("late completion".into()),
        )
        .await;
    assert!(reserve_test_login(&gateway, "c").await.is_err());
    assert_eq!(
        gateway
            .start_provider_login("request-a".into(), "openai_codex".into(), "client-a")
            .await
            .expect("retry failed start"),
        Some(failure)
    );
}

#[tokio::test]
async fn unpaired_client_login_completion_does_not_restore_its_replay() {
    let root = tempfile::tempdir().expect("root");
    let (gateway, _, _, _) = provider_removal_gateway(&root).await;
    // Unpair can race an already-selected request before its reservation. Dispatch
    // also forgets after reservation when its paired-client post-check fails.
    gateway
        .forget_provider_login("client-a")
        .await
        .expect("early unpair");
    reserve_test_login(&gateway, "a")
        .await
        .expect("reserve login");
    gateway
        .publish_provider_login("login-a", provider_login_started())
        .await
        .expect("code");
    gateway
        .forget_provider_login("client-a")
        .await
        .expect("unpair");
    assert!(reserve_test_login(&gateway, "b").await.is_err());
    gateway
        .finish_provider_login(
            "request-a".into(),
            "login-a".into(),
            "openai_codex".into(),
            Ok(()),
        )
        .await;
    assert!(
        gateway
            .state
            .lock()
            .await
            .provider_login
            .lock()
            .expect("login state")
            .attempts
            .is_empty()
    );
    reserve_test_login(&gateway, "b")
        .await
        .expect("slot released");
}

#[test]
fn provider_login_reservation_rejects_changed_identity_and_starts_unknown_requests() {
    let mut logins = ProviderLogins::default();
    for request_id in [String::new(), "x".repeat(MAX_LOGIN_REQUEST_ID_BYTES + 1)] {
        assert_eq!(
            reserve_provider_login(
                &mut logins,
                "unused",
                "client-a",
                &request_id,
                "openai_codex"
            )
            .expect_err("retained request identity must be bounded")
            .code,
            "invalid_provider_login"
        );
        assert!(logins.active_id.is_none());
        assert!(logins.attempts.is_empty());
    }
    assert!(
        reserve_provider_login(
            &mut logins,
            "login-a",
            "client-a",
            "request-a",
            "openai_codex"
        )
        .expect("unknown request")
    );
    assert!(
        !reserve_provider_login(
            &mut logins,
            "unused",
            "client-a",
            "request-a",
            "openai_codex"
        )
        .expect("idempotent retry")
    );
    assert_eq!(logins.active_id.as_deref(), Some("login-a"));
    assert_eq!(
        reserve_provider_login(
            &mut logins,
            "unused",
            "client-a",
            "request-a",
            "other-provider"
        )
        .expect_err("request identity changed")
        .code,
        "invalid_provider_login"
    );
    assert!(
        reserve_provider_login(
            &mut logins,
            "login-b",
            "client-b",
            "request-a",
            "openai_codex"
        )
        .is_err()
    );
    // A restarted gateway has no pending upstream task; retrying starts a fresh code.
    let mut restarted = ProviderLogins::default();
    assert!(
        reserve_provider_login(
            &mut restarted,
            "login-new",
            "client-a",
            "request-a",
            "openai_codex"
        )
        .expect("restart recovery")
    );
}

#[tokio::test]
async fn provider_login_code_acquisition_survives_its_request_task_cancellation() {
    let root = tempfile::tempdir().expect("root");
    let (gateway, _, _, _) = provider_removal_gateway(&root).await;
    reserve_test_login(&gateway, "a")
        .await
        .expect("reserve login");
    let mut events = gateway.subscribe();
    let (started, waiting) = tokio::sync::oneshot::channel();
    let (resume, resumed) = tokio::sync::oneshot::channel();
    let request_gateway = gateway.clone();
    let request = tokio::spawn(async move {
        request_gateway.spawn_provider_login(
            "request-a".into(),
            "login-a".into(),
            "openai_codex".into(),
            Box::pin(async move {
                started.send(()).expect("code acquisition started");
                resumed.await.expect("resume acquisition");
                Err(mobius::Error::Auth("code request failed".into()))
            }),
        );
        std::future::pending::<()>().await;
    });
    waiting.await.expect("acquiring code");
    request.abort();
    assert!(
        request
            .await
            .expect_err("request task cancelled")
            .is_cancelled()
    );
    resume.send(()).expect("gateway task retained acquisition");
    let failure = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .expect("detached acquisition completion")
        .expect("failure event")
        .message;
    assert!(
        matches!(&failure, ServerMessage::Rejected { request_id, code, .. }
        if request_id == "request-a" && code == "provider_login_failed")
    );
    assert_eq!(
        gateway
            .start_provider_login("request-a".into(), "openai_codex".into(), "client-a")
            .await
            .expect("recover detached failure"),
        Some(failure)
    );
    reserve_test_login(&gateway, "b")
        .await
        .expect("failed acquisition released the slot");
}

#[tokio::test]
async fn stale_provider_login_success_does_not_refresh_sessions_or_release_another_login() {
    let root = tempfile::tempdir().expect("root");
    let (gateway, _, _, _) = provider_removal_gateway(&root).await;
    reserve_test_login(&gateway, "b")
        .await
        .expect("current login");
    let (commands, mut receiver) = mpsc::channel(1);
    let (events, _) = broadcast::channel(1);
    gateway.state.lock().await.sessions.insert(
        "observer".into(),
        super::super::HostHandle {
            inner: Arc::new(HostInner {
                session_id: Arc::from("observer"),
                bot_id: Arc::from("observer-bot"),
                commands,
                events,
                alive: Arc::new(AtomicBool::new(true)),
                terminated: Arc::new(AtomicBool::new(true)),
                termination: Arc::new(tokio::sync::Notify::new()),
                session_mutations: Arc::new(tokio::sync::RwLock::new(())),
                realtime_voice: Arc::new(tokio::sync::Mutex::new(())),
            }),
        },
    );
    tokio::time::timeout(
        Duration::from_secs(1),
        gateway.finish_provider_login(
            "request-a".into(),
            "login-a".into(),
            "openai_codex".into(),
            Ok(()),
        ),
    )
    .await
    .expect("stale completion must not wait for a session refresh");
    assert!(matches!(
        receiver.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(reserve_test_login(&gateway, "c").await.is_err());
}
