use super::configured_model;
use super::route::with_route;
use mobius::protocol::{ModelChoice, ToolDiscoveryMode};
use mobius_gateway::wire::{AgentComposition, ReadyPayload};
use serde_json::json;

/// Two models of one provider instance, each at low and high reasoning.
fn catalog() -> ReadyPayload {
    let mut gateway: ReadyPayload = serde_json::from_value(json!({
        "gateway_version":"test", "machine_name":"test", "computer_view":"unavailable", "bots":[], "sessions":[],
        "background_approvals":[], "harnesses":[], "max_active_sessions":1, "revisions":{}, "omitted":[],
        "agent_catalog": {
            "bot_defaults":null, "middleware_features":[], "extensions":[], "contributions":[],
            "models":[], "model_providers":{}, "image_models":[], "voice_models":[]
        },
        "session_file_limits": {
            "max_attachment_references":1, "max_file_bytes":100, "max_session_files":1,
            "max_session_bytes":100, "max_upload_chunk_bytes":100
        },
        "provider_instances":[{
            "label":"Work", "tint":"blue", "configured":true,
            "models":[],
            "selection":{
                "instance":"work", "provider":"openai", "model":"sol",
                "base_url":null, "endpoint_auth":"provider_default",
                "reasoning_effort":"high", "web_search":"off"
            }
        }],
        "providers":[]
    }))
    .unwrap();
    for model in ["sol", "luna"] {
        for effort in ["low", "high"] {
            let route: std::sync::Arc<str> = format!("work/{model}/{effort}").into();
            gateway
                .agent_catalog
                .model_providers
                .insert(std::sync::Arc::clone(&route), "work".into());
            gateway.agent_catalog.models.push(ModelChoice {
                route,
                group: "Work".into(),
                model: model.into(),
                reasoning_effort: Some(effort.into()),
                context_window: None,
                supports_image_input: false,
                supports_image_generation: false,
                supports_realtime_voice: false,
                tool_discovery: ToolDiscoveryMode::Native,
                variant_label: None,
            });
        }
    }
    gateway
}

fn composition(gateway: &ReadyPayload) -> AgentComposition {
    serde_json::from_value(json!({
        "provider": gateway.provider_instances[0].selection,
        "middleware": {"enabled": [], "settings": {}},
        "extensions": [],
        "system_prompt": "Stay focused.",
        "max_model_steps": 64
    }))
    .unwrap()
}

#[test]
fn a_draft_model_change_moves_only_the_model_and_reasoning() {
    let gateway = catalog();
    let saved = composition(&gateway);
    assert_eq!(
        configured_model(&gateway, &saved.provider).map(|model| model.route.as_ref()),
        Some("work/sol/high")
    );
    let draft = with_route(&gateway, &saved, "work/luna/low").unwrap();
    assert_eq!(draft.provider.model, "luna");
    assert_eq!(draft.provider.reasoning_effort.as_deref(), Some("low"));
    assert_eq!(draft.provider.instance, "work");
    assert_eq!(
        (draft.system_prompt.as_str(), draft.max_model_steps),
        ("Stay focused.", 64)
    );
    assert_eq!(
        configured_model(&gateway, &draft.provider).map(|model| model.route.as_ref()),
        Some("work/luna/low"),
        "the picker shows the draft's choice as current"
    );
}
