use mobius::SessionId;
use mobius::backend::model::provider::BaseUrl;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};

use crate::bots::BotStore;
use crate::config::{ConfigStore, CredentialStore};
use crate::host::session::HostInner;
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
        tool_discovery: None,
        instance: "openrouter".into(),
        provider: "openrouter".into(),
        model: "openai/gpt-5".into(),
        base_url: Some(BaseUrl::new("https://connector.example/v1").unwrap()),
        endpoint_auth: ProviderEndpointAuth::Credentialless,
        reasoning_effort: None,
        service_tier: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let removable = ProviderConfig {
        tool_discovery: None,
        instance: "kimi-unused".into(),
        provider: "kimi".into(),
        model: "kimi-k3".into(),
        base_url: Some(BaseUrl::new("https://api.moonshot.ai/v1").unwrap()),
        endpoint_auth: ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: Some("max".into()),
        service_tier: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let config = config
        .registering_provider(
            primary.clone(),
            "Primary".into(),
            Default::default(),
            vec![crate::wire::ConfiguredModel {
                id: primary.model.clone(),
                reasoning_efforts: Some(Vec::new()),
                default_reasoning: None,
                ..Default::default()
            }],
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
    store
        .save(&config.clone().validated().expect("valid config"))
        .expect("save provider catalog");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    credentials
        .set(
            &removable.instance,
            &removable.provider,
            "unused-secret",
            removable.base_url.as_ref(),
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
async fn browser_proxy_registration_requires_operator_and_allows_trusted_root_reuse() {
    use base64::Engine as _;
    let root = tempfile::tempdir().expect("root");
    let (gateway, _, _, _) = provider_removal_gateway(&root).await;
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "test-account"}
        }))
        .expect("test claims"),
    );
    let auth_path = gateway.state.lock().await.store.provider_auth_path();
    std::fs::write(
        &auth_path,
        serde_json::to_vec(&serde_json::json!({
            "openai_codex": {
                "access": format!("e30.{claims}.signature"),
                "refresh": "test-refresh",
                "expires": u64::MAX,
                "account_id": "test-account"
            }
        }))
        .expect("test credential"),
    )
    .expect("write test credential");
    let mut selection = ProviderConfig {
        tool_discovery: None,
        instance: "trusted-codex".into(),
        provider: "openai_codex".into(),
        model: "gpt-6-luna".into(),
        base_url: Some(BaseUrl::new("https://operator-proxy.example/native").unwrap()),
        endpoint_auth: ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: None,
        service_tier: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let error = gateway
        .register_provider(
            false,
            crate::config::ProviderRegistration {
                selection: selection.clone(),
                label: Some("Proxy".into()),
                tint: Some(Default::default()),
                models: Vec::new(),
                image_model_ids: Some(Vec::new()),
            },
            false,
            false,
        )
        .await
        .expect_err("paired clients cannot introduce an authentication destination");
    assert_eq!(error.code, "operator_required");
    gateway
        .register_provider(
            true,
            crate::config::ProviderRegistration {
                selection: selection.clone(),
                label: Some("Proxy".into()),
                tint: Some(Default::default()),
                models: Vec::new(),
                image_model_ids: Some(Vec::new()),
            },
            false,
            false,
        )
        .await
        .expect("operator registers native proxy");
    selection.instance = "reused-codex".into();
    selection.base_url = Some(BaseUrl::new("https://operator-proxy.example/native/").unwrap());
    gateway
        .register_provider(
            false,
            crate::config::ProviderRegistration {
                selection: selection.clone(),
                label: Some("Reused".into()),
                tint: Some(Default::default()),
                models: Vec::new(),
                image_model_ids: Some(Vec::new()),
            },
            false,
            false,
        )
        .await
        .expect("paired client can reuse an operator-approved root");
    selection.base_url = Some(BaseUrl::new("https://client-controlled.example/native").unwrap());
    let error = gateway
        .register_provider(
            false,
            crate::config::ProviderRegistration {
                selection,
                label: Some("Retargeted".into()),
                tint: Some(Default::default()),
                models: Vec::new(),
                image_model_ids: Some(Vec::new()),
            },
            false,
            false,
        )
        .await
        .expect_err("paired client cannot retarget an approved instance");
    assert_eq!(error.code, "operator_required");
    let (_, persisted) = ConfigStore::open(root.path().join("state")).expect("saved config");
    assert_eq!(
        persisted.configured_providers["reused-codex"]
            .selection
            .base_url
            .as_deref(),
        Some("https://operator-proxy.example/native/")
    );
    gateway.shutdown().await;
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
    let session_id = host.session_id().share();
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
                removable.base_url.as_ref(),
            )
            .expect("credential")
            .map(|credential| credential.api_key),
        Some("unused-secret".into())
    );
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
async fn stale_provider_login_success_does_not_refresh_sessions_or_release_another_login() {
    let root = tempfile::tempdir().expect("root");
    let (gateway, _, _, _) = provider_removal_gateway(&root).await;
    reserve_test_login(&gateway, "b")
        .await
        .expect("current login");
    let (commands, mut receiver) = mpsc::channel(1);
    let (events, _) = broadcast::channel(1);
    gateway.state.lock().await.sessions.insert(
        SessionId::new("observer").unwrap(),
        super::super::HostHandle {
            inner: Arc::new(HostInner {
                session_id: SessionId::new("observer").unwrap(),
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
