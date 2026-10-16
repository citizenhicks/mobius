use mobius_gateway::wire::{ClientMessage, ProfileSnapshot, ProviderInstance, ServerMessage};
use uuid::Uuid;

use super::catalog::GatewayAction;
use super::provider_instance_label;

pub(super) type PreparedAction = Box<ClientMessage>;

pub(super) struct RenderedResponse {
    pub(super) text: String,
    pub(super) severity: ResponseSeverity,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ResponseSeverity {
    Neutral,
    Error,
    Fatal,
}

pub(super) fn prepare(action: GatewayAction, session_id: &str) -> PreparedAction {
    match action {
        GatewayAction::Pair => send(|request_id| ClientMessage::CreatePairingCode { request_id }),
        GatewayAction::Profile => send(|request_id| ClientMessage::GetProfile {
            request_id,
            include_provider_usage: true,
        }),
        GatewayAction::Rename(title) => send(|request_id| ClientMessage::RenameSession {
            request_id,
            session_id: session_id.into(),
            title,
        }),
        GatewayAction::SetPinned(pinned) => send(|request_id| ClientMessage::SetSessionPinned {
            request_id,
            session_id: session_id.into(),
            pinned,
        }),
        GatewayAction::Reassign(bot_id) => send(|request_id| ClientMessage::ReassignSession {
            request_id,
            session_id: session_id.into(),
            bot_id,
        }),
        GatewayAction::AttachFolder(folder) => {
            send(|request_id| ClientMessage::AttachSessionFolder {
                request_id,
                session_id: session_id.into(),
                folder,
            })
        }
        GatewayAction::DeleteCurrent => send(|request_id| ClientMessage::DeleteSessions {
            request_id,
            session_ids: vec![session_id.into()],
            selection: mobius::backend::session_files::SessionFileSelection::All,
        }),
        GatewayAction::ListSessionFiles => send(|request_id| ClientMessage::ListSessionFiles {
            request_id,
            session_id: session_id.into(),
        }),
        GatewayAction::DeleteStoredFile(file_id) => {
            send(|request_id| ClientMessage::DeleteSessions {
                request_id,
                session_ids: vec![session_id.into()],
                selection: mobius::backend::session_files::SessionFileSelection::Ids(vec![file_id]),
            })
        }
        GatewayAction::GitDiff(scope) => send(|request_id| ClientMessage::GetGitDiff {
            request_id,
            session_id: session_id.into(),
            scope,
        }),
        GatewayAction::SwitchBranch(branch) => send(|request_id| ClientMessage::SwitchGitBranch {
            request_id,
            session_id: session_id.into(),
            branch,
        }),
    }
}

pub(super) fn render_response(
    message: &ServerMessage,
    provider_instances: &[ProviderInstance],
) -> Option<RenderedResponse> {
    let response = |text, severity| Some(RenderedResponse { text, severity });
    match message {
        ServerMessage::Accepted { .. } => None,
        ServerMessage::Rejected { message, fatal, .. }
        | ServerMessage::Error { message, fatal, .. } => response(
            message.clone(),
            if *fatal {
                ResponseSeverity::Fatal
            } else {
                ResponseSeverity::Error
            },
        ),
        ServerMessage::ProviderCredentialSaved { provider, .. } => {
            response(format!("{provider}: configured"), ResponseSeverity::Neutral)
        }
        ServerMessage::PairingCode {
            code, expires_at, ..
        } => response(
            format!("one-time code {code} · expires {expires_at}"),
            ResponseSeverity::Neutral,
        ),
        ServerMessage::ProviderLoginStarted {
            provider,
            verification_url,
            user_code,
            ..
        } => response(
            format!("{provider} login · open {verification_url} · enter {user_code}"),
            ResponseSeverity::Neutral,
        ),
        ServerMessage::ProviderLoginFinished { provider, .. } => response(
            format!("{provider} login complete"),
            ResponseSeverity::Neutral,
        ),
        ServerMessage::Profile { profile, .. } => response(
            render_profile(profile, provider_instances),
            ResponseSeverity::Neutral,
        ),
        _ => None,
    }
}

fn request_id() -> String {
    Uuid::new_v4().to_string()
}

fn send(build: impl FnOnce(String) -> ClientMessage) -> PreparedAction {
    Box::new(build(request_id()))
}

fn render_profile(profile: &ProfileSnapshot, provider_instances: &[ProviderInstance]) -> String {
    let mut lines = vec![profile.user_name.as_deref().unwrap_or("user").into()];
    let runs = &profile.run_stats.completed;
    lines.push(format!(
        "runs {} · failed {} · aborted {} · model calls {} · tool calls {}",
        runs.run_count,
        runs.failed_run_count,
        runs.aborted_run_count,
        runs.model_calls,
        runs.tool_calls
    ));
    lines.extend(profile.daily_usage.iter().map(|day| {
        let provider =
            provider_instance_label(provider_instances, &day.provider).unwrap_or(&day.provider);
        format!(
            "day {} · {} · {} tokens · {} cached",
            day.unix_day, provider, day.usage.total_tokens, day.usage.cached_input_tokens
        )
    }));
    for usage in &profile.provider_usage {
        let provider =
            provider_instance_label(provider_instances, &usage.provider).unwrap_or(&usage.provider);
        if let Some(error) = &usage.error {
            lines.push(format!("{provider} · {error}"));
            continue;
        }
        let Some(limits) = &usage.limits else {
            lines.push(format!("{provider} · usage unavailable"));
            continue;
        };
        if limits.is_empty() {
            lines.push(format!("{provider} · no reported limits"));
        }
        lines.extend(limits.iter().map(|limit| {
            let remaining = (limit.remaining_fraction * 100.0).clamp(0.0, 100.0);
            let reset = limit
                .resets_at
                .map_or_else(String::new, |value| format!(" · resets {value}"));
            format!(
                "{provider} · {} · {remaining:.1}% remaining · {}s window{reset}",
                limit.label, limit.window_seconds
            )
        }));
    }
    lines.join("\n")
}
