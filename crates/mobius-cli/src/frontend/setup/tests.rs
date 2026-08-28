use mobius::backend::model::provider::HostedWebSearch;
use mobius::protocol::{
    FrontendSetting, FrontendSettingKind, FrontendSettingOption, FrontendSettingValue,
    FrontendSymbol, MiddlewareFeature, ToolDiscoveryMode,
};
use mobius_gateway::wire::{
    AgentComposition, ExtensionHookRecord, ExtensionKind, ExtensionRecord, ProviderAuthKind,
    ProviderConfig, ProviderEndpointAuth, ProviderInstance, ProviderModel, ProviderStatus,
    ReasoningChoice,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use uuid::Uuid;

use super::SetupMode;
use super::runtime::ExpectedResponse;
use super::state::{
    ApplyTarget, AuthField, Authentication, Flow, MiddlewareRow, Page, SetupState,
    validated_providers,
};
use super::view::{agent_layout, display_width, masked_credential, render, render_page};
use crate::frontend::theme::{Role, current};

fn status(provider: &str) -> ProviderStatus {
    let (auth, default_base_url, default_api_key_env, models, web_search) = match provider {
        "responses" => (
            ProviderAuthKind::ApiKey,
            Some("https://api.openai.com/v1".into()),
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
            None,
            Some("OPENAI_API_KEY".into()),
            vec![model("gpt-5.6-sol", "Sol", Some("medium"))],
            vec![
                search(HostedWebSearch::Off),
                search(HostedWebSearch::Cached),
                search(HostedWebSearch::Live),
            ],
        ),
        _ => panic!("unknown fixture provider"),
    };
    let model_ids_configurable = models.is_empty();
    ProviderStatus {
        provider: provider.into(),
        label: provider.into(),
        symbol: FrontendSymbol::Storage,
        description: format!("{provider} provider"),
        model_ids_configurable,
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
        default_reasoning: default_reasoning.map(str::to_string),
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
    if statuses[0].configurable_base_url() {
        original.provider.base_url = statuses[0].default_base_url.clone();
    }
    let instances = vec![ProviderInstance {
        label: provider.into(),
        tint: Default::default(),
        configured,
        credential_hint: None,
        selection: original.provider.clone(),
        model_ids: if statuses[0].model_ids_configurable {
            vec![original.provider.model.clone()]
        } else {
            Vec::new()
        },
        reasoning_efforts: if statuses[0].model_ids_configurable {
            original
                .provider
                .reasoning_effort
                .clone()
                .into_iter()
                .collect()
        } else {
            Vec::new()
        },
    }];
    let providers = validated_providers(&statuses, &instances).expect("validated providers");
    original.middleware.set_enabled("plain", true);
    original.middleware.set_enabled("configured", true);
    SetupState::from_parts(mode, providers, features(), Vec::new(), original, false)
        .expect("setup state")
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

fn expand_feature(state: &mut SetupState, id: &str) {
    let feature = state
        .features
        .iter()
        .position(|feature| feature.id == id)
        .expect("feature");
    if !state.expanded_features.contains(id) {
        state.toggle_feature_expansion(feature);
    }
}

fn setting_row(state: &mut SetupState, feature_id: &str, setting_id: &str) -> usize {
    expand_feature(state, feature_id);
    (0..state.middleware_row_count())
        .find(|row| {
            matches!(
                state.middleware_row(*row),
                Some(MiddlewareRow::Setting { feature, setting })
                    if state.features[feature].id == feature_id
                        && state.features[feature].settings[setting].id == setting_id
            )
        })
        .expect("setting row")
}

fn extension(capability: &str) -> ExtensionRecord {
    ExtensionRecord {
        id: "plugin-a".into(),
        capability: capability.into(),
        kind: ExtensionKind::Plugin,
        name: "Plugin A".into(),
        description: "An installed plugin".into(),
        version: Some("1.0.0".into()),
        source: "https://github.com/example/plugin-a".into(),
        reference: None,
        subdirectory: None,
        resolved_revision: "abc123".into(),
        digest: "sha256:plugin-a".into(),
        skills: vec!["plugin-a".into()],
        hooks: Vec::new(),
        hooks_trusted: false,
    }
}

#[test]
fn login_is_three_pages_with_endpoint_and_custom_model_inline() {
    let mut state = state(SetupMode::Login, "responses", false);

    assert_eq!(state.page, Page::Provider);
    assert_eq!(
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Flow::Continue
    );
    assert_eq!(state.page, Page::Authentication);
    state.credential = "secret".into();
    // Tab walks Name -> API key -> Base URL for a configurable-endpoint provider.
    assert_eq!(state.auth_field, AuthField::Label);
    state.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(state.auth_field, AuthField::Credential);
    state.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(state.auth_field, AuthField::Endpoint);
    assert_eq!(state.page, Page::Authentication);
    assert_eq!(
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Flow::Authenticate
    );
    assert_eq!(state.page, Page::Authentication);
    state.authentication_succeeded();
    assert_eq!(state.page, Page::Models);
    state.row = 0;
    state.custom_model.clear();
    state.paste("custom-model, alternate-model");
    assert_eq!(
        state.configured_model_ids().expect("model IDs"),
        ["custom-model", "alternate-model"]
    );
    state.row = state.models_action_start();
    assert_eq!(
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Flow::Finish
    );
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

    assert!(matches!(
        state.take_authentication().expect("deferred credential"),
        Authentication::Reuse
    ));
}

#[test]
fn device_login_is_reused_across_same_provider_instances() {
    let mut state = state(SetupMode::Login, "kimi", true);
    for entry in &mut state.providers {
        entry.status.auth = ProviderAuthKind::DeviceCode;
    }
    state.provider = 1;
    state.reset_provider_fields();

    assert!(state.has_matching_credential());
    assert!(matches!(
        state.take_authentication().expect("shared provider login"),
        Authentication::Reuse
    ));
}

#[test]
fn configured_fixed_provider_can_be_selected_from_another_provider() {
    let statuses = [status("responses"), status("kimi")];
    let mut original = AgentComposition::default();
    original.provider.instance = "responses".into();
    original.provider.provider = "responses".into();
    original.provider.base_url = statuses[0].default_base_url.clone();
    let mut kimi = original.provider.clone();
    kimi.instance = "kimi".into();
    kimi.provider = "kimi".into();
    kimi.base_url = None;
    kimi.model = "kimi-k3".into();
    kimi.reasoning_effort = Some("max".into());
    let instances = vec![
        ProviderInstance {
            label: "responses".into(),
            tint: Default::default(),
            configured: false,
            credential_hint: None,
            selection: original.provider.clone(),
            model_ids: vec![original.provider.model.clone()],
            reasoning_efforts: original
                .provider
                .reasoning_effort
                .clone()
                .into_iter()
                .collect(),
        },
        ProviderInstance {
            label: "kimi".into(),
            tint: Default::default(),
            configured: true,
            credential_hint: None,
            selection: kimi,
            model_ids: Vec::new(),
            reasoning_efforts: Vec::new(),
        },
    ];
    let providers = validated_providers(&statuses, &instances).expect("validated providers");
    let mut state = SetupState::from_parts(
        SetupMode::Login,
        providers,
        features(),
        Vec::new(),
        original,
        false,
    )
    .expect("setup state");

    state.select_provider("kimi").expect("select Kimi");

    assert!(state.has_matching_credential());
}

#[test]
fn new_provider_reuses_one_opaque_instance_id() {
    let mut state = state(SetupMode::Login, "kimi", true);
    state.provider = 1;
    state.reset_provider_fields();

    let instance = state.target_instance();
    let config = state
        .agent_composition(&state.original)
        .expect("new provider config");

    assert!(Uuid::parse_str(&instance).is_ok());
    assert_eq!(config.provider.instance, instance);
}

#[test]
fn preferred_provider_must_be_advertised() {
    let mut state = state(SetupMode::Login, "responses", false);

    let error = state
        .select_provider("missing")
        .expect_err("unknown provider must fail");

    assert!(error.to_string().contains("run `/login`"));
}

#[test]
fn unchanged_custom_model_keeps_its_reasoning_effort() {
    let state = state(SetupMode::Login, "responses", true);
    let mut current = state.original.clone();
    current.provider.reasoning_effort = Some("provider-defined".into());

    let configured = state.agent_composition(&current).expect("configuration");

    assert_eq!(
        configured.provider.reasoning_effort.as_deref(),
        Some("provider-defined")
    );
}

#[test]
fn hosted_search_is_selected_only_from_the_gateway_manifest() {
    let mut selectable = state(SetupMode::Login, "openai_socket", true);
    let search_start = selectable.model_choice_count() + selectable.reasoning_choice_count();
    selectable.row = search_start + 2;
    selectable.select_model_row();
    let configured = selectable
        .agent_composition(&selectable.original)
        .expect("select live search");
    assert_eq!(configured.provider.web_search, HostedWebSearch::Live);

    let fixed = state(SetupMode::Login, "kimi", true);
    assert_eq!(fixed.definition().web_search[0].value, "off");
    assert_eq!(
        fixed
            .agent_composition(&fixed.original)
            .expect("fixed search")
            .provider
            .web_search,
        HostedWebSearch::Off
    );
}

#[test]
fn agent_is_one_page_and_preserves_unedited_provider_settings() {
    let mut state = state(SetupMode::Agent, "openai_socket", true);
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
fn agent_capability_children_are_collapsed_until_opened() {
    let mut state = state(SetupMode::Agent, "openai_socket", true);
    assert_eq!(state.middleware_row_count(), state.features.len());
    state.row = feature_row(&state, "configured");

    state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(
        state.middleware_row_count(),
        state.features.len() + state.features[1].settings.len()
    );
    state.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(state.middleware_row_count(), state.features.len());
}

#[test]
fn agent_activates_installed_children_through_their_advertised_capability() {
    let mut state = state(SetupMode::Agent, "openai_socket", true);
    state.available_extensions.push(extension("plain"));
    state.middleware.set_enabled("plain", false);
    state.row = feature_row(&state, "plain");
    state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let extension_row = (0..state.middleware_row_count())
        .find(|row| {
            matches!(
                state.middleware_row(*row),
                Some(MiddlewareRow::Extension { feature, extension })
                    if state.features[feature].id == "plain"
                        && state.available_extensions[extension].id == "plugin-a"
            )
        })
        .expect("capability child row");
    state.row = extension_row;

    state.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));

    let configured = state
        .agent_composition(&state.original)
        .expect("agent composition");
    assert!(configured.extensions.contains("plugin-a"));
    assert!(configured.middleware.enabled("plain"));

    state.row = feature_row(&state, "plain");
    state.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    assert!(state.selected_extensions.contains("plugin-a"));
    assert!(!state.middleware.enabled("plain"));
}

#[test]
fn agent_activates_untrusted_plugin_skills_without_authorizing_hooks() {
    let mut state = state(SetupMode::Agent, "openai_socket", true);
    let mut untrusted = extension("plain");
    untrusted.hooks.push(ExtensionHookRecord {
        event: "PreToolUse".into(),
        matcher: None,
        command: "review-hook".into(),
        timeout_seconds: 10,
    });
    state.available_extensions.push(untrusted);
    state.row = feature_row(&state, "plain");
    state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    state.row = (0..state.middleware_row_count())
        .find(|row| {
            matches!(
                state.middleware_row(*row),
                Some(MiddlewareRow::Extension { extension, .. })
                    if state.available_extensions[extension].id == "plugin-a"
            )
        })
        .expect("extension row");

    state.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));

    assert!(state.selected_extensions.contains("plugin-a"));
    assert_eq!(state.error, None);
}

