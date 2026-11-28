use mobius::backend::model::provider::provider;

use super::*;

#[test]
fn selected_endpoint_uses_explicit_then_default_urls_only_for_configurable_providers() {
    for (provider_id, explicit, expected) in [
        ("openrouter", None, Some("https://openrouter.ai/api/v1")),
        (
            "openrouter",
            Some("https://custom.example/v1"),
            Some("https://custom.example/v1"),
        ),
        (
            "openai_codex",
            None,
            Some(
                provider("openai_codex")
                    .expect("Codex")
                    .default_base_url()
                    .as_str(),
            ),
        ),
        (
            "openai_codex",
            Some("https://custom.example/v1"),
            Some("https://custom.example/v1"),
        ),
    ] {
        let mut selection = crate::wire::AgentComposition::default().provider;
        selection.provider = provider_id.into();
        selection.base_url = explicit.map(|url| BaseUrl::new(url).unwrap());
        assert_eq!(
            Some(selected_base_url(provider(provider_id).expect("provider"), &selection).as_str()),
            expected
        );
    }
}

#[test]
fn configured_catalogs_preserve_per_model_reasoning_and_plain_routes() {
    use crate::config::ConfiguredModel;

    let mut selection = crate::wire::AgentComposition::default().provider;
    selection.instance = "custom".into();
    selection.provider = "responses".into();
    selection.model = "reasoner".into();
    selection.reasoning_effort = None;
    let mut configured = ConfiguredProvider {
        image_defaults: Default::default(),
        operator_managed: false,
        selection,
        label: "Custom".into(),
        tint: Default::default(),
        models: prepare_configured_models(
            provider("responses").expect("provider"),
            vec![
                ConfiguredModel {
                    id: "reasoner".into(),
                    reasoning_efforts: Some(vec!["low".into(), "high".into()]),
                    default_reasoning: Some("high".into()),
                    ..Default::default()
                },
                ConfiguredModel {
                    id: "fast::v2".into(),
                    reasoning_efforts: Some(vec!["brief::soft".into()]),
                    default_reasoning: Some("brief::soft".into()),
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
        image_models: None,
        voice_models: None,
        group_endpoints: Vec::new(),
        dictation_model_ids: Vec::new(),
        decision_model_ids: Vec::new(),
    };
    let definition = provider("responses").expect("custom provider");
    let models = configured_models(definition, &configured);
    assert_eq!(
        models
            .iter()
            .map(|model| (
                model.id.as_str(),
                model
                    .reasoning
                    .iter()
                    .map(|effort| effort.id.as_str())
                    .collect::<Vec<_>>(),
                model.default_reasoning.as_deref(),
            ))
            .collect::<Vec<_>>(),
        vec![
            ("reasoner", vec!["low", "high"], Some("high")),
            ("fast::v2", vec!["brief::soft"], Some("brief::soft")),
            ("plain", Vec::new(), None),
        ]
    );
    let routes = catalog_routes(definition, &configured, &configured.selection);
    assert_eq!(
        routes
            .iter()
            .map(|route| &*route.choice.route)
            .collect::<Vec<_>>(),
        [
            "custom::reasoner::low",
            "custom::reasoner::high",
            "custom::fast::v2::brief::soft",
            "custom::plain::default"
        ]
    );
    let target = default_model_route(definition, &configured, "fast::v2");
    assert_eq!(target, "custom::fast::v2::brief::soft");
    assert!(
        has_chat_route(definition, &configured, &configured.selection, &target),
        "membership accepts separators inside model and effort IDs"
    );
    assert!(
        !has_chat_route(
            definition,
            &configured,
            &configured.selection,
            "custom::fast::v2::brief"
        ),
        "a prefix is not a catalog route"
    );
    let retired = retired_model_routes(
        definition,
        &configured,
        &BTreeSet::from(["fast", "gone::v1"]),
    );
    assert!(
        !retired(&target),
        "retiring a shorter ID keeps the advertised model"
    );
    assert!(
        retired("custom::gone::v1::high"),
        "a saved route can outlive its catalog entry"
    );
    let builtin = provider("openai_socket").expect("builtin provider");
    configured.selection.provider = "openai_socket".into();
    configured.models.clear();
    let inherited = configured_models(builtin, &configured);
    assert!(!inherited.is_empty());
    assert_eq!(inherited, provider_status(builtin).models);
}
