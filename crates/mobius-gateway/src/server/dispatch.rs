use super::{
    AccessLease, Arc, AsyncWrite, AuthStore, BTreeMap, BTreeSet, BotStore, ClientCapabilities,
    ClientConnections, ClientMessage, ClientView, Error, GatewayHost, GatewayNotification,
    HostHandle, JoinSet, Op, PathBuf, PendingSessionFileWrite, Rejection, Result, ServerFrame,
    ServerMessage, SessionFileStore, SharedFrame, Utc, Uuid, broadcast, directory_rejection,
    list_directories, open_selected, require_selected, session_file_rejection,
    uploads_disabled_rejection, write_client_inventory, write_frame, write_gateway_result,
    write_rejection, write_result, write_server_error, write_session_history,
};
use crate::host::{event_sequence, internal, invalid_routine};
use mobius::SessionId;
use mobius::backend::session_files::session_file_limits;
use mobius::backend::session_files::{SessionFileOrigin as StoredFileOrigin, SessionFileSelection};
use mobius::protocol::{MessageAuthor, MessageTarget, Submission};
use std::future::Future;

use crate::wire::{GitDiffScope, ProfileParts, WorkspaceFileScope};

pub(super) struct SelectedChat {
    pub(super) host: HostHandle,
    /// Live events, unless the chat was selected only for requests.
    pub(super) broadcasts: Option<broadcast::Receiver<SharedFrame>>,
    pub(super) delivered_sequence: u64,
}

pub(super) struct AuthenticatedClient<'a> {
    pub(super) local: bool,
    pub(super) access_lease: Option<AccessLease>,
    pub(super) bots: &'a BotStore,
    pub(super) auth: &'a AuthStore,
    pub(super) desktop_transport: bool,
    pub(super) connection_id: Uuid,
    pub(super) client: ClientCapabilities,
    pub(super) id: &'a str,
    pub(super) connections: &'a ClientConnections,
    pub(super) revocations: &'a broadcast::Sender<String>,
}

pub(super) const MAX_PENDING_REQUESTS: usize = 4;

pub(super) struct ConnectionSessionState<'a> {
    pub(super) gateway: &'a Arc<GatewayHost>,
    pub(super) disabled_notifications: &'a mut BTreeSet<GatewayNotification>,
    pub(super) view: &'a mut ClientView,
    pub(super) selected: &'a mut Option<SelectedChat>,
    pub(super) requests: &'a mut JoinSet<ServerMessage>,
    pub(super) session_files: &'a SessionFileStore,
    pub(super) bots: &'a BotStore,
    pub(super) uploads: &'a mut BTreeMap<(SessionId, String), PendingSessionFileWrite>,
    pub(super) voice: &'a mut Option<super::voice::ConnectionVoice>,
    pub(super) browser: &'a mut Option<crate::computer_runtime::browser::BrowserConnection>,
    pub(super) desktop: &'a mut Option<crate::computer_runtime::desktop::DesktopConnection>,
    pub(super) remote_desktop:
        &'a mut Option<crate::computer_runtime::remote_desktop::DesktopStream>,
    pub(super) pending_desktop_control: &'a mut Option<super::desktop::PendingControl>,
    pub(super) pending_profile: &'a mut Option<super::transport::PendingProfile>,
    pub(super) queued_profile: &'a mut Option<(Arc<str>, ProfileParts)>,
    pub(super) readable: &'a mut Option<SessionId>,
}

pub(super) async fn selected_broadcast(
    selected: &mut Option<SelectedChat>,
) -> std::result::Result<SharedFrame, broadcast::error::RecvError> {
    let Some(SelectedChat {
        broadcasts: Some(broadcasts),
        delivered_sequence,
        ..
    }) = selected
    else {
        std::future::pending::<!>().await
    };
    loop {
        let frame = broadcasts.recv().await?;
        let sequence = event_sequence(&frame);
        if sequence.is_none_or(|value| value > *delivered_sequence) {
            *delivered_sequence = sequence.unwrap_or(*delivered_sequence);
            return Ok(frame);
        }
    }
}

