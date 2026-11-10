//! Sandbox prompts, labels, and defaults loaded from `sandbox.toml`.

use std::sync::Arc;

use super::MAX_BACKGROUND_COMMANDS;
use super::MAX_TOOL_OUTPUT_BYTES;
use crate::middleware::manifest::MiddlewareSettingChoice;
use crate::middleware::manifest::MiddlewareSettingChoices;
use crate::middleware::manifest::MiddlewareSettingManifest;
use crate::protocol::FrontendTone;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Definition {
    pub(super) background_command_notice: String,
    pub(super) background_command_exited: String,
    pub(super) background_command_failed: String,
    pub(super) prompt_workspace: String,
    pub(super) prompt_attached_folders: String,
    pub(super) prompt_temporary_directory: String,
    pub(super) setting_tool_output_bytes_label: String,
    pub(super) setting_tool_output_bytes_description: String,
    pub(super) setting_background_commands_label: String,
    pub(super) setting_background_commands_description: String,
    pub(super) approval_policy_allow_description: String,
    pub(super) approval_policy_allow_label: String,
    pub(super) approval_policy_allow_network_description: String,
    pub(super) approval_policy_allow_network_label: String,
    pub(super) approval_policy_ask_description: String,
    pub(super) approval_policy_ask_label: String,
    pub(super) approval_policy_full_access_description: String,
    pub(super) approval_policy_full_access_label: String,
    pub(super) defaults_approval_policy: String,
    pub(super) manifest_description: String,
    pub(super) manifest_label: String,
    pub(super) prompt_linux: String,
    pub(super) prompt_macos: String,
    pub(super) prompt_other: String,
    pub(super) setting_approval_policy_description: String,
    pub(super) setting_approval_policy_label: String,
    pub(super) defaults_tool_output_bytes: usize,
    pub(super) defaults_background_commands: usize,
    pub(super) defaults_command_timeout_seconds: u64,
    pub(super) defaults_procfs_mode: super::ProcfsMode,
    pub(super) defaults_shell: String,
    pub(super) defaults_environment: Vec<String>,
    pub(super) setting_tool_output_bytes_step: i64,
    pub(super) setting_background_commands_step: i64,
    #[serde(skip)]
    pub(super) settings: Vec<MiddlewareSettingManifest>,
}
pub(super) static DEFINITION: std::sync::LazyLock<Definition> = std::sync::LazyLock::new(|| {
    let mut definition: Definition = crate::config::embedded(include_str!("sandbox.toml"));
    assert!((1..=MAX_TOOL_OUTPUT_BYTES).contains(&definition.defaults_tool_output_bytes));
    assert!((1..=MAX_BACKGROUND_COMMANDS).contains(&definition.defaults_background_commands));
    assert!(definition.defaults_command_timeout_seconds > 0);
    assert!(definition.setting_tool_output_bytes_step > 0);
    assert!(definition.setting_background_commands_step > 0);
    assert!(std::path::Path::new(&definition.defaults_shell).is_absolute());
    definition.settings = settings(&mut definition);
    definition
});

#[expect(
    clippy::expect_used,
    reason = "the safety bounds are small constants and the defaults are validated against them first"
)]
fn settings(definition: &mut Definition) -> Vec<MiddlewareSettingManifest> {
    use std::mem::take;
    vec![
        MiddlewareSettingManifest::Select {
            id: "approval_policy".into(),
            label: take(&mut definition.setting_approval_policy_label),
            description: take(&mut definition.setting_approval_policy_description),
            choices: MiddlewareSettingChoices::Static(vec![
                MiddlewareSettingChoice {
                    disables: Arc::default(),
                    value: "ask".into(),
                    label: take(&mut definition.approval_policy_ask_label).into(),
                    description: take(&mut definition.approval_policy_ask_description).into(),
                    symbol: Some("shield_check".into()),
                    tone: FrontendTone::Neutral,
                },
                MiddlewareSettingChoice {
                    disables: Arc::default(),
                    value: "allow".into(),
                    label: take(&mut definition.approval_policy_allow_label).into(),
                    description: take(&mut definition.approval_policy_allow_description).into(),
                    symbol: Some("shield".into()),
                    tone: FrontendTone::Warning,
                },
                MiddlewareSettingChoice {
                    disables: Arc::default(),
                    value: "allow_network".into(),
                    label: take(&mut definition.approval_policy_allow_network_label).into(),
                    description: take(&mut definition.approval_policy_allow_network_description)
                        .into(),
                    symbol: Some("shield_alert".into()),
                    tone: FrontendTone::Warning,
                },
                MiddlewareSettingChoice {
                    disables: Arc::default(),
                    value: "full_access".into(),
                    label: take(&mut definition.approval_policy_full_access_label).into(),
                    description: take(&mut definition.approval_policy_full_access_description)
                        .into(),
                    symbol: Some("shield_off".into()),
                    tone: FrontendTone::Error,
                },
            ]),
            unset_label: None,
            default: Some(take(&mut definition.defaults_approval_policy)),
            max_bytes: 32,
            composer: true,
        },
        MiddlewareSettingManifest::Integer {
            id: "tool_output_bytes".into(),
            label: take(&mut definition.setting_tool_output_bytes_label),
            description: take(&mut definition.setting_tool_output_bytes_description),
            min: 1,
            max: Some(i64::try_from(MAX_TOOL_OUTPUT_BYTES).expect("output safety bound must fit")),
            step: definition.setting_tool_output_bytes_step,
            default: i64::try_from(definition.defaults_tool_output_bytes)
                .expect("validated output default must fit"),
        },
        MiddlewareSettingManifest::Integer {
            id: "background_commands".into(),
            label: take(&mut definition.setting_background_commands_label),
            description: take(&mut definition.setting_background_commands_description),
            min: 1,
            max: Some(
                i64::try_from(MAX_BACKGROUND_COMMANDS).expect("command safety bound must fit"),
            ),
            step: definition.setting_background_commands_step,
            default: i64::try_from(definition.defaults_background_commands)
                .expect("validated command default must fit"),
        },
    ]
}