#[test]
fn agent_edits_an_advertised_select_without_knowing_the_middleware() {
    let mut state = state(SetupMode::Agent, "openai_socket", true);
    let row = setting_row(&mut state, "configured", "route");
    state.row = row;

    state.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));

    let configured = state
        .agent_composition(&state.original)
        .expect("agent composition");

    assert_eq!(
        configured.middleware.setting("configured", "route"),
        Some(&FrontendSettingValue::String("route-a".into()))
    );
}

#[test]
fn agent_edits_an_advertised_integer_without_knowing_the_middleware() {
    let mut state = state(SetupMode::Agent, "openai_socket", true);
    state.middleware.set_setting(
        "configured",
        "limit",
        Some(FrontendSettingValue::Integer(50)),
    );
    let row = setting_row(&mut state, "configured", "limit");
    state.row = row;

    state.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));

    assert_eq!(
        state
            .agent_composition(&state.original)
            .expect("agent composition")
            .middleware
            .setting("configured", "limit"),
        Some(&FrontendSettingValue::Integer(60))
    );
}

#[test]
fn agent_setting_rows_style_inherited_explicit_and_focused_values() {
    let mut inherited = state(SetupMode::Agent, "openai_socket", true);
    expand_feature(&mut inherited, "configured");
    let mut inherited_lines = Vec::new();
    render_page(&mut inherited_lines, &inherited, 82);
    let inherited_route = inherited_lines
        .iter()
        .find(|line| line.to_string().contains("Route  ‹ Inherit ›"))
        .expect("inherited route row");

    let mut explicit = state(SetupMode::Agent, "openai_socket", true);
    explicit.middleware.set_setting(
        "configured",
        "route",
        Some(FrontendSettingValue::String("route-a".into())),
    );
    explicit.middleware.set_setting(
        "configured",
        "limit",
        Some(FrontendSettingValue::Integer(50)),
    );
    expand_feature(&mut explicit, "configured");
    let mut explicit_lines = Vec::new();
    render_page(&mut explicit_lines, &explicit, 82);
    let explicit_route = explicit_lines
        .iter()
        .find(|line| line.to_string().contains("Route  ‹ Route A ›"))
        .expect("explicit route row");
    let explicit_limit = explicit_lines
        .iter()
        .find(|line| line.to_string().contains("Limit  ‹ 50 ›"))
        .expect("explicit integer row");

    let row = setting_row(&mut explicit, "configured", "route");
    explicit.row = row;
    let mut focused_lines = Vec::new();
    render_page(&mut focused_lines, &explicit, 82);
    let focused_route = focused_lines
        .iter()
        .find(|line| line.to_string().contains("Route  ‹ Route A ›"))
        .expect("focused route row");
    let theme = current();

    assert_eq!(
        (
            inherited_route.spans[0].style.fg,
            inherited_route.spans[1].style.fg,
            inherited_route.spans[2].style.fg,
            inherited_route
                .to_string()
                .contains("An advertised selection"),
            explicit_route.spans[1].style.fg,
            explicit_limit.spans[1].style.fg,
            focused_route.style,
            focused_route.spans[0].style.fg,
            focused_route.spans[1].style.fg,
            focused_route.spans[2].style.fg,
        ),
        (
            Some(theme.color(Role::Text)),
            Some(theme.color(Role::Info)),
            Some(theme.color(Role::Muted)),
            true,
            Some(theme.color(Role::Accent)),
            Some(theme.color(Role::Accent)),
            theme.style(Role::Selection),
            Some(theme.color(Role::Selection)),
            Some(theme.color(Role::Selection)),
            Some(theme.color(Role::Selection)),
        )
    );
}

