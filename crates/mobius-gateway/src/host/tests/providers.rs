use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use mobius::protocol::FrontendSettingValue;
use tokio::sync::{broadcast, mpsc};

use crate::config::{ConfigStore, CredentialStore};
use crate::cron::CronStore;

use super::super::HostHandle;
use super::super::session::{
    HostCommand, HostInner, ProviderCutoverStatus, ProviderRefresh, provider_refresh_matches,
};
use super::*;

fn provider_removal_gateway(
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
        endpoint_auth: crate::wire::ProviderEndpointAuth::Credentialless,
        reasoning_effort: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let removable = ProviderConfig {
        instance: "kimi-unused".into(),
        provider: "kimi".into(),
        model: "kimi-k3".into(),
        base_url: Some("https://api.moonshot.ai/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: Some("max".into()),
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let route = crate::config::model_route_id(
        &removable.instance,
        &removable.model,
        removable.reasoning_effort.as_deref(),
    );
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
    let mut default = config
        .default_agent
        .as_ref()
        .expect("default")
        .config
        .clone();
    default.middleware.set_setting(
        "subagents",
        "model_route",
        Some(FrontendSettingValue::String(route.clone())),
    );
    let config = config
        .replacing_default_agent(1, default)
        .expect("middleware route default");
    store.save(&config).expect("save provider catalog");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    credentials
        .set(
            &removable.instance,
            &removable.provider,
            "unused-secret",
            removable.base_url.as_deref(),
        )
        .expect("removable credential");
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway =
        GatewayHost::start(store, config, Arc::clone(&credentials), cron).expect("gateway");
    (gateway, credentials, primary, removable)
}

#[tokio::test]
async fn provider_removal_rejects_the_primary_default_without_changes() {
    let root = tempfile::tempdir().expect("root");
    let (gateway, credentials, primary, removable) = provider_removal_gateway(&root);
    let before = gateway
        .state
        .lock()
        .await
        .config
        .lock()
        .expect("gateway config")
        .clone();

    let error = gateway
        .remove_provider(primary.instance)
        .await
        .expect_err("the primary default must remain configured");

    assert_eq!(error.code, "invalid_config");
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
            .expect("credential"),
        Some("unused-secret".into())
    );
}

#[tokio::test]
async fn provider_removal_reloads_idle_chat_and_deletes_credential() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let (gateway, credentials, primary, removable) = provider_removal_gateway(&root);
    let host = gateway.create_session(&workspace).await.expect("chat");
    let before = host.snapshot(None).await.expect("snapshot").ready.config;
    let mut composition = before.config;
    composition.provider = removable.clone();
    host.configure(before.revision, composition)
        .await
        .expect("select removable provider");
    let mut events = host.subscribe();
    let mut gateway_events = gateway.subscribe();
    let host_id = host.session_id().to_owned();

    let ready = gateway
        .remove_provider(removable.instance.clone())
        .await
        .expect("remove unused provider");

    assert!(host.is_alive());
    assert!(Arc::ptr_eq(
        &gateway.state.lock().await.sessions[&host_id].inner,
        &host.inner
    ));
    let config = host
        .snapshot(None)
        .await
        .expect("reloaded chat")
        .ready
        .config;
    assert_eq!(config.config.provider, primary);
    assert_eq!(
        config.config.middleware.setting("subagents", "model_route"),
        None
    );
    assert!(
        ready
            .provider_instances
            .iter()
            .all(|provider| provider.selection.instance != removable.instance)
    );
    assert_eq!(
        ready
            .default_config
            .as_ref()
            .expect("gateway default")
            .config
            .middleware
            .setting("subagents", "model_route"),
        None
    );
    assert_eq!(
        credentials
            .get(
                &removable.instance,
                &removable.provider,
                removable.base_url.as_deref(),
            )
            .expect("credential"),
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
async fn active_chat_blocks_provider_removal_without_changes() {
    let root = tempfile::tempdir().expect("root");
    let (gateway, credentials, _, removable) = provider_removal_gateway(&root);
    let before = gateway
        .state
        .lock()
        .await
        .config
        .lock()
        .expect("gateway config")
        .clone();
    let (commands, mut receiver) = mpsc::channel(1);
    let busy_selection = removable.clone();
    tokio::spawn(async move {
        if let Some(HostCommand::ProviderCutoverStatus { reply }) = receiver.recv().await {
            let _ = reply.send(ProviderCutoverStatus {
                selection: busy_selection,
                provider_epoch: 0,
                idle: false,
            });
        }
    });
    let (events, _) = broadcast::channel(1);
    gateway.state.lock().await.sessions.insert(
        "busy".into(),
        HostHandle {
            inner: Arc::new(HostInner {
                session_id: "busy".into(),
                commands,
                events,
                accepts_file_attachments: Arc::new(AtomicBool::new(false)),
                alive: Arc::new(AtomicBool::new(true)),
            }),
        },
    );

    let error = gateway
        .remove_provider(removable.instance.clone())
        .await
        .expect_err("active chat must block provider removal");

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
            .expect("credential"),
        Some("unused-secret".into())
    );
}

#[tokio::test]
async fn dormant_chat_falls_back_when_removed_provider_is_reopened() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let (gateway, _, primary, removable) = provider_removal_gateway(&root);
    let host = gateway.create_session(&workspace).await.expect("chat");
    let before = host.snapshot(None).await.expect("snapshot").ready.config;
    let mut composition = before.config;
    composition.provider = removable.clone();
    host.configure(before.revision, composition)
        .await
        .expect("select removable provider");
    let session_id = host.session_id().to_owned();
    assert!(host.stop_if_idle().await);
    while host.is_alive() {
        tokio::task::yield_now().await;
    }
    gateway.state.lock().await.sessions.remove(&session_id);

    gateway
        .remove_provider(removable.instance)
        .await
        .expect("remove dormant provider");
    let reopened = gateway
        .open_session(&session_id)
        .await
        .expect("reopen chat");
    let config = reopened
        .snapshot(None)
        .await
        .expect("snapshot")
        .ready
        .config;

    assert_eq!(config.config.provider, primary);
    assert_eq!(
        config.config.middleware.setting("subagents", "model_route"),
        None
    );
}

