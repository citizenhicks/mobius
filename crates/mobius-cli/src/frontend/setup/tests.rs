use std::sync::Arc;

use mobius::backend::model::provider::{BaseUrl, HostedWebSearch};
use mobius::protocol::{
    FrontendSetting, FrontendSettingKind, FrontendSettingOption, FrontendSymbol, MiddlewareFeature,
    ModelChoice, ToolDiscoveryMode,
};
use mobius_gateway::wire::{
    AgentComposition, ExtensionHookRecord, ExtensionKind, ExtensionRecord, ProviderAuthKind,
    ProviderEndpointAuth, ProviderInstance, ProviderModel, ProviderStatus, ReasoningChoice,
};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::SetupMode;
use super::runtime::credential_saved;
use super::state::{
    AuthField, Authentication, Flow, MiddlewareRow, Page, SetupState, provider_rows,
};
use super::view::{masked_credential, render_page};

fn status(provider: &str) -> ProviderStatus {
    let (auth, default_base_url, default_api_key_env, models, web_search) = match provider {
        "responses" => (
            ProviderAuthKind::ApiKey,
            Some(BaseUrl::new("https://api.openai.com/v1").unwrap()),
            None,
            Vec::new(),
            vec![search(HostedWebSearch::Off)],
        ),
        "kimi" => (
            ProviderAuthKind::ApiKey,
            None,
            Some("MOONSHOT_API_KEY".into()),
            vec![model("kimi-k3", "Kimi K3", Some("max"))],
            vec![search(HostedWebSearch::Off)],
        ),
        "openai_socket" => (
            ProviderAuthKind::ApiKey,
            Some(BaseUrl::new("https://api.openai.com/v1").unwrap()),
            None,
            vec![model("gpt-6-sol", "Sol", Some("medium"))],
            vec![
                search(HostedWebSearch::Off),
                search(HostedWebSearch::Cached),
                search(HostedWebSearch::Live),
            ],
        ),
        _ => panic!("unknown fixture provider"),
    };
    let model_ids_configurable = provider != "openai_socket";
    ProviderStatus {
        native_custom_endpoints: provider == "openai_socket",
        provider: provider.into(),
        label: provider.into(),
        symbol: FrontendSymbol::Storage,
        description: format!("{provider} provider"),
        model_ids_configurable,
        supported_tool_discovery: vec![ToolDiscoveryMode::Native, ToolDiscoveryMode::Rebuild],
        image_models: Default::default(),
        image_model_ids_configurable: false,
        voice_models: Default::default(),
        auth,
        default_base_url,
        default_api_key_env,
        models,
        web_search,
        tool_discovery: ToolDiscoveryMode::Rebuild,
        custom_endpoint_tool_discovery: None,
    }
}

fn search(mode: HostedWebSearch) -> FrontendSettingOption {
    FrontendSettingOption {
        disables: Arc::default(),
        value: mode.id().into(),
        label: mode.label().into(),
        description: mode.description().into(),
        symbol: None,
        tone: mobius::protocol::FrontendTone::Neutral,
    }
}

fn model(id: &str, label: &str, default_reasoning: Option<&str>) -> ProviderModel {
    ProviderModel {
        id: id.into(),
        label: label.into(),
        description: format!("{label} capabilities"),
        context_window: 1_000_000,
        reasoning: default_reasoning
            .into_iter()
            .map(|id| ReasoningChoice {
                id: id.into(),
                label: id.into(),
                description: format!("{id} reasoning"),
            })
            .collect(),
        default_reasoning: default_reasoning.map(str::to_owned),
        tool_discovery: ToolDiscoveryMode::Rebuild,
    }
}

fn route(route: &str, group: &str, model: &str, reasoning_effort: Option<&str>) -> ModelChoice {
    ModelChoice {
        route: route.into(),
        group: group.into(),
        model: model.into(),
        reasoning_effort: reasoning_effort.map(Arc::from),
        variant_label: None,
        context_window: Some(1_000_000),
        supports_image_input: false,
        supports_image_generation: false,
        supports_realtime_voice: false,
        tool_discovery: ToolDiscoveryMode::Rebuild,
    }
}