#[test]
fn agent_descriptions_share_a_column_and_wrap_under_it() {
    fn column_of(line: &str, value: &str) -> Option<usize> {
        line.find(value).map(|index| display_width(&line[..index]))
    }

    let mut state = state(SetupMode::Agent, "openai_socket", true);
    expand_feature(&mut state, "configured");
    let layout = agent_layout(&state, 70);
    let column = layout.description_column.expect("wide inline layout");
    state.features[0].description = format!("{} wrapped-marker", "x".repeat(layout.width - column));
    state.row = feature_row(&state, "plain");
    let mut lines = Vec::new();

    render_page(&mut lines, &state, layout.width as u16);

    let feature = lines
        .iter()
        .find(|line| line.to_string().contains("[x] Plain"))
        .expect("feature row")
        .to_string();
    let setting = lines
        .iter()
        .find(|line| line.to_string().contains("An advertised integer"))
        .expect("setting row")
        .to_string();
    let action_description = "Restart the active chat";
    let action = lines
        .iter()
        .find(|line| line.to_string().contains(action_description))
        .expect("apply row")
        .to_string();
    let continuation = lines
        .iter()
        .find(|line| line.to_string().contains("wrapped-marker"))
        .expect("wrapped feature description");

    assert_eq!(column_of(&feature, "xxxxx"), Some(column));
    assert_eq!(column_of(&setting, "An advertised integer"), Some(column));
    assert_eq!(column_of(&action, action_description), Some(column));
    assert_eq!(
        column_of(&continuation.to_string(), "wrapped-marker"),
        Some(column)
    );
    assert_eq!(continuation.style, current().style(Role::Selection));
}

