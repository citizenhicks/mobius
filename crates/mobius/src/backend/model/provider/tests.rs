use std::collections::BTreeSet;

use super::*;

#[test]
fn custom_image_operation_covers_edits_and_preserves_serialized_root_identity() {
    use super::super::endpoints::ProviderModelKind;
    let provider = provider("openai_socket").unwrap();
    let base = provider.default_base_url();
    let overridden = provider
        .with_group_endpoint(ProviderModelKind::Image, base, "images", "custom/images")
        .unwrap();
    for path in ["images/generations", "images/edits"] {
        assert_eq!(
            overridden.operation_url(path),
            format!("{}/custom/images", base.root())
        );
        assert_eq!(base.operation_url(path), format!("{}/{path}", base.root()));
    }
    let restored: BaseUrl =
        serde_json::from_value(serde_json::to_value(&overridden).unwrap()).unwrap();
    assert_eq!(&overridden, base);
    assert_eq!(overridden, restored);
}

#[test]
fn native_image_overrides_preserve_sibling_edit_paths_under_a_prefix() {
    use super::super::endpoints::ProviderModelKind;
    for id in ["openai_socket", "openai_codex", "responses"] {
        let definition = provider(id).unwrap();
        let base = definition.default_base_url();
        for (generation, edit) in [
            ("images/generations", "images/edits"),
            ("/images/generations", "images/edits"),
            ("/v2/images/generations", "v2/images/edits"),
            ("v2/notimages/generations", "v2/notimages/generations"),
            ("custom/images", "custom/images"),
        ] {
            let bound = definition
                .with_group_endpoint(ProviderModelKind::Image, base, "images", generation)
                .unwrap();
            assert_eq!(
                bound.operation_url("images/generations"),
                format!("{}/{}", base.root(), generation.trim_start_matches('/'))
            );
            assert_eq!(
                bound.operation_url("images/edits"),
                format!("{}/{edit}", base.root())
            );
        }
    }
    let base = BaseUrl::new("https://proxy.example/images").unwrap();
    let bound = provider("responses")
        .unwrap()
        .with_group_endpoint(ProviderModelKind::Image, &base, "images", "generations")
        .unwrap();
    assert_eq!(
        bound.operation_url("images/edits"),
        "https://proxy.example/images/generations"
    );
}

#[test]
fn current_provider_catalogs_advertise_supported_models_and_efforts() {
    assert!(
        provider("openai_socket")
            .unwrap()
            .models()
            .iter()
            .any(|model| model.id == "gpt-6.1-terra")
    );
    assert!(
        provider("anthropic")
            .unwrap()
            .models()
            .iter()
            .any(|model| model.id == "claude-fable-5-2")
    );
    let mistral = provider("mistral").unwrap();
    let large = mistral
        .models()
        .iter()
        .find(|model| model.id == "mistral-large-4-0")
        .unwrap();
    assert_eq!(
        large
            .reasoning
            .iter()
            .map(|effort| effort.id.as_str())
            .collect::<Vec<_>>(),
        ["low", "medium", "high"]
    );
    assert_eq!(large.default_reasoning.as_deref(), Some("high"));
}

#[test]
fn required_tool_choice_is_each_manifest_s_documented_wire_value() {
    let choice = |manifest: &'static ProviderMetadata| {
        serde_json::to_value(ToolChoice::new(
            manifest.required_tool_choice.as_ref(),
            Some("write_handoff"),
        ))
        .expect("tool choice")
    };
    assert_eq!(
        choice(&super::super::openai_socket::MANIFEST),
        serde_json::json!({"type": "function", "name": "write_handoff"})
    );
    assert_eq!(
        choice(&super::super::mistral::MANIFEST),
        serde_json::json!({"type": "function", "function": {"name": "write_handoff"}})
    );
    // No documented way to force a tool: the request stays on `auto`.
    assert_eq!(choice(&super::super::kimi::MANIFEST), "auto");
    assert_eq!(
        serde_json::to_value(ToolChoice::new(
            super::super::mistral::MANIFEST
                .required_tool_choice
                .as_ref(),
            None
        ))
        .expect("tool choice"),
        "auto"
    );
}