#[tokio::test]
async fn provider_removal_save_failure_keeps_resident_alive() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let (gateway, credentials, _, removable) = provider_removal_gateway(&root);
    let host = gateway.create_session(&workspace).await.expect("chat");
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
            .expect("credential"),
        Some("unused-secret".into())
    );
}

#[tokio::test]
async fn provider_registration_commits_against_the_latest_usage() {
    let root = tempfile::tempdir().expect("root");
    let state_dir = root.path().join("state");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) = ConfigStore::initialize(state_dir.clone(), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
    let selection = ProviderConfig {
        instance: "openrouter".into(),
        provider: "openrouter".into(),
        model: "openai/gpt-5".into(),
        base_url: Some("https://connector.example/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::Credentialless,
        reasoning_effort: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let usage = mobius::protocol::TokenUsage {
        input_tokens: 13,
        total_tokens: 13,
        ..mobius::protocol::TokenUsage::default()
    };
    let state = gateway.state.lock().await;
    let stale = state.config.lock().expect("gateway config").clone();
    provider_registration(
        &stale,
        &selection,
        "Test",
        Default::default(),
        std::slice::from_ref(&selection.model),
        &[],
        true,
    )
    .expect("stale registration plan");
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
        Default::default(),
        std::slice::from_ref(&selection.model),
        &[],
        true,
    )
    .expect("commit registration");

    let latest = state.config.lock().expect("gateway config").clone();
    assert_eq!(latest.profile().daily_usage[0].usage, usage);
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
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway =
        GatewayHost::start(store, config, Arc::clone(&credentials), cron).expect("gateway");
    gateway.create_session(&workspace).await.expect("chat");
    let custom_endpoint = "https://example.com/v1";

    gateway
        .set_credential(
            "responses".into(),
            "responses".into(),
            "custom-secret".into(),
            Some(custom_endpoint.into()),
        )
        .await
        .expect("store custom credential");
    let error = gateway
        .set_credential(
            "openai_socket".into(),
            "openai_socket".into(),
            "fixed-secret".into(),
            Some(custom_endpoint.into()),
        )
        .await
        .expect_err("fixed provider endpoint must be rejected");

    assert_eq!(
        credentials
            .get("responses", "responses", Some(custom_endpoint))
            .expect("custom credential"),
        Some("custom-secret".into())
    );
    assert_eq!(
        credentials
            .get("responses", "openrouter", Some(custom_endpoint))
            .expect("different provider"),
        None
    );
    assert_eq!(error.code, "invalid_config");
    assert!(error.message.contains("fixed API endpoint"));
    assert_eq!(
        credentials
            .get("openai_socket", "openai_socket", None)
            .expect("fixed credential"),
        None
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
        )
        .expect("initial Kimi credential");
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
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
            false,
        )
        .await
        .expect("register Kimi");
    let first = gateway
        .create_session(&workspace)
        .await
        .expect("first chat");
    let second = gateway
        .create_session(&workspace)
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

#[test]
fn active_provider_login_reserves_the_only_polling_slot() {
    let active = StdMutex::new(None);
    reserve_provider_login(&active, "login-a").expect("reserve first login");
    let rejection = reserve_provider_login(&active, "login-b")
        .expect_err("a second provider login must be rejected");

    assert_eq!(rejection.code, "provider_login_in_progress");
    release_provider_login(&active, "another-login").expect("ignore stale completion");
    assert!(reserve_provider_login(&active, "login-b").is_err());
    release_provider_login(&active, "login-a").expect("finish first login");
    reserve_provider_login(&active, "login-b").expect("reserve next login");
}