/// Writes the frame `build` makes from a success, or the rejection.
async fn respond<T>(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    result: std::result::Result<T, Rejection>,
    build: impl FnOnce(Arc<str>, T) -> ServerMessage,
) -> Result<()> {
    match result {
        Ok(value) => write_frame(writer, &ServerFrame::new(build(request_id, value))).await,
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

/// Messages answered before pending Bot deletion recovery runs.
const fn answers_before_recovery(message: &ClientMessage) -> bool {
    matches!(
        message,
        ClientMessage::GetStorageUsage { .. }
            | ClientMessage::GetTelemetry { .. }
            | ClientMessage::ConfigureTelemetry { .. }
            | ClientMessage::GetProfile { .. }
            | ClientMessage::OpenComputer { .. }
            | ClientMessage::SetDesktopStream { .. }
            | ClientMessage::DesktopData { .. }
            | ClientMessage::SetDesktopControl { .. }
    )
}

fn memories(request_id: Arc<str>, memories: Vec<crate::wire::MemoryRecord>) -> ServerMessage {
    ServerMessage::Memories {
        request_id,
        memories,
    }
}

pub(super) async fn handle_message(
    message: ClientMessage,
    auth: &AuthStore,
    gateway: &Arc<GatewayHost>,
    bots: &BotStore,
    client: &AuthenticatedClient<'_>,
    mut connection: ConnectionSessionState<'_>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    let operator = client.local && crate::auth::is_local_operator(client.id);
    if !answers_before_recovery(&message) {
        if let Err(rejection) = gateway.reconcile_pending_bot_deletion().await {
            return write_server_error(writer, "bot_deletion_recovery", rejection.message, false)
                .await;
        }
        super::voice::end_on_switch(&message, connection.voice).await;
    }
    match message {
        ClientMessage::InvalidRequest {
            request_id,
            message,
        } => {
            return write_rejection(
                writer,
                request_id,
                Rejection::new("invalid_request", message),
            )
            .await;
        }
        ClientMessage::GetStorageUsage { request_id } => {
            return storage_usage(writer, request_id, gateway).await;
        }
        ClientMessage::GetTelemetry { request_id } => {
            return write_telemetry(writer, request_id, gateway).await;
        }
        ClientMessage::ConfigureTelemetry {
            request_id,
            expected_revision,
            sinks,
            preserve_auth,
        } => {
            let update = TelemetryUpdate {
                expected_revision,
                sinks,
                preserve_auth,
            };
            return configure_telemetry(writer, request_id, update, operator, gateway).await;
        }
        ClientMessage::GetProfile { request_id, parts } => {
            return super::transport::request_profile(
                request_id,
                parts,
                gateway,
                connection.pending_profile,
                connection.queued_profile,
                writer,
            )
            .await;
        }
        ClientMessage::SetNotifications {
            request_id,
            disabled,
        } => {
            *connection.disabled_notifications = disabled;
            return write_result(writer, request_id, Ok(())).await;
        }
        ClientMessage::SelectSession {
            request_id,
            session_id,
        } => {
            return select_session(writer, connection.selected, request_id, session_id, gateway)
                .await;
        }
        ClientMessage::GetGitDiff {
            request_id,
            session_id,
            scope,
            totals,
        } => {
            return git_diff(
                writer,
                &mut connection,
                request_id,
                session_id,
                scope,
                totals,
            )
            .await;
        }
        ClientMessage::Pair { .. }
        | ClientMessage::RepairPairing { .. }
        | ClientMessage::Authenticate { .. } => {
            return write_server_error(
                writer,
                "already_authenticated",
                "this connection is already authenticated",
                false,
            )
            .await;
        }
        ClientMessage::ListClients { request_id } => {
            return write_client_inventory(writer, request_id, client.id, auth, client.connections)
                .await;
        }
        ClientMessage::UnpairClient {
            request_id,
            client_id,
        } => return unpair_client(writer, request_id, client_id, auth, client, gateway).await,
        ClientMessage::ListSessions { request_id } => {
            return list_sessions(writer, connection.view, request_id, gateway).await;
        }
        ClientMessage::CreateSession {
            request_id,
            workspace,
            bot_id,
            auto,
            prompt,
        } => {
            return create_session(
                writer,
                connection.selected,
                request_id,
                crate::host::NewChat {
                    workspace,
                    bot_id,
                    auto,
                    prompt,
                },
                gateway,
            )
            .await;
        }
        ClientMessage::CreateWorkspaceDirectory {
            request_id,
            parent,
            name,
        } => {
            return create_workspace_directory(writer, request_id, parent, name, gateway).await;
        }
        ClientMessage::OpenSession {
            request_id,
            session_id,
            last_sequence,
        } => {
            return open_session(
                writer,
                connection.selected,
                request_id,
                session_id,
                last_sequence,
                gateway,
            )
            .await;
        }
        ClientMessage::GetSessionHistory {
            request_id,
            session_id,
            before,
        } => {
            return session_history(
                writer,
                connection.selected.as_ref(),
                gateway,
                request_id,
                session_id,
                before,
            )
            .await;
        }
        ClientMessage::ReassignSession {
            request_id,
            session_id,
            bot_id,
        } => {
            return write_result(
                writer,
                request_id,
                gateway.reassign_session(&session_id, &bot_id).await,
            )
            .await;
        }
        ClientMessage::RenameSession {
            request_id,
            session_id,
            title,
        } => {
            let renamed = gateway.rename_session(&session_id, &title);
            return answer(writer, request_id, renamed).await;
        }
        ClientMessage::AttachSessionFolder {
            request_id,
            session_id,
            folder,
        } => {
            return attach_session_folder(
                writer,
                connection.selected.as_ref(),
                request_id,
                session_id,
                folder,
            )
            .await;
        }
        ClientMessage::SetSessionPinned {
            request_id,
            session_id,
            pinned,
        } => {
            let pinned = gateway.set_session_pinned(&session_id, pinned);
            return answer(writer, request_id, pinned).await;
        }
        ClientMessage::SetMessagePinned {
            request_id,
            session_id,
            target,
            pinned,
        } => {
            return set_message_pinned(
                writer,
                connection.selected.as_ref(),
                request_id,
                session_id,
                target,
                pinned,
            )
            .await;
        }
        ClientMessage::DeleteSessions {
            request_id,
            session_ids,
            selection,
        } => {
            return delete_sessions(
                writer,
                &mut connection,
                request_id,
                session_ids,
                selection,
                gateway,
            )
            .await;
        }
        ClientMessage::Submit {
            session_id,
            submission,
        } => {
            return submit(
                writer,
                connection.selected.as_ref(),
                connection.session_files,
                session_id,
                submission,
            )
            .await;
        }
        ClientMessage::GetContributions { request_id } => {
            let contributions = gateway.contributions().await;
            return contribution_response(writer, connection.view, request_id, contributions).await;
        }
        ClientMessage::SubmitContribution {
            request_id,
            operation,
        } => {
            let contributions = gateway.submit_contribution(operation).await;
            return contribution_response(writer, connection.view, request_id, contributions).await;
        }
        ClientMessage::BeginSessionFileUpload {
            request_id,
            session_id,
            name,
            size,
            media_type,
        } => {
            return begin_session_file_upload(
                writer,
                &mut connection,
                request_id,
                session_id,
                name,
                size,
                media_type,
            )
            .await;
        }
        ClientMessage::UploadSessionFileChunk {
            request_id,
            session_id,
            upload_id,
            offset,
            data,
        } => {
            return update_session_file_upload(
                writer,
                &mut connection,
                request_id,
                session_id,
                upload_id,
                UploadStep::Chunk { offset, data },
            )
            .await;
        }
        ClientMessage::FinishSessionFileUpload {
            request_id,
            session_id,
            upload_id,
        } => {
            return update_session_file_upload(
                writer,
                &mut connection,
                request_id,
                session_id,
                upload_id,
                UploadStep::Finish,
            )
            .await;
        }
        ClientMessage::ListSessionFiles {
            request_id,
            session_id,
        } => {
            return list_session_files(writer, &mut connection, request_id, session_id).await;
        }
        ClientMessage::ReadSessionFile {
            request_id,
            session_id,
            file_id,
            offset,
            max_bytes,
        } => {
            let read = SessionFileRead {
                file_id,
                offset,
                max_bytes,
            };
            return read_session_file(writer, &mut connection, request_id, session_id, read).await;
        }
        ClientMessage::CreateBot {
            request_id,
            harness,
            name,
            description,
        } => {
            let created = gateway.create_bot(harness, &name, &description).await;
            return write_bot_result(writer, connection.view, request_id, created, gateway).await;
        }
        ClientMessage::ListBots { request_id } => {
            return write_bot_result(writer, connection.view, request_id, Ok(()), gateway).await;
        }
        ClientMessage::UpdateBot {
            request_id,
            id,
            expected_revision,
            name,
            description,
            tint,
            shape,
            config,
        } => {
            let updated = gateway
                .update_bot(
                    &id,
                    expected_revision,
                    crate::bots::BotIdentity {
                        name: &name,
                        description: &description,
                        tint,
                        shape,
                    },
                    config,
                )
                .await;
            return write_bot_result(writer, connection.view, request_id, updated, gateway).await;
        }
        ClientMessage::DeleteBot {
            request_id,
            id,
            expected_revision,
        } => {
            return write_bot_catalog_result(
                writer,
                &mut connection,
                request_id,
                gateway.delete_bot(&id, expected_revision).await,
            )
            .await;
        }
        ClientMessage::ConfigureBotDefaults {
            request_id,
            expected_revision,
            config,
        } => {
            let configured = gateway
                .configure_bot_defaults(expected_revision, config)
                .await;
            return write_gateway_result(writer, connection.view, request_id, configured).await;
        }
        ClientMessage::ConfigureAutoRouting { request_id, values } => {
            return configure_auto_routing(writer, connection.view, request_id, values, gateway)
                .await;
        }
        ClientMessage::SwitchBotModels {
            request_id,
            instance,
            from,
            to,
        } => {
            return switch_bot_models(writer, gateway, request_id, &instance, &from, &to).await;
        }
        ClientMessage::ConfigureHarness {
            request_id,
            kind,
            cli_path,
            defaults,
        } => {
            let configured = gateway.configure_harness(kind, cli_path, defaults);
            return answer_gateway(writer, connection.view, request_id, configured).await;
        }
        ClientMessage::RefreshHarnesses { request_id } => {
            let refreshed = gateway.refresh_harnesses();
            return answer_gateway(writer, connection.view, request_id, refreshed).await;
        }
        ClientMessage::SetSessionMemory {
            request_id,
            session_id,
            memory,
        } => {
            return set_session_memory(
                writer,
                connection.selected.as_ref(),
                request_id,
                session_id,
                memory,
            )
            .await;
        }
        ClientMessage::ListMemories { request_id } => {
            return answer_memories(writer, request_id, gateway.list_memories()).await;
        }
        ClientMessage::EditMemory {
            request_id,
            id,
            description,
            text,
        } => {
            let edited = gateway.edit_memory(&id, &description, &text);
            return answer_memories(writer, request_id, edited).await;
        }
        ClientMessage::ForgetMemory { request_id, id } => {
            return answer_memories(writer, request_id, gateway.forget_memory(&id)).await;
        }
        ClientMessage::InstallExtension {
            request_id,
            source,
            reference,
            subdirectory,
        } => {
            let installed = gateway
                .install_extension(source, reference, subdirectory)
                .await;
            return write_gateway_result(writer, connection.view, request_id, installed).await;
        }
        ClientMessage::UpdateExtension { request_id, id } => {
            let updated = gateway.update_extension(id).await;
            return write_gateway_result(writer, connection.view, request_id, updated).await;
        }
        ClientMessage::UninstallExtension { request_id, id } => {
            let uninstalled = gateway.uninstall_extension(id).await;
            return write_gateway_result(writer, connection.view, request_id, uninstalled).await;
        }
        ClientMessage::TrustExtensionHooks {
            request_id,
            id,
            expected_digest,
            trusted,
        } => {
            let changed = gateway
                .set_extension_hooks_trusted(id, expected_digest, trusted)
                .await;
            return write_gateway_result(writer, connection.view, request_id, changed).await;
        }
        ClientMessage::ProbeGitCredential { request_id, target } => {
            return probe_git_credential(writer, request_id, &target).await;
        }
        ClientMessage::ApproveGitCredential {
            request_id,
            target,
            username,
            token,
        } => {
            return approve_git_credential(writer, request_id, &target, &username, &token).await;
        }
        ClientMessage::ListSshIdentities { request_id } => {
            return list_ssh_identities(writer, request_id).await;
        }
        ClientMessage::GenerateSshIdentity { request_id } => {
            return generate_ssh_identity(writer, request_id).await;
        }
        ClientMessage::SwitchGitBranch {
            request_id,
            session_id,
            branch,
        } => {
            return switch_git_branch(
                writer,
                connection.selected.as_ref(),
                request_id,
                session_id,
                branch,
            )
            .await;
        }
        ClientMessage::ListWorkspaceFiles {
            request_id,
            session_id,
            scope,
        } => {
            return list_workspace_files(
                writer,
                connection.selected.as_ref(),
                request_id,
                session_id,
                scope,
            )
            .await;
        }
        ClientMessage::ReadWorkspaceFile {
            request_id,
            session_id,
            path,
            offset,
            max_bytes,
        } => {
            return read_workspace_file(
                writer,
                connection.selected.as_ref(),
                request_id,
                session_id,
                path,
                offset,
                max_bytes,
            )
            .await;
        }
        ClientMessage::DeleteWorkspaceFile {
            request_id,
            session_id,
            path,
        } => {
            let host = match require_selected(connection.selected.as_ref(), &session_id) {
                Ok(host) => host,
                Err(rejection) => return write_rejection(writer, request_id, rejection).await,
            };
            return write_result(writer, request_id, host.delete_workspace_file(path).await).await;
        }
        ClientMessage::WriteWorkspaceFile {
            request_id,
            session_id,
            path,
            content,
        } => {
            return write_workspace_file(
                writer,
                connection.selected.as_ref(),
                request_id,
                session_id,
                path,
                content,
            )
            .await;
        }
        ClientMessage::ListDirectories {
            request_id,
            path,
            include_files,
        } => {
            return list_directories_response(writer, request_id, path, include_files).await;
        }
        ClientMessage::ClearProviderCredential {
            request_id,
            instance,
        } => {
            return clear_provider_credential(writer, request_id, instance, gateway).await;
        }
        ClientMessage::SetProviderCredential {
            request_id,
            instance,
            provider,
            base_url,
            api_key,
            expires_at,
        } => {
            return set_provider_credential(
                writer, request_id, instance, provider, api_key, base_url, expires_at, gateway,
            )
            .await;
        }
        ClientMessage::RegisterProvider {
            request_id,
            config,
            preserve_selection,
            if_configured,
            label,
            tint,
            models,
            image_model_ids,
            decision_model_ids,
            operator_managed,
        } => {
            let registered = gateway
                .register_provider(
                    operator,
                    crate::config::ProviderRegistration {
                        selection: config,
                        label,
                        tint,
                        models,
                        image_model_ids,
                        decision_model_ids,
                        operator_managed,
                    },
                    preserve_selection,
                    if_configured,
                )
                .await;
            return write_gateway_result(writer, connection.view, request_id, registered).await;
        }
        ClientMessage::RemoveProvider {
            request_id,
            instance,
        } => {
            let removed = gateway.remove_provider(instance).await;
            return write_gateway_result(writer, connection.view, request_id, removed).await;
        }
        ClientMessage::CreatePairingCode { request_id } => {
            return create_pairing_code(writer, request_id, auth).await;
        }
        ClientMessage::StartProviderLogin {
            request_id,
            provider,
        } => {
            return start_provider_login(writer, request_id, provider, client.id, auth, gateway)
                .await;
        }
        ClientMessage::CreateRoutine {
            request_id,
            bot_id,
            definition,
        } => {
            return write_result(
                writer,
                request_id,
                gateway
                    .create_routine(&bot_id, &definition, None)
                    .await
                    .map(|_| ()),
            )
            .await;
        }
        ClientMessage::ListRoutines { request_id, bot_id } => {
            return list_routines(writer, request_id, bot_id, bots).await;
        }
        ClientMessage::RoutineCommand {
            request_id,
            command,
        } => {
            let result = gateway
                .execute_routine_command(&command, None, None, &request_id)
                .await;
            return write_result(writer, request_id, result).await;
        }
        ClientMessage::EmitHook {
            request_id,
            bot_id,
            name,
            data,
        } => {
            return emit_hook(writer, request_id, &bot_id, &name, data, gateway).await;
        }
        ClientMessage::ListRoutineHistory { request_id, id } => {
            return list_routine_history(writer, request_id, id, bots).await;
        }
        ClientMessage::DeleteRoutineRun { request_id, id } => {
            return write_result(writer, request_id, gateway.delete_routine_run(&id).await).await;
        }
        ClientMessage::GetRoutineRunPreview {
            request_id,
            id,
            before,
        } => {
            routine_run_preview(writer, request_id, &id, before, gateway).await?;
        }
        ClientMessage::SetBrowserRuntime {
            request_id,
            enabled,
        } => {
            return crate::computer_runtime::browser::set_runtime(
                crate::computer_runtime::browser::Registration {
                    request_id,
                    enabled,
                    local: client.local,
                    client: client.client,
                    available: gateway.remote_desktop.browser_available(),
                },
                &gateway.remote_desktop.browser,
                connection.browser,
                writer,
            )
            .await;
        }
        ClientMessage::BrowserPageReply {
            request_id,
            endpoint,
        } => {
            let browser = connection.browser.as_ref();
            return crate::computer_runtime::browser::reply(request_id, endpoint, browser, writer)
                .await;
        }
        ClientMessage::SetDesktopRuntime {
            request_id,
            enabled,
        } => {
            return crate::computer_runtime::desktop::set_runtime(
                request_id,
                enabled,
                &gateway.desktop,
                connection.desktop,
                client.local,
                client.client,
                writer,
            )
            .await;
        }
        ClientMessage::DesktopControlReply {
            request_id,
            response,
        } => {
            let desktop = connection.desktop.as_ref();
            return crate::computer_runtime::desktop::reply(request_id, response, desktop, writer)
                .await;
        }
        ClientMessage::OpenComputer {
            request_id,
            session_id,
        } => {
            return super::desktop::open_computer(
                request_id,
                session_id,
                gateway,
                client,
                &mut connection,
                writer,
            )
            .await;
        }
        ClientMessage::SetDesktopStream {
            request_id,
            enabled,
        } => {
            return super::desktop::set_stream(
                request_id,
                enabled,
                gateway,
                client,
                &mut connection,
                writer,
            )
            .await;
        }
        ClientMessage::DesktopData { data } => {
            return super::desktop::forward_data(&data, client, &mut connection, writer).await;
        }
        ClientMessage::SetDesktopControl {
            request_id,
            enabled,
            session_id,
        } => {
            return super::desktop::set_control(
                request_id,
                enabled,
                session_id,
                gateway,
                client,
                &mut connection,
                writer,
            )
            .await;
        }
        ClientMessage::StartRealtimeVoice {
            request_id,
            session_id,
            offer_sdp,
        } => {
            return super::voice::start_call(
                request_id,
                session_id,
                offer_sdp,
                &mut connection,
                writer,
            )
            .await;
        }
        ClientMessage::EndRealtimeVoice {
            session_id,
            voice_id,
        } => {
            return super::voice::end_call(session_id, voice_id, connection.voice, writer).await;
        }
        ClientMessage::ListBotSessions { request_id, bot_id } => {
            return list_bot_sessions(writer, request_id, bot_id, gateway).await;
        }
    }
    Ok(())
}

async fn start_provider_login(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    provider: String,
    client_id: &str,
    auth: &AuthStore,
    gateway: &GatewayHost,
) -> Result<()> {
    let result = gateway
        .start_provider_login(Arc::clone(&request_id), provider, client_id)
        .await;
    // Unpairing may have removed replay before this already-selected request
    // reserved its slot. Check after reservation to close that ordering too.
    if !auth.clients()?.iter().any(|paired| paired.id == client_id) {
        gateway
            .forget_provider_login(client_id)
            .await
            .map_err(|rejection| Error::Protocol(rejection.message.to_string()))?;
        return Err(Error::Unauthorized);
    }
    match result {
        Ok(Some(message)) => write_frame(writer, &ServerFrame::new(message)).await,
        result => write_result(writer, request_id, result.map(|_| ())).await,
    }
}

async fn unpair_client(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    client_id: String,
    auth: &AuthStore,
    client: &AuthenticatedClient<'_>,
    gateway: &GatewayHost,
) -> Result<()> {
    let publication = match auth.unpair_client(client.id, &client_id) {
        Ok(false) => {
            return write_rejection(
                writer,
                request_id,
                Rejection::new(
                    "unpair_rejected",
                    "that paired device cannot be unpaired from this connection",
                ),
            )
            .await;
        }
        Err(error) if !matches!(error, Error::PublicationApplied { .. }) => {
            return write_rejection(
                writer,
                request_id,
                internal("failed to update paired devices"),
            )
            .await;
        }
        result => result.map(|_| ()),
    };
    // Broadcast retains identity independently of cleanup; visible revocation must precede its await.
    let revoked = client_id.clone(); // rules-allow: the broadcast owns the revoked ID while cleanup still uses it
    if client.revocations.send(revoked).is_err() {
        tracing::debug!("no connections were listening for the revocation");
    }
    let follow_up = gateway.forget_provider_login(&client_id).await;
    match crate::host::finish_publication(publication, follow_up) {
        Ok(()) => {
            write_client_inventory(writer, request_id, client.id, auth, client.connections).await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn list_sessions(
    writer: &mut (impl AsyncWrite + Unpin),
    view: &mut ClientView,
    request_id: Arc<str>,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway.sessions().await {
        Ok(sessions) => {
            view.write_sessions(writer, Some(request_id), sessions)
                .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn list_bot_sessions(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    bot_id: Arc<str>,
    gateway: &GatewayHost,
) -> Result<()> {
    let sessions = gateway.hidden_bot_sessions(&bot_id).await;
    respond(writer, request_id, sessions, |request_id, sessions| {
        ServerMessage::BotSessions {
            request_id,
            bot_id,
            sessions,
        }
    })
    .await
}

async fn create_session(
    writer: &mut (impl AsyncWrite + Unpin),
    selected: &mut Option<SelectedChat>,
    request_id: Arc<str>,
    chat: crate::host::NewChat,
    gateway: &GatewayHost,
) -> Result<()> {
    let host = gateway.create_routed_session(chat).await;
    open_selected(writer, selected, request_id, host, None).await
}

async fn create_workspace_directory(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    parent: PathBuf,
    name: String,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway.create_workspace_directory(&parent, &name).await {
        Ok(path) => list_directories_response(writer, request_id, path, false).await,
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn open_session(
    writer: &mut (impl AsyncWrite + Unpin),
    selected: &mut Option<SelectedChat>,
    request_id: Arc<str>,
    session_id: SessionId,
    last_sequence: Option<u64>,
    gateway: &GatewayHost,
) -> Result<()> {
    let host = gateway.open_session(&session_id).await;
    open_selected(writer, selected, request_id, host, last_sequence).await
}

/// Selects a chat for requests only: no replay, no live events.
async fn select_session(
    writer: &mut (impl AsyncWrite + Unpin),
    selected: &mut Option<SelectedChat>,
    request_id: Arc<str>,
    session_id: SessionId,
    gateway: &GatewayHost,
) -> Result<()> {
    let opened = match gateway.open_session(&session_id).await {
        Ok(host) => host.ready().await.map(|payload| (host, payload)),
        Err(rejection) => Err(rejection),
    };
    let (host, payload) = match opened {
        Ok(opened) => opened,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    let delivered_sequence = payload.latest_sequence;
    write_frame(
        writer,
        &ServerFrame::new(ServerMessage::SessionOpened {
            request_id,
            payload,
        }),
    )
    .await?;
    *selected = Some(SelectedChat {
        host,
        broadcasts: None,
        delivered_sequence,
    });
    Ok(())
}

async fn session_history(
    writer: &mut (impl AsyncWrite + Unpin),
    selected: Option<&SelectedChat>,
    gateway: &GatewayHost,
    request_id: Arc<str>,
    session_id: SessionId,
    before: Option<crate::wire::HistoryCursor>,
) -> Result<()> {
    let page = match selected {
        Some(selected) if **selected.host.session_id() == *session_id => {
            selected.host.history_page(before).await
        }
        _ => gateway.read_session_history(&session_id, before).await,
    };
    write_session_history(writer, page, request_id, session_id).await
}

async fn on_selected<'a, T, F>(
    selected: Option<&'a SelectedChat>,
    session_id: &SessionId,
    operation: impl FnOnce(&'a HostHandle) -> F,
) -> std::result::Result<T, Rejection>
where
    F: Future<Output = std::result::Result<T, Rejection>>,
{
    operation(require_selected(selected, session_id)?).await
}

async fn set_session_memory(
    writer: &mut (impl AsyncWrite + Unpin),
    selected: Option<&SelectedChat>,
    request_id: Arc<str>,
    session_id: SessionId,
    memory: Option<bool>,
) -> Result<()> {
    let result = on_selected(selected, &session_id, |host| host.set_memory(memory)).await;
    write_result(writer, request_id, result).await
}

async fn attach_session_folder(
    writer: &mut (impl AsyncWrite + Unpin),
    selected: Option<&SelectedChat>,
    request_id: Arc<str>,
    session_id: SessionId,
    folder: PathBuf,
) -> Result<()> {
    let result = on_selected(selected, &session_id, |host| host.attach_folder(folder)).await;
    write_result(writer, request_id, result).await
}

async fn set_message_pinned(
    writer: &mut (impl AsyncWrite + Unpin),
    selected: Option<&SelectedChat>,
    request_id: Arc<str>,
    session_id: SessionId,
    target: MessageTarget,
    pinned: bool,
) -> Result<()> {
    let result = on_selected(selected, &session_id, |host| {
        host.set_message_pinned(target, pinned)
    })
    .await;
    write_result(writer, request_id, result).await
}

/// Answers a Bot request with the current Bot catalog.
async fn write_bot_result<T>(
    writer: &mut (impl AsyncWrite + Unpin),
    view: &mut ClientView,
    request_id: Arc<str>,
    result: std::result::Result<T, Rejection>,
    gateway: &GatewayHost,
) -> Result<()> {
    let bots = match result {
        Ok(_) => gateway.bots().await,
        Err(rejection) => Err(rejection),
    };
    match bots {
        Ok(bots) => view.write_bots(writer, Some(request_id), bots).await,
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn write_bot_catalog_result(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: Arc<str>,
    result: std::result::Result<(Vec<crate::wire::BotRecord>, Vec<SessionId>), Rejection>,
) -> Result<()> {
    match result {
        Ok((bots, deleted_sessions)) => {
            forget_deleted_sessions(connection, &deleted_sessions);
            connection
                .view
                .write_bots(writer, Some(request_id), bots)
                .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn delete_sessions(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: Arc<str>,
    session_ids: Vec<SessionId>,
    mut selection: SessionFileSelection,
    gateway: &GatewayHost,
) -> Result<()> {
    if let SessionFileSelection::Ids(ids) = &mut selection
        && let [selected_id] = session_ids.as_slice()
        && !ids.is_empty()
    {
        // Keep the checked session identity while moving each file ID through the lookup key.
        let mut key = (selected_id.share(), String::new());
        ids.retain_mut(|id| {
            std::mem::swap(id, &mut key.1);
            let retained = connection.uploads.remove(&key).is_none();
            std::mem::swap(id, &mut key.1);
            retained
        });
        if ids.is_empty() {
            return write_result(writer, request_id, Ok(())).await;
        }
    }
    let all = selection == SessionFileSelection::All;
    match gateway.delete_sessions(&session_ids, selection).await {
        Ok(deleted) => {
            if all {
                forget_deleted_sessions(connection, &deleted);
            }
            write_result(writer, request_id, Ok(())).await
        }
        Err(rejection) => write_result(writer, request_id, Err(rejection)).await,
    }
}

fn forget_deleted_sessions(connection: &mut ConnectionSessionState<'_>, deleted: &[SessionId]) {
    if connection
        .voice
        .as_ref()
        .is_some_and(|voice| deleted.iter().any(|id| id == &voice.session_id))
    {
        *connection.voice = None;
    }
    connection
        .uploads
        .retain(|(session_id, _), _| !deleted.contains(session_id));
    if connection.selected.as_ref().is_some_and(|selected| {
        deleted
            .iter()
            .any(|session_id| session_id == selected.host.session_id())
    }) {
        *connection.selected = None;
    }
}

async fn submit(
    writer: &mut (impl AsyncWrite + Unpin),
    selected: Option<&SelectedChat>,
    session_files: &SessionFileStore,
    session_id: SessionId,
    submission: Submission,
) -> Result<()> {
    let request_id = Arc::clone(&submission.id);
    let result = on_selected(selected, &session_id, |host| async {
        let submission = crate::host::validated(submission)?;
        if let Op::Message { message } = &submission.submission().op {
            match &message.author {
                MessageAuthor::Source { .. } => {
                    return Err(Rejection::new(
                        "invalid_submission",
                        "peer messages are gateway-owned",
                    ));
                }
                MessageAuthor::User => {}
            }
            if !message.attachments.is_empty() {
                if !host.accepts_file_attachments().await? {
                    return Err(uploads_disabled_rejection());
                }
                for reference in &message.attachments {
                    session_files
                        .verify_upload(&session_id, reference)
                        .await
                        .map_err(session_file_rejection)?;
                }
            }
        }
        host.submit_validated(submission).await
    })
    .await;
    write_result(writer, request_id, result).await
}

async fn contribution_response(
    writer: &mut (impl AsyncWrite + Unpin),
    view: &mut ClientView,
    request_id: Arc<str>,
    result: std::result::Result<Vec<mobius::protocol::FrontendContribution>, Rejection>,
) -> Result<()> {
    if result.is_ok() {
        view.forget(crate::wire::ReadySection::Config);
    }
    respond(writer, request_id, result, |request_id, contributions| {
        ServerMessage::Contributions {
            request_id,
            contributions,
        }
    })
    .await
}

async fn begin_upload_mutation(
    selected: Option<&SelectedChat>,
    bots: &BotStore,
    session_id: &SessionId,
) -> std::result::Result<tokio::sync::OwnedRwLockReadGuard<()>, Rejection> {
    let host = require_selected(selected, session_id)?;
    if !host.accepts_file_attachments().await? {
        return Err(uploads_disabled_rejection());
    }
    host.begin_session_file_mutation(bots)
}

async fn begin_session_file_upload(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: Arc<str>,
    session_id: SessionId,
    name: String,
    size: u64,
    media_type: String,
) -> Result<()> {
    let _mutation =
        match begin_upload_mutation(connection.selected.as_ref(), connection.bots, &session_id)
            .await
        {
            Ok(mutation) => mutation,
            Err(rejection) => return write_rejection(writer, request_id, rejection).await,
        };
    let capacity = connection.gateway.pending_upload_capacity().await?;
    if connection.uploads.len() >= capacity {
        return write_rejection(
            writer,
            request_id,
            session_file_rejection(mobius::Error::Tool(format!(
                "a connection cannot hold more than {capacity} pending uploads"
            ))),
        )
        .await;
    }
    match connection
        .session_files
        .begin_upload(&session_id, name, size, media_type)
        .await
    {
        Ok(upload) => {
            if let Err(rejection) =
                crate::telemetry::Telemetry::admit_upload(connection.gateway, upload.id(), size)
                    .await
            {
                drop(upload);
                return write_rejection(writer, request_id, rejection).await;
            }
            let upload_id = upload.id().to_owned();
            connection
                .uploads
                .insert((session_id.share(), upload_id.clone()), upload); // rules-allow: the upload table and ready frame each own the upload ID
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::SessionFileUploadReady {
                    request_id,
                    session_id,
                    upload_id,
                    max_chunk_bytes: session_file_limits().max_upload_chunk_bytes,
                }),
            )
            .await
        }
        Err(error) => write_rejection(writer, request_id, session_file_rejection(error)).await,
    }
}

enum UploadStep {
    Chunk { offset: u64, data: Vec<u8> },
    Finish,
}

async fn update_session_file_upload(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: Arc<str>,
    session_id: SessionId,
    upload_id: String,
    step: UploadStep,
) -> Result<()> {
    let key = (session_id, upload_id);
    let _mutation =
        match begin_upload_mutation(connection.selected.as_ref(), connection.bots, &key.0).await {
            Ok(mutation) => mutation,
            Err(rejection) => {
                connection.uploads.remove(&key);
                return write_rejection(writer, request_id, rejection).await;
            }
        };
    let inactive = || mobius::Error::Tool("session file upload is not active".into());
    let reply = match step {
        UploadStep::Chunk { offset, data } => {
            let result = match connection.uploads.get_mut(&key) {
                Some(upload) => upload.append(offset, &data).await,
                None => Err(inactive()),
            };
            match result {
                Ok(next_offset) => ServerMessage::SessionFileUploadChunkAccepted {
                    request_id,
                    session_id: key.0,
                    upload_id: key.1,
                    next_offset,
                },
                Err(error) => {
                    connection.uploads.remove(&key);
                    return write_rejection(writer, request_id, session_file_rejection(error))
                        .await;
                }
            }
        }
        UploadStep::Finish => {
            let result = match connection.uploads.remove(&key) {
                Some(upload) => upload.finish().await,
                None => Err(inactive()),
            };
            match result {
                Ok(file) => {
                    connection.gateway.invalidate_storage_usage();
                    ServerMessage::SessionFileUploadCompleted {
                        request_id,
                        session_id: key.0,
                        file,
                    }
                }
                Err(error) => {
                    return write_rejection(writer, request_id, session_file_rejection(error))
                        .await;
                }
            }
        }
    };
    write_frame(writer, &ServerFrame::new(reply)).await
}

async fn authorize_stored_read<'a>(
    connection: &mut ConnectionSessionState<'a>,
    session_id: &SessionId,
) -> std::result::Result<&'a SessionFileStore, Rejection> {
    let selected = connection
        .selected
        .as_ref()
        .is_some_and(|selected| **selected.host.session_id() == **session_id);
    if selected || connection.readable.as_ref() == Some(session_id) {
        return Ok(connection.session_files);
    }
    connection.gateway.authorize_read(session_id).await?;
    *connection.readable = Some(session_id.share());
    Ok(connection.session_files)
}

async fn configure_auto_routing(
    writer: &mut (impl AsyncWrite + Unpin),
    view: &mut ClientView,
    request_id: Arc<str>,
    values: crate::config::auto_routing::Values,
    gateway: &GatewayHost,
) -> Result<()> {
    let configured = gateway.configure_auto_routing(values).await;
    write_gateway_result(writer, view, request_id, configured).await
}

async fn switch_bot_models(
    writer: &mut (impl AsyncWrite + Unpin),
    gateway: &GatewayHost,
    request_id: Arc<str>,
    instance: &str,
    from: &[String],
    to: &str,
) -> Result<()> {
    let switched = gateway.switch_bot_models(instance, from, to).await;
    respond(writer, request_id, switched, |request_id, switch| {
        ServerMessage::BotModelsSwitched { request_id, switch }
    })
    .await
}

async fn list_session_files(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: Arc<str>,
    session_id: SessionId,
) -> Result<()> {
    let listed = async {
        authorize_stored_read(connection, &session_id)
            .await?
            .list_files_with_modified(
                &session_id,
                &[StoredFileOrigin::Upload, StoredFileOrigin::Artifact],
            )
            .await
            .map_err(session_file_rejection)
    }
    .await;
    respond(writer, request_id, listed, |request_id, items| {
        ServerMessage::SessionFiles {
            request_id,
            session_id,
            files: items,
        }
    })
    .await
}

struct SessionFileRead {
    file_id: String,
    offset: u64,
    max_bytes: usize,
}

async fn read_session_file(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: Arc<str>,
    session_id: SessionId,
    read: SessionFileRead,
) -> Result<()> {
    let SessionFileRead {
        file_id,
        offset,
        max_bytes,
    } = read;
    let chunk = async {
        authorize_stored_read(connection, &session_id)
            .await?
            .read_chunk(&session_id, &file_id, offset, max_bytes)
            .await
            .map_err(session_file_rejection)
    }
    .await;
    respond(writer, request_id, chunk, |request_id, chunk| {
        ServerMessage::SessionFileChunk {
            request_id,
            session_id,
            file_id,
            offset: chunk.offset,
            data: chunk.data,
            next_offset: chunk.next_offset,
        }
    })
    .await
}

async fn probe_git_credential(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    target: &str,
) -> Result<()> {
    let probed = crate::host::git::probe_credential(target).await;
    respond(writer, request_id, probed, |request_id, username| {
        ServerMessage::GitCredentialStatus {
            request_id,
            available: username.is_some(),
            username,
        }
    })
    .await
}

async fn list_ssh_identities(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
) -> Result<()> {
    let identities = tokio::task::spawn_blocking(crate::host::ssh::identities)
        .await
        .map_err(internal)
        .and_then(std::convert::identity);
    respond(writer, request_id, identities, |request_id, identities| {
        ServerMessage::SshIdentities {
            request_id,
            identities,
        }
    })
    .await
}

/// Answers with line totals when `totals` is set, otherwise with the diff text.
async fn git_diff(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: Arc<str>,
    session_id: SessionId,
    scope: GitDiffScope,
    totals: bool,
) -> Result<()> {
    let host = match require_selected(connection.selected.as_ref(), &session_id) {
        Ok(host) => host.clone(), // rules-allow: the background diff task owns a session handle
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    if connection.requests.len() >= MAX_PENDING_REQUESTS {
        return write_rejection(
            writer,
            request_id,
            Rejection::new(
                "git_busy",
                "Git diff requests are already in progress; try again shortly",
            ),
        )
        .await;
    }
    // Owned by the connection: disconnecting aborts its outstanding Git reads.
    connection.requests.spawn(async move {
        if totals {
            return match host.git_diff_totals(scope).await {
                Ok(totals) => ServerMessage::GitDiffTotals {
                    request_id,
                    session_id,
                    scope,
                    totals,
                },
                Err(rejection) => rejection.for_request(request_id),
            };
        }
        match host.git_diff(scope).await {
            Ok(diff) => ServerMessage::GitDiff {
                request_id,
                session_id,
                scope,
                diff: diff.text,
                truncated: diff.truncated,
            },
            Err(rejection) => rejection.for_request(request_id),
        }
    });
    Ok(())
}

async fn switch_git_branch(
    writer: &mut (impl AsyncWrite + Unpin),
    selected: Option<&SelectedChat>,
    request_id: Arc<str>,
    session_id: SessionId,
    branch: String,
) -> Result<()> {
    let result = on_selected(selected, &session_id, |host| host.switch_git_branch(branch)).await;
    write_result(writer, request_id, result).await
}

async fn list_workspace_files(
    writer: &mut (impl AsyncWrite + Unpin),
    selected: Option<&SelectedChat>,
    request_id: Arc<str>,
    session_id: SessionId,
    scope: WorkspaceFileScope,
) -> Result<()> {
    let catalog = on_selected(selected, &session_id, |host| host.workspace_files(scope)).await;
    respond(writer, request_id, catalog, |request_id, catalog| {
        ServerMessage::WorkspaceFiles {
            request_id,
            session_id,
            files: catalog.files,
            truncated: catalog.truncated,
        }
    })
    .await
}

async fn read_workspace_file(
    writer: &mut (impl AsyncWrite + Unpin),
    selected: Option<&SelectedChat>,
    request_id: Arc<str>,
    session_id: SessionId,
    path: String,
    offset: u64,
    max_bytes: usize,
) -> Result<()> {
    let chunk = on_selected(selected, &session_id, |host| {
        host.read_workspace_file(&path, offset, max_bytes)
    })
    .await;
    respond(writer, request_id, chunk, |request_id, chunk| {
        ServerMessage::WorkspaceFileChunk {
            request_id,
            session_id,
            path,
            offset,
            data: chunk.data,
            next_offset: chunk.next_offset,
        }
    })
    .await
}

async fn write_workspace_file(
    writer: &mut (impl AsyncWrite + Unpin),
    selected: Option<&SelectedChat>,
    request_id: Arc<str>,
    session_id: SessionId,
    path: String,
    content: String,
) -> Result<()> {
    let result = on_selected(selected, &session_id, |host| {
        host.write_workspace_file(path, content)
    })
    .await;
    write_result(writer, request_id, result).await
}

async fn list_directories_response(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    path: PathBuf,
    include_files: bool,
) -> Result<()> {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(move || list_directories(&path, include_files)),
    )
    .await
    .map_err(|elapsed| { tracing::debug!(%elapsed, "directory listing timed out"); directory_rejection("folder access timed out; check filesystem availability and allow any folder-access prompt on the gateway computer") })
    .and_then(|result| result.map_err(internal))
    .and_then(std::convert::identity);
    respond(writer, request_id, result, |request_id, listing| {
        ServerMessage::Directories {
            request_id,
            listing,
        }
    })
    .await
}

async fn emit_hook(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    bot_id: &str,
    name: &str,
    data: serde_json::Value,
    gateway: &GatewayHost,
) -> Result<()> {
    let emitted = gateway.emit_bot_hook(bot_id, name, data, &request_id).await;
    write_result(writer, request_id, emitted.map(drop)).await
}

async fn clear_provider_credential(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    instance: String,
    gateway: &GatewayHost,
) -> Result<()> {
    let cleared = gateway.clear_credential(&instance).await;
    respond(writer, request_id, cleared, |request_id, ()| {
        ServerMessage::ProviderCredentialCleared {
            request_id,
            instance,
        }
    })
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "credential wire fields stay explicit"
)]
async fn set_provider_credential(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    instance: String,
    provider: String,
    api_key: String,
    base_url: Option<mobius::backend::model::provider::BaseUrl>,
    expires_at: Option<u64>,
    gateway: &GatewayHost,
) -> Result<()> {
    let saved = gateway
        .set_credential(&instance, &provider, api_key, base_url, expires_at)
        .await;
    respond(writer, request_id, saved, |request_id, ()| {
        ServerMessage::ProviderCredentialSaved {
            request_id,
            instance,
            provider,
        }
    })
    .await
}

async fn create_pairing_code(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    auth: &AuthStore,
) -> Result<()> {
    let grant = auth.create_pairing_code().map_err(internal);
    respond(writer, request_id, grant, |request_id, grant| {
        ServerMessage::PairingCode {
            request_id,
            code: grant.code,
            expires_at: grant.expires_at,
        }
    })
    .await
}

async fn list_routines(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    bot_id: Option<Arc<str>>,
    bots: &BotStore,
) -> Result<()> {
    let routines = bots
        .routine_records(bot_id.as_deref(), Utc::now().timestamp())
        .map_err(invalid_routine);
    respond(writer, request_id, routines, |request_id, routines| {
        ServerMessage::Routines {
            request_id,
            routines,
        }
    })
    .await
}

async fn list_routine_history(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    id: Option<Arc<str>>,
    bots: &BotStore,
) -> Result<()> {
    let runs = bots.history(id.as_deref()).map_err(invalid_routine);
    respond(writer, request_id, runs, |request_id, runs| {
        ServerMessage::RoutineHistory { request_id, runs }
    })
    .await
}

/// Answers with an acceptance once `operation` succeeds.
async fn answer(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    operation: impl Future<Output = std::result::Result<(), Rejection>>,
) -> Result<()> {
    write_result(writer, request_id, operation.await).await
}

async fn answer_gateway(
    writer: &mut (impl AsyncWrite + Unpin),
    view: &mut ClientView,
    request_id: Arc<str>,
    operation: impl Future<Output = std::result::Result<Arc<crate::wire::ReadyPayload>, Rejection>>,
) -> Result<()> {
    write_gateway_result(writer, view, request_id, operation.await).await
}

async fn answer_memories(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    operation: impl Future<Output = std::result::Result<Vec<crate::wire::MemoryRecord>, Rejection>>,
) -> Result<()> {
    respond(writer, request_id, operation.await, memories).await
}

async fn storage_usage(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    gateway: &GatewayHost,
) -> Result<()> {
    let usage = gateway.storage_usage_request().await;
    respond(writer, request_id, usage, |request_id, usage| {
        ServerMessage::StorageUsage { request_id, usage }
    })
    .await
}

async fn approve_git_credential(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    target: &str,
    username: &str,
    token: &str,
) -> Result<()> {
    let approved = crate::host::git::approve_credential(target, username, token).await;
    respond(writer, request_id, approved, |request_id, username| {
        ServerMessage::GitCredentialStatus {
            request_id,
            available: true,
            username: Some(username),
        }
    })
    .await
}

async fn generate_ssh_identity(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
) -> Result<()> {
    let generated = crate::host::ssh::generate().await;
    respond(
        writer,
        request_id,
        generated,
        |request_id, (identity, public_key)| ServerMessage::SshIdentityGenerated {
            request_id,
            identity,
            public_key,
        },
    )
    .await
}

async fn routine_run_preview(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    id: &str,
    before: Option<crate::wire::HistoryCursor>,
    gateway: &GatewayHost,
) -> Result<()> {
    let preview = gateway.routine_run_preview(id, before).await;
    respond(writer, request_id, preview, |request_id, preview| {
        ServerMessage::RoutineRunPreview {
            request_id,
            preview,
        }
    })
    .await
}

pub(super) struct TelemetryUpdate {
    pub(super) expected_revision: u64,
    pub(super) sinks: Vec<crate::telemetry::TelemetrySink>,
    pub(super) preserve_auth: Vec<String>,
}

/// Only the local operator may change telemetry collectors.
pub(super) async fn configure_telemetry(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    update: TelemetryUpdate,
    operator: bool,
    gateway: &GatewayHost,
) -> Result<()> {
    let configured = if operator {
        gateway
            .configure_telemetry(
                update.expected_revision,
                update.sinks,
                &update.preserve_auth,
            )
            .await
    } else {
        Err(Rejection::new(
            "operator_required",
            "telemetry collectors are configured locally by the gateway operator",
        ))
    };
    match configured {
        Ok(()) => write_telemetry(writer, request_id, gateway).await,
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn write_telemetry(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    gateway: &GatewayHost,
) -> Result<()> {
    let report = gateway.telemetry_report().await.map_err(internal);
    respond(
        writer,
        request_id,
        report,
        |request_id, (revision, sinks)| ServerMessage::Telemetry {
            request_id,
            revision,
            sinks,
        },
    )
    .await
}
