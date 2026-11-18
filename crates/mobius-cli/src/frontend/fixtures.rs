//! Gateway catalog records shared by the frontend tests.

use std::sync::Arc;

use mobius::backend::session_files::session_file_limits;
use mobius_gateway::wire::{
    BotCapabilities, BotMemory, BotRecord, BotShape, ComputerView, ExtensionHookRecord,
    ExtensionKind, ExtensionRecord, HarnessConfig, HarnessKind, ProviderTint, ReadyPayload,
    RoutineInteractionPolicy, VersionedAgentConfig, VoiceCapability,
};

/// A gateway catalog holding `bots` and nothing else.
pub(super) fn ready(bots: Vec<BotRecord>) -> ReadyPayload {
    ReadyPayload {
        gateway_version: env!("CARGO_PKG_VERSION").into(),
        machine_name: "test".into(),
        computer_view: ComputerView::Unavailable,
        bots,
        sessions: Vec::new(),
        background_approvals: Vec::new(),
        providers: Vec::new(),
        provider_instances: Vec::new(),
        agent_catalog: Default::default(),
        harnesses: Vec::new(),
        max_active_sessions: 1,
        session_file_limits: session_file_limits(),
        revisions: Default::default(),
        omitted: Default::default(),
    }
}

/// A möbius Bot at revision 7 whose id, handle, and name are all `id`.
pub(super) fn bot(id: &str) -> BotRecord {
    BotRecord {
        id: id.into(),
        conversation_session_id: mobius::SessionId::new("bot-conversation-test").expect("session"),
        handle: id.into(),
        name: id.into(),
        description: "description".into(),
        tint: ProviderTint::Teal,
        shape: BotShape::Circle,
        harness: HarnessKind::Mobius,
        config: Arc::new(VersionedAgentConfig {
            revision: 7,
            config: HarnessConfig::Mobius(Arc::default()),
        }),
        capabilities: BotCapabilities {
            attachments: false,
            computer: false,
            voice: VoiceCapability::Off,
            memory: BotMemory {
                enabled: false,
                global: false,
            },
        },
        routine_interaction_policy: RoutineInteractionPolicy::Unattended,
        built_in: false,
    }
}

pub(super) fn extension(capability: &str) -> ExtensionRecord {
    ExtensionRecord {
        id: "plugin-a".into(),
        capability: capability.to_owned().into(),
        kind: ExtensionKind::Plugin,
        name: "Ponytail".into(),
        description: "Minimal solutions".into(),
        version: Some("4.9.0".into()),
        source: "https://github.com/example/ponytail".into(),
        reference: Some("main".into()),
        subdirectory: None,
        resolved_revision: "abc123".into(),
        digest: "sha256:trusted-snapshot".into(),
        skills: Arc::from(["ponytail".into()]),
        hooks: Arc::from([ExtensionHookRecord {
            event: "pre_tool".into(),
            matcher: Some("shell".into()),
            command: "review-hook --safe".into(),
            timeout_seconds: 10,
        }]),
        hooks_trusted: false,
    }
}