fn state(mode: SetupMode, provider: &str, configured: bool) -> SetupState {
    let statuses = vec![status(provider)];
    let mut original = AgentComposition::default();
    original.provider.instance = provider.into();
    original.provider.provider = provider.into();
    if let Some(model) = statuses[0].models.first() {
        original.provider.model.clone_from(&model.id);
        original
            .provider
            .reasoning_effort
            .clone_from(&model.default_reasoning);
    }
    original.provider.base_url = statuses[0].default_base_url.clone();
    let instances = vec![ProviderInstance {
        label: provider.into(),
        tint: Default::default(),
        configured,
        credential_hint: None,
        selection: original.provider.clone(),
        models: if statuses[0].model_ids_configurable {
            vec![model(
                &original.provider.model,
                &original.provider.model,
                original.provider.reasoning_effort.as_deref(),
            )]
        } else {
            Vec::new()
        },
        image_model_ids: Vec::new(),
    }];
    let providers = provider_rows(&statuses, &instances);
    assert!(std::sync::Arc::ptr_eq(
        &providers[0].status,
        &providers[1].status
    ));
    original.middleware.set_enabled("plain", true);
    original.middleware.set_enabled("configured", true);
    let mut state =
        SetupState::from_parts(mode, providers, features(), Vec::new(), original, false)
            .expect("setup state");
    if mode == SetupMode::BotModel {
        let current = state.original.provider.clone();
        state
            .set_bot_model_routes(
                &[route(
                    "current-route",
                    provider,
                    &current.model,
                    current.reasoning_effort.as_deref(),
                )],
                &std::collections::BTreeMap::from([(
                    "current-route".into(),
                    current.instance.clone(),
                )]),
            )
            .expect("Bot model routes");
    }
    state
}

fn features() -> Vec<MiddlewareFeature> {
    vec![
        MiddlewareFeature {
            id: "plain".into(),
            label: "Plain".into(),
            description: "Plain optional capability".into(),
            required: false,
            settings: Vec::new(),
        },
        MiddlewareFeature {
            id: "configured".into(),
            label: "Configured".into(),
            description: "Capability with advertised settings".into(),
            required: false,
            settings: vec![
                FrontendSetting {
                    id: "limit".into(),
                    label: "Limit".into(),
                    description: "An advertised integer".into(),
                    composer: false,
                    kind: FrontendSettingKind::Integer {
                        min: 1,
                        max: Some(100),
                        step: 10,
                    },
                },
                FrontendSetting {
                    id: "route".into(),
                    label: "Route".into(),
                    description: "An advertised selection".into(),
                    composer: false,
                    kind: FrontendSettingKind::Select {
                        options: vec![FrontendSettingOption {
                            disables: Arc::default(),
                            value: "route-a".into(),
                            label: "Route A".into(),
                            description: "First route".into(),
                            symbol: None,
                            tone: mobius::protocol::FrontendTone::Neutral,
                        }],
                        unset_label: Some("Inherit".into()),
                    },
                },
            ],
        },
        MiddlewareFeature {
            id: "required".into(),
            label: "Required".into(),
            description: "Required capability".into(),
            required: true,
            settings: Vec::new(),
        },
    ]
}

fn feature_row(state: &SetupState, id: &str) -> usize {
    (0..state.middleware_row_count())
        .find(|row| {
            matches!(
                state.middleware_row(*row),
                Some(MiddlewareRow::Feature(index)) if state.features[index].id == id
            )
        })
        .expect("feature row")
}

fn extension(capability: &str) -> ExtensionRecord {
    ExtensionRecord {
        id: "plugin-a".into(),
        capability: capability.to_owned().into(),
        kind: ExtensionKind::Plugin,
        name: "Plugin A".into(),
        description: "An installed plugin".into(),
        version: Some("1.0.0".into()),
        source: "https://github.com/example/plugin-a".into(),
        reference: None,
        subdirectory: None,
        resolved_revision: "abc123".into(),
        digest: "sha256:plugin-a".into(),
        skills: Arc::from(["plugin-a".into()]),
        hooks: Vec::new().into(),
        hooks_trusted: false,
    }
}

#[test]
fn configurable_provider_requires_an_exact_authenticated_endpoint() {
    let mut custom = state(SetupMode::Login, "responses", true);
    custom.endpoint = "https://other.example/v1".into();

    assert!(!custom.has_matching_credential());
    custom.authentication_succeeded();
    assert!(custom.has_matching_credential());

    let fixed = state(SetupMode::Login, "kimi", true);
    assert!(fixed.has_matching_credential());
}