#[test]
fn configured_discovery_is_applied_by_each_api_key_transport() {
    for id in [
        "openai_socket",
        "responses",
        "openrouter",
        "aimlapi",
        "a2agent",
        "anthropic",
        "deepseek",
        "kimi",
        "mistral",
    ] {
        let definition = provider(id).unwrap();
        for discovery in [ToolDiscoveryMode::Native, ToolDiscoveryMode::Rebuild] {
            let built = definition.build(ProviderBuildConfig {
                capability: None,
                tool_discovery: Some(discovery),
                credential: ProviderCredential::ApiKey("test-key".into()),
                model: definition.default_model().unwrap_or("custom-model").into(),
                base_url: None,
                reasoning_effort: None,
                service_tier: None,
                web_search: HostedWebSearch::Off,
                http: crate::backend::model::ModelTransportSettings::default()
                    .streaming_client()
                    .unwrap(),
                transport: super::super::ModelTransportSettings::default(),
            });
            if definition.supports_tool_discovery(discovery) {
                assert_eq!(built.unwrap().tool_discovery(), discovery, "{id}");
            } else {
                assert!(
                    built
                        .err()
                        .unwrap()
                        .to_string()
                        .contains("does not support Native tool discovery")
                );
            }
        }
    }
    let model = &provider("anthropic").unwrap().models()[0];
    assert_eq!(
        serde_json::from_str::<ModelPreset>(&serde_json::to_string(model).unwrap()).unwrap(),
        *model
    );
}

#[test]
fn omitted_provider_endpoints_resolve_to_advertised_defaults() {
    for definition in providers() {
        let model = definition.default_model().unwrap_or("test-model");
        definition
            .build_config_is_valid(model, None, HostedWebSearch::Off)
            .expect("omitted URL selects the advertised default");
        if matches!(definition.auth(), ProviderAuth::ApiKey(_)) {
            definition
                .build(ProviderBuildConfig {
                    capability: None,
                    tool_discovery: None,
                    credential: ProviderCredential::ApiKey("test-secret".into()),
                    model: model.into(),
                    base_url: None,
                    reasoning_effort: None,
                    service_tier: None,
                    web_search: HostedWebSearch::Off,
                    http: crate::backend::model::ModelTransportSettings::default()
                        .streaming_client()
                        .unwrap(),
                    transport: super::super::ModelTransportSettings::default(),
                })
                .expect("provider constructor receives its advertised default");
        }
        if definition.supports_credentialless_endpoints() {
            definition
                .validate_credentialless_endpoint(None)
                .expect_err("credentialless endpoints still require an explicit custom URL");
        }
    }
}

#[test]
fn credentialless_providers_build_without_a_credential() {
    let credentialless = providers()
        .iter()
        .filter(|definition| definition.supports_credentialless_endpoints())
        .collect::<Vec<_>>();
    assert_eq!(
        credentialless
            .iter()
            .map(|definition| definition.id())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "anthropic",
            "deepseek",
            "kimi",
            "mistral",
            "openrouter",
            "responses"
        ])
    );
    let proxy = BaseUrl::new("https://proxy.example/v1").expect("proxy URL");
    let error = provider("openai_socket")
        .expect("fixed-endpoint provider")
        .validate_credentialless_endpoint(Some(&proxy))
        .expect_err("a provider that does not opt in is rejected");
    assert!(
        error
            .to_string()
            .contains("does not support credentialless")
    );
    for definition in credentialless {
        let base_url = definition.default_base_url();
        definition
            .validate_credentialless_endpoint(Some(&proxy))
            .expect("a non-default HTTPS endpoint is accepted");
        definition
            .validate_credentialless_endpoint(Some(base_url))
            .expect_err("the provider's own endpoint still requires authentication");
        definition
            .build(ProviderBuildConfig {
                capability: None,
                tool_discovery: None,
                credential: ProviderCredential::Credentialless,
                model: definition
                    .default_model()
                    .unwrap_or("test-model")
                    .to_owned(),
                base_url: Some(proxy.share()),
                reasoning_effort: None,
                service_tier: None,
                web_search: HostedWebSearch::Off,
                http: crate::backend::model::ModelTransportSettings::default()
                    .streaming_client()
                    .unwrap(),
                transport: crate::backend::model::ModelTransportSettings::default(),
            })
            .expect("credentialless provider builds without a credential");
    }
}