#[test]
fn selected_agent_row_stays_visible_in_a_short_viewport() {
    let mut state = state(SetupMode::Agent, "openai_socket", true);
    state.row = state.agent_action_start() + 1;
    let mut terminal = Terminal::new(TestBackend::new(90, 10)).expect("terminal");

    terminal
        .draw(|frame| render(frame, &state))
        .expect("agent setup draw");

    assert!(terminal.backend().to_string().contains("Save as default"));
}

#[test]
fn save_as_default_row_selects_the_default_target() {
    let mut state = state(SetupMode::Agent, "openai_socket", true);
    state.row = state.agent_action_start() + 1;

    let flow = state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(flow, Flow::Finish);
    assert_eq!(state.target, ApplyTarget::Default);
}

#[test]
fn required_features_are_visible_but_cannot_be_toggled() {
    let mut state = state(SetupMode::Agent, "openai_socket", true);
    state.row = feature_row(&state, "required");

    state.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));

    assert!(!state.middleware.enabled("required"));
    let mut lines = Vec::new();
    render_page(&mut lines, &state, 82);
    assert!(
        lines
            .iter()
            .any(|line| line.to_string().contains("[x] Required"))
    );
}

#[test]
fn provider_validation_rejects_an_incomplete_manifest() {
    let mut advertised = status("openai_socket");
    advertised.web_search[0].description.clear();

    let error = match validated_providers(&[advertised], &[]) {
        Ok(_) => panic!("incomplete provider manifest must fail"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("incomplete manifest"));
}

#[test]
fn provider_validation_rejects_duplicate_ids() {
    let advertised = status("openai_socket");

    let error = match validated_providers(&[advertised.clone(), advertised], &[]) {
        Ok(_) => panic!("duplicate providers must fail"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("duplicate provider"));
}

#[test]
fn setup_rejects_active_provider_values_outside_the_manifest() {
    let reject = |status: ProviderStatus, config: ProviderConfig, catalog: Vec<String>| {
        let instances = catalog.is_empty().then(Vec::new).unwrap_or_else(|| {
            vec![ProviderInstance {
                label: config.instance.clone(),
                tint: Default::default(),
                configured: true,
                credential_hint: None,
                selection: config.clone(),
                model_ids: vec![config.model.clone()],
                reasoning_efforts: catalog.clone(),
            }]
        });
        let original = AgentComposition {
            provider: config,
            ..AgentComposition::default()
        };
        // An invalid selection is caught either while validating the advertised
        // instances or while seeding setup state from them.
        let providers = match validated_providers(&[status], &instances) {
            Ok(providers) => providers,
            Err(error) => return error.to_string(),
        };
        match SetupState::from_parts(
            SetupMode::Login,
            providers,
            features(),
            Vec::new(),
            original,
            false,
        ) {
            Ok(_) => panic!("invalid active provider state must fail"),
            Err(error) => error.to_string(),
        }
    };

    let missing_provider = reject(
        status("kimi"),
        ProviderConfig {
            instance: "missing".into(),
            provider: "missing".into(),
            model: "model".into(),
            base_url: None,
            endpoint_auth: ProviderEndpointAuth::ProviderDefault,
            reasoning_effort: None,
            web_search: HostedWebSearch::Off,
        },
        Vec::new(),
    );
    assert!(missing_provider.contains("active provider"));

    let missing_model = reject(
        status("openai_socket"),
        ProviderConfig {
            instance: "openai_socket".into(),
            provider: "openai_socket".into(),
            model: "missing".into(),
            base_url: None,
            endpoint_auth: ProviderEndpointAuth::ProviderDefault,
            reasoning_effort: None,
            web_search: HostedWebSearch::Off,
        },
        Vec::new(),
    );
    assert!(missing_model.contains("unadvertised model"));

    let missing_search = reject(
        status("kimi"),
        ProviderConfig {
            instance: "kimi".into(),
            provider: "kimi".into(),
            model: "kimi-k3".into(),
            base_url: None,
            endpoint_auth: ProviderEndpointAuth::ProviderDefault,
            reasoning_effort: Some("max".into()),
            web_search: HostedWebSearch::Live,
        },
        Vec::new(),
    );
    assert!(missing_search.contains("unadvertised web-search"));

    let missing_reasoning = reject(
        status("openai_socket"),
        ProviderConfig {
            instance: "openai_socket".into(),
            provider: "openai_socket".into(),
            model: "gpt-5.6-sol".into(),
            base_url: None,
            endpoint_auth: ProviderEndpointAuth::ProviderDefault,
            reasoning_effort: Some("missing".into()),
            web_search: HostedWebSearch::Off,
        },
        Vec::new(),
    );
    assert!(missing_reasoning.contains("unadvertised reasoning"));

    let missing_custom_reasoning = reject(
        status("responses"),
        ProviderConfig {
            instance: "responses".into(),
            provider: "responses".into(),
            model: AgentComposition::default().provider.model,
            base_url: Some("https://api.openai.com/v1".into()),
            endpoint_auth: ProviderEndpointAuth::ProviderDefault,
            reasoning_effort: Some("missing".into()),
            web_search: HostedWebSearch::Off,
        },
        vec!["medium".into()],
    );
    assert!(missing_custom_reasoning.contains("unconfigured reasoning"));
}

#[test]
fn agent_reuses_authentication_without_provider_controls() {
    let mut state = state(SetupMode::Agent, "responses", true);

    assert_eq!(state.page, Page::Agent);
    assert!(matches!(
        state.take_authentication().expect("reuse authentication"),
        Authentication::Reuse
    ));
}

#[test]
fn credential_entry_is_masked_and_supports_backspace() {
    let mut state = state(SetupMode::Login, "openai_socket", false);
    state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    // The name field takes focus first; tab moves to the key.
    state.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(state.auth_field, AuthField::Credential);
    state.paste("abc123\n");
    state.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));

    assert_eq!(masked_credential(&state.credential), "•••••");
}

#[test]
fn explicit_api_key_replaces_credentialless_auth() {
    let mut state = state(SetupMode::Login, "responses", true);
    state.original.provider.endpoint_auth = ProviderEndpointAuth::Credentialless;
    state.credential = "replacement-secret".into();

    assert!(matches!(
        state.take_authentication().expect("API-key authentication"),
        Authentication::ApiKey(_)
    ));
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
    let expected = ExpectedResponse::Credential {
        instance: "work",
        provider: "responses",
    };

    assert!(expected.matches_credential("work", "responses"));
    assert!(!expected.matches_credential("personal", "responses"));
    assert!(!expected.matches_credential("work", "openrouter"));
}

#[test]
fn provider_removal_requires_a_configured_row_and_confirmation() {
    let mut state = state(SetupMode::Login, "kimi", true);
    state.provider = 1;
    assert_eq!(
        state.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)),
        Flow::Continue
    );
    assert!(state.remove_confirmation.is_none());

    state.provider = 0;
    state.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
    assert!(state.remove_confirmation.is_some());
    assert_eq!(
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Flow::Continue
    );
    assert_eq!(
        state.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
        Flow::Remove("kimi".into())
    );
}
