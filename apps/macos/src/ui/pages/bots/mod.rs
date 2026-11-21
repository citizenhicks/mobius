//! Bots: the list, each Bot's overview and settings, routines and agent defaults (WP4).

mod auto_routing;
mod bots_ui;
mod capabilities;
mod capability_rows;
mod create;
mod events;
mod menu;
mod model;
mod overview;
mod pin;
mod routine_dialog;
mod routine_form;
mod routines;
mod runs;
mod schedule;
mod sections;
mod settings;
mod sidebar;
mod status;

pub(in crate::ui) use capabilities::extension_metadata;
pub(in crate::ui) use create::dialog_heading;
pub(in crate::ui) use settings::AgentScope;

pub(in crate::ui) use bots_ui::{BotsUi, Pending};

/// A saved Bot with recognizable non-default settings for editing tests.
#[cfg(test)]
pub(in crate::ui) fn test_composition(
    model: &str,
    effort: &str,
) -> mobius_gateway::wire::AgentComposition {
    serde_json::from_value(serde_json::json!({
        "provider": {
            "instance": "work", "provider": "openai", "model": model,
            "endpoint_auth": "provider_default", "reasoning_effort": effort, "web_search": "off"
        },
        "middleware": {"enabled": [], "settings": {}},
        "extensions": [], "system_prompt": "Stay focused.", "max_model_steps": 64
    }))
    .unwrap()
}