#[test]
fn empty_api_key_reuse_is_deferred_to_the_gateway() {
    let mut state = state(SetupMode::Login, "openai_socket", false);
    state.provider = 1;
    state.reset_provider_fields();

    assert!(matches!(state.authentication(), Authentication::Reuse));
}

#[test]
fn agent_is_one_page_and_preserves_unedited_provider_settings() {
    let mut state = state(SetupMode::Bot, "openai_socket", true);
    state.original.provider.web_search = HostedWebSearch::Live;
    state.original.system_prompt = "Keep this system prompt".into();
    state.middleware.set_enabled("plain", true);
    state.row = feature_row(&state, "plain");
    state.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    let original = state.original.clone();
    state.row = state.agent_action_start();

    assert_eq!(state.page, Page::Agent);
    assert_eq!(
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Flow::Finish
    );
    let configured = state
        .agent_composition(&original)
        .expect("agent composition");

    assert_eq!(configured.provider, original.provider);
    assert!(!configured.middleware.enabled("plain"));
    assert_eq!(configured.system_prompt, "Keep this system prompt");
}

#[test]
fn agent_activates_untrusted_plugin_skills_without_authorizing_hooks() {
    let mut state = state(SetupMode::Bot, "openai_socket", true);
    let mut untrusted = extension("plain");
    untrusted.hooks = Arc::from([ExtensionHookRecord {
        event: "PreToolUse".into(),
        matcher: None,
        command: "review-hook".into(),
        timeout_seconds: 10,
    }]);
    state.available_extensions.push(untrusted);
    state.row = feature_row(&state, "plain");
    state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    state.row = (0..state.middleware_row_count())
        .find(|row| {
            matches!(
                state.middleware_row(*row),
                Some(MiddlewareRow::Extension { extension, .. })
                    if state.available_extensions[extension].id == "plugin-a".into()
            )
        })
        .expect("extension row");

    state.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));

    assert!(state.selected_extensions.contains("plugin-a"));
    assert_eq!(state.error, None);
}

#[test]
fn credential_entry_is_masked_and_supports_backspace() {
    let mut state = state(SetupMode::Login, "openai_socket", false);
    state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(state.auth_field, AuthField::Credential);
    state.paste("abc123\n");
    state.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));

    assert_eq!(masked_credential(&state.credential, 32), "•••••");
    assert_eq!(state.label, "openai_socket");
    let mut lines = Vec::new();
    render_page(&mut lines, &state, 100);
    let rendered = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("Name (optional)"));
    assert!(!rendered.contains("abc12"));
}

#[test]
fn authentication_rejection_preserves_input_for_retry() {
    let mut state = state(SetupMode::Login, "openai_socket", false);
    state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    state.paste("new-secret");
    assert!(matches!(state.authentication(), Authentication::ApiKey(_)));
    state.set_progress("Saving credential", "Waiting…");

    assert!(
        state
            .authentication_finished(Err(mobius::Error::Auth(
                "Key rejected; try another key".into()
            )))
            .expect("retry")
    );
    assert_eq!(state.page, Page::Authentication);
    assert_eq!(state.credential, "new-secret");
    assert!(state.progress.is_none());
    assert_eq!(
        state.error.as_deref(),
        Some("Key rejected; try another key")
    );

    state.authentication_finished(Ok(true)).expect("finish");
    assert_eq!(state.page, Page::Models);
    assert!(state.credential.is_empty());
    assert!(state.error.is_none());
}

#[test]
fn explicit_api_key_replaces_credentialless_auth() {
    let mut state = state(SetupMode::Login, "responses", true);
    state.original.provider.endpoint_auth = ProviderEndpointAuth::Credentialless;
    state.credential = "replacement-secret".into();

    assert!(matches!(state.authentication(), Authentication::ApiKey(_)));
    let config = state
        .agent_composition(&state.original)
        .expect("agent composition");

    assert_eq!(
        config.provider.endpoint_auth,
        ProviderEndpointAuth::ProviderDefault
    );
}

#[test]
fn credential_response_matches_instance_and_provider() {
    let response = mobius_gateway::wire::ServerMessage::ProviderCredentialSaved {
        request_id: "save".into(),
        instance: "work".into(),
        provider: "responses".into(),
    };
    assert!(credential_saved(&response, "save", "work", "responses"));
    assert!(!credential_saved(&response, "other", "work", "responses"));
    assert!(!credential_saved(
        &response,
        "save",
        "personal",
        "responses"
    ));
    assert!(!credential_saved(&response, "save", "work", "openrouter"));
}
