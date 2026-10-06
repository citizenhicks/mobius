use super::*;
use mobius::backend::session_files::session_file_limits;
use mobius::backend::session_files::{SessionFileOrigin as StoredFileOrigin, SessionFileSelection};
use mobius::protocol::{MessageAuthor, Submission};

use crate::wire::{GitDiffScope, WorkspaceFileScope};

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
    pub(super) kind: ClientKind,
    pub(super) id: &'a str,
    pub(super) connections: &'a ClientConnections,
    pub(super) revocations: &'a broadcast::Sender<String>,
}

pub(super) const MAX_PENDING_REQUESTS: usize = 4;

pub(super) struct ConnectionSessionState<'a> {
    pub(super) gateway: &'a GatewayHost,
    pub(super) disabled_notifications: &'a mut BTreeSet<GatewayNotification>,
    pub(super) view: &'a mut ClientView,
    pub(super) selected: &'a mut Option<SelectedChat>,
    pub(super) requests: &'a mut JoinSet<ServerMessage>,
    pub(super) session_files: &'a SessionFileStore,
    pub(super) bots: &'a BotStore,
    pub(super) uploads: &'a mut BTreeMap<(String, String), PendingSessionFileWrite>,
    pub(super) voice: &'a mut Option<super::voice::ConnectionVoice>,
    pub(super) browser: &'a mut Option<crate::computer_runtime::browser::BrowserConnection>,
    pub(super) desktop: &'a mut Option<crate::computer_runtime::desktop::DesktopConnection>,
    pub(super) remote_desktop:
        &'a mut Option<crate::computer_runtime::remote_desktop::DesktopStream>,
    pub(super) pending_desktop_control: &'a mut Option<super::desktop::PendingControl>,
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
        return std::future::pending().await;
    };
    loop {
        let frame = broadcasts.recv().await?;
        let sequence = sequence(&frame);
        if sequence.is_none_or(|value| value > *delivered_sequence) {
            *delivered_sequence = sequence.unwrap_or(*delivered_sequence);
            return Ok(frame);
        }
    }
}

pub(super) async fn handle_message(
    message: ClientMessage,
    auth: &AuthStore,
    gateway: &GatewayHost,
    bots: &BotStore,
    client: &AuthenticatedClient<'_>,
    mut connection: ConnectionSessionState<'_>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    let operator = client.local && crate::auth::is_local_operator(client.id);
    let Some(message) = handle_runtime_message(message, gateway, operator, writer).await? else {
        return Ok(());
    };
    let Some(message) =
        super::desktop::handle_message(message, gateway, client, &mut connection, writer).await?
    else {
        return Ok(());
    };
    if let Err(rejection) = gateway.reconcile_pending_bot_deletion().await {
        return write_server_error(writer, "bot_deletion_recovery", rejection.message, false).await;
    }
    let Some(message) = super::voice::handle_message(message, &mut connection, writer).await?
    else {
        return Ok(());
    };
    match message {
        ClientMessage::GetStorageUsage { .. }
        | ClientMessage::GetTelemetry { .. }
        | ClientMessage::ConfigureTelemetry { .. }
        | ClientMessage::SendTelemetry { .. } => {
            unreachable!("runtime control was handled above")
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
        ClientMessage::GetGitDiffTotals {
            request_id,
            session_id,
            scope,
        } => {
            return get_git_diff(
                writer,
                &mut connection,
                GitDiffRequest {
                    request_id,
                    session_id,
                    scope,
                    totals: true,
                },
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
        } => {
            return create_session(
                writer,
                connection.selected,
                request_id,
                workspace,
                bot_id,
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
            before_sequence,
        } => {
            return get_session_history(
                writer,
                &mut connection,
                request_id,
                session_id,
                before_sequence,
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
            return rename_session(writer, gateway, request_id, session_id, title).await;
        }
        ClientMessage::AttachSessionFolder {
            request_id,
            session_id,
            folder,
        } => {
            return attach_session_folder(
                writer,
                &*connection.selected,
                gateway,
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
            return set_session_pinned(writer, gateway, request_id, session_id, pinned).await;
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
        } => return submit(writer, &mut connection, session_id, submission).await,
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
            return upload_session_file_chunk(
                writer,
                &mut connection,
                request_id,
                session_id,
                upload_id,
                offset,
                data,
            )
            .await;
        }
        ClientMessage::FinishSessionFileUpload {
            request_id,
            session_id,
            upload_id,
        } => {
            return finish_session_file_upload(
                writer,
                &mut connection,
                request_id,
                session_id,
                upload_id,
            )
            .await;
        }
        ClientMessage::ListSessionFiles {
            request_id,
            session_id,
        } => return list_session_files(writer, &mut connection, request_id, session_id).await,
        ClientMessage::ReadSessionFile {
            request_id,
            session_id,
            file_id,
            offset,
            max_bytes,
        } => {
            return read_session_file(
                writer,
                &mut connection,
                request_id,
                session_id,
                file_id,
                offset,
                max_bytes,
            )
            .await;
        }
        ClientMessage::CreateBot {
            request_id,
            name,
            description,
        } => {
            let created = gateway.create_bot(&name, &description).await;
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
        } => {
            let trusted = gateway
                .set_extension_hooks_trusted(id, expected_digest, true)
                .await;
            return write_gateway_result(writer, connection.view, request_id, trusted).await;
        }
        ClientMessage::RevokeExtensionHooksTrust {
            request_id,
            id,
            expected_digest,
        } => {
            let revoked = gateway
                .set_extension_hooks_trusted(id, expected_digest, false)
                .await;
            return write_gateway_result(writer, connection.view, request_id, revoked).await;
        }
        ClientMessage::ProbeGitCredential { request_id, target } => {
            return probe_git_credential(writer, request_id, target, gateway).await;
        }
        ClientMessage::ApproveGitCredential {
            request_id,
            target,
            username,
            token,
        } => {
            return approve_git_credential(writer, request_id, target, username, token, gateway)
                .await;
        }
        ClientMessage::ListSshIdentities { request_id } => {
            return list_ssh_identities(writer, request_id, gateway).await;
        }
        ClientMessage::GenerateSshIdentity { request_id } => {
            return generate_ssh_identity(writer, request_id, gateway).await;
        }
        ClientMessage::GetGitDiff {
            request_id,
            session_id,
            scope,
        } => {
            return get_git_diff(
                writer,
                &mut connection,
                GitDiffRequest {
                    request_id,
                    session_id,
                    scope,
                    totals: false,
                },
            )
            .await;
        }
        ClientMessage::SwitchGitBranch {
            request_id,
            session_id,
            branch,
        } => {
            return switch_git_branch(writer, &mut connection, request_id, session_id, branch)
                .await;
        }
        ClientMessage::ListWorkspaceFiles {
            request_id,
            session_id,
            scope,
        } => {
            return list_workspace_files(writer, &mut connection, request_id, session_id, scope)
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
                &mut connection,
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
            let host = match require_selected(&*connection.selected, &session_id) {
                Ok(host) => host,
                Err(rejection) => return write_rejection(writer, request_id, rejection).await,
            };
            let result = host.delete_workspace_file(path).await;
            if result.is_ok() {
                gateway.invalidate_storage_usage();
            }
            return write_result(writer, request_id, result).await;
        }
        ClientMessage::WriteWorkspaceFile {
            request_id,
            session_id,
            path,
            content,
        } => {
            return write_workspace_file(
                writer,
                &mut connection,
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
            api_key,
            expires_at,
        } => {
            return set_provider_credential(
                writer, request_id, instance, provider, api_key, None, expires_at, gateway,
            )
            .await;
        }
        ClientMessage::SetProviderEndpointCredential {
            request_id,
            instance,
            provider,
            base_url,
            api_key,
            expires_at,
        } => {
            return set_provider_credential(
                writer,
                request_id,
                instance,
                provider,
                api_key,
                Some(base_url),
                expires_at,
                gateway,
            )
            .await;
        }
        ClientMessage::RegisterProvider {
            request_id,
            config,
            label,
            tint,
            model_ids,
            reasoning_efforts,
            image_model_ids,
        } => {
            let registered = gateway
                .register_provider(
                    operator,
                    crate::config::ConfiguredProvider {
                        selection: config,
                        label,
                        tint,
                        model_ids,
                        reasoning_efforts,
                        image_model_ids,
                    },
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
        ClientMessage::GetProfile { .. } => {
            unreachable!("profile messages are handled by connection transport")
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
        ClientMessage::ListRoutineHistory { request_id, id } => {
            return list_routine_history(writer, request_id, id, bots).await;
        }
        ClientMessage::DeleteRoutineRun { request_id, id } => {
            return write_result(writer, request_id, gateway.delete_routine_run(&id).await).await;
        }
        ClientMessage::GetRoutineRunPreview {
            request_id,
            id,
            before_sequence,
        } => {
            get_routine_run_preview(writer, request_id, id, before_sequence, gateway).await?;
        }
        ClientMessage::SetBrowserRuntime { .. } | ClientMessage::BrowserPageReply { .. } => {
            return crate::computer_runtime::browser::handle_message(
                message,
                &gateway.remote_desktop.browser,
                connection.browser,
                client.local,
                client.kind,
                gateway.remote_desktop.browser_available(),
                writer,
            )
            .await;
        }
        ClientMessage::SetDesktopRuntime { .. } | ClientMessage::DesktopControlReply { .. } => {
            return crate::computer_runtime::desktop::handle_message(
                message,
                &gateway.desktop,
                connection.desktop,
                client.local,
                client.kind,
                writer,
            )
            .await;
        }
        ClientMessage::OpenComputer { .. }
        | ClientMessage::SetDesktopStream { .. }
        | ClientMessage::DesktopData { .. }
        | ClientMessage::SetDesktopControl { .. } => {
            unreachable!("desktop messages were handled above")
        }
        ClientMessage::StartRealtimeVoice { .. } | ClientMessage::EndRealtimeVoice { .. } => {
            unreachable!("voice messages are handled before general dispatch")
        }
        ClientMessage::ListBotSessions { request_id, bot_id } => {
            return list_bot_sessions(writer, request_id, bot_id, gateway).await;
        }
    }
    Ok(())
}

async fn start_provider_login(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    provider: String,
    client_id: &str,
    auth: &AuthStore,
    gateway: &GatewayHost,
) -> Result<()> {
    let result = gateway
        .start_provider_login(request_id.clone(), provider, client_id)
        .await;
    // Unpairing may have removed replay before this already-selected request
    // reserved its slot. Check after reservation to close that ordering too.
    if !auth.clients()?.iter().any(|paired| paired.id == client_id) {
        gateway
            .forget_provider_login(client_id)
            .await
            .map_err(|rejection| Error::Protocol(rejection.message))?;
        return Err(Error::Unauthorized);
    }
    match result {
        Ok(Some(message)) => write_frame(writer, &ServerFrame::new(message)).await,
        result => write_result(writer, request_id, result.map(|_| ())).await,
    }
}

async fn unpair_client(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    client_id: String,
    auth: &AuthStore,
    client: &AuthenticatedClient<'_>,
    gateway: &GatewayHost,
) -> Result<()> {
    match auth.unpair_client(client.id, &client_id) {
        Ok(true) => {
            let _ = client.revocations.send(client_id.clone());
            gateway
                .forget_provider_login(&client_id)
                .await
                .map_err(|rejection| Error::Protocol(rejection.message))?;
            write_client_inventory(writer, request_id, client.id, auth, client.connections).await
        }
        Ok(false) => {
            write_rejection(
                writer,
                request_id,
                Rejection::new(
                    "unpair_rejected",
                    "that paired device cannot be unpaired from this connection",
                ),
            )
            .await
        }
        Err(_) => {
            write_rejection(
                writer,
                request_id,
                internal_rejection("failed to update paired devices".into()),
            )
            .await
        }
    }
}

async fn list_sessions(
    writer: &mut (impl AsyncWrite + Unpin),
    view: &mut ClientView,
    request_id: String,
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
    request_id: String,
    bot_id: String,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway.hidden_bot_sessions(&bot_id).await {
        Ok(sessions) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::BotSessions {
                    request_id,
                    bot_id,
                    sessions,
                }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn create_session(
    writer: &mut (impl AsyncWrite + Unpin),
    selected: &mut Option<SelectedChat>,
    request_id: String,
    workspace: PathBuf,
    bot_id: String,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway.create_session(&workspace, &bot_id).await {
        Ok(host) => open_selected(writer, selected, request_id, host, None).await,
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn create_workspace_directory(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
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
    request_id: String,
    session_id: String,
    last_sequence: Option<u64>,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway.open_session(&session_id).await {
        Ok(host) => open_selected(writer, selected, request_id, host, last_sequence).await,
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

/// Selects a chat for requests only: no replay, no live events.
async fn select_session(
    writer: &mut (impl AsyncWrite + Unpin),
    selected: &mut Option<SelectedChat>,
    request_id: String,
    session_id: String,
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

async fn get_session_history(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
    before_sequence: Option<u64>,
) -> Result<()> {
    let host = match require_selected(&*connection.selected, &session_id) {
        Ok(host) => host,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    write_session_history(writer, host, request_id, session_id, before_sequence).await
}

async fn rename_session(
    writer: &mut (impl AsyncWrite + Unpin),
    gateway: &GatewayHost,
    request_id: String,
    session_id: String,
    title: String,
) -> Result<()> {
    write_result(
        writer,
        request_id,
        gateway.rename_session(&session_id, &title).await,
    )
    .await
}

async fn attach_session_folder(
    writer: &mut (impl AsyncWrite + Unpin),
    selected: &Option<SelectedChat>,
    gateway: &GatewayHost,
    request_id: String,
    session_id: String,
    folder: PathBuf,
) -> Result<()> {
    let host = match require_selected(selected, &session_id) {
        Ok(host) => host,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    let result = host.attach_folder(folder).await;
    if result.is_ok() {
        gateway.invalidate_storage_usage();
    }
    write_result(writer, request_id, result).await
}

async fn set_session_pinned(
    writer: &mut (impl AsyncWrite + Unpin),
    gateway: &GatewayHost,
    request_id: String,
    session_id: String,
    pinned: bool,
) -> Result<()> {
    write_result(
        writer,
        request_id,
        gateway.set_session_pinned(&session_id, pinned).await,
    )
    .await
}

/// Answers a Bot request with the current Bot catalog.
async fn write_bot_result<T>(
    writer: &mut (impl AsyncWrite + Unpin),
    view: &mut ClientView,
    request_id: String,
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
    request_id: String,
    result: std::result::Result<(Vec<crate::wire::BotRecord>, Vec<String>), Rejection>,
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
    request_id: String,
    session_ids: Vec<String>,
    mut selection: SessionFileSelection,
    gateway: &GatewayHost,
) -> Result<()> {
    if let SessionFileSelection::Ids(ids) = &mut selection
        && session_ids.len() == 1
        && !ids.is_empty()
    {
        ids.retain(|id| {
            connection
                .uploads
                .remove(&(session_ids[0].clone(), id.clone()))
                .is_none()
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

fn forget_deleted_sessions(connection: &mut ConnectionSessionState<'_>, deleted: &[String]) {
    if connection
        .voice
        .as_ref()
        .is_some_and(|voice| deleted.contains(&voice.session_id))
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
    connection: &mut ConnectionSessionState<'_>,
    session_id: String,
    submission: Submission,
) -> Result<()> {
    let request_id = submission.id.clone();
    let host = match require_selected(&*connection.selected, &session_id) {
        Ok(host) => host,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    let user_message = match &submission.op {
        Op::Message { message } => match &message.author {
            MessageAuthor::User => Some(message),
            MessageAuthor::Source { .. } => {
                return write_rejection(
                    writer,
                    request_id,
                    Rejection::new("invalid_submission", "peer messages are gateway-owned"),
                )
                .await;
            }
        },
        _ => None,
    };
    if let Err(error) = validate_submission(&submission) {
        return write_rejection(
            writer,
            request_id,
            Rejection::new("invalid_submission", error.to_string()),
        )
        .await;
    }
    if let Some(message) = user_message
        && !message.attachments.is_empty()
    {
        match host.accepts_file_attachments().await {
            Ok(true) => {}
            Ok(false) => {
                return write_rejection(writer, request_id, uploads_disabled_rejection()).await;
            }
            Err(rejection) => return write_rejection(writer, request_id, rejection).await,
        }
        for reference in &message.attachments {
            if let Err(error) = connection
                .session_files
                .verify_upload(&session_id, reference)
                .await
            {
                return write_rejection(writer, request_id, session_file_rejection(error)).await;
            }
        }
    }
    write_result(writer, request_id, host.submit(submission).await).await
}

async fn contribution_response(
    writer: &mut (impl AsyncWrite + Unpin),
    view: &mut ClientView,
    request_id: String,
    result: std::result::Result<Vec<mobius::protocol::FrontendContribution>, Rejection>,
) -> Result<()> {
    match result {
        Ok(contributions) => {
            view.forget(crate::wire::ReadySection::Config);
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::Contributions {
                    request_id,
                    contributions,
                }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn begin_session_file_upload(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
    name: String,
    size: u64,
    media_type: String,
) -> Result<()> {
    let host = match require_uploads_enabled(connection.selected, &session_id).await {
        Ok(host) => host,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    let _mutation = match host.begin_session_file_mutation(connection.bots) {
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
            let upload_id = upload.id().to_string();
            connection
                .uploads
                .insert((session_id.clone(), upload_id.clone()), upload);
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

async fn upload_session_file_chunk(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
    upload_id: String,
    offset: u64,
    data: Vec<u8>,
) -> Result<()> {
    let key = (session_id.clone(), upload_id.clone());
    let host = match require_uploads_enabled(connection.selected, &session_id).await {
        Ok(host) => host,
        Err(rejection) => {
            connection.uploads.remove(&key);
            return write_rejection(writer, request_id, rejection).await;
        }
    };
    let _mutation = match host.begin_session_file_mutation(connection.bots) {
        Ok(mutation) => mutation,
        Err(rejection) => {
            connection.uploads.remove(&key);
            return write_rejection(writer, request_id, rejection).await;
        }
    };
    let Some(upload) = connection.uploads.get_mut(&key) else {
        return write_rejection(
            writer,
            request_id,
            session_file_rejection(mobius::Error::Tool(
                "session file upload is not active".into(),
            )),
        )
        .await;
    };
    let result = upload.append(offset, &data).await;
    match result {
        Ok(next_offset) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::SessionFileUploadChunkAccepted {
                    request_id,
                    session_id,
                    upload_id,
                    next_offset,
                }),
            )
            .await
        }
        Err(error) => {
            connection.uploads.remove(&key);
            write_rejection(writer, request_id, session_file_rejection(error)).await
        }
    }
}

async fn finish_session_file_upload(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
    upload_id: String,
) -> Result<()> {
    let key = (session_id.clone(), upload_id);
    let host = match require_uploads_enabled(connection.selected, &session_id).await {
        Ok(host) => host,
        Err(rejection) => {
            connection.uploads.remove(&key);
            return write_rejection(writer, request_id, rejection).await;
        }
    };
    let _mutation = match host.begin_session_file_mutation(connection.bots) {
        Ok(mutation) => mutation,
        Err(rejection) => {
            connection.uploads.remove(&key);
            return write_rejection(writer, request_id, rejection).await;
        }
    };
    let Some(upload) = connection.uploads.remove(&key) else {
        return write_rejection(
            writer,
            request_id,
            session_file_rejection(mobius::Error::Tool(
                "session file upload is not active".into(),
            )),
        )
        .await;
    };
    match upload.finish().await {
        Ok(file) => {
            connection.gateway.invalidate_storage_usage();
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::SessionFileUploadCompleted {
                    request_id,
                    session_id,
                    file,
                }),
            )
            .await
        }
        Err(error) => write_rejection(writer, request_id, session_file_rejection(error)).await,
    }
}

fn require_readable_files(
    selected: &Option<SelectedChat>,
    session_id: &str,
) -> std::result::Result<(), Rejection> {
    require_selected(selected, session_id).map(|_| ())
}

async fn list_session_files(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
) -> Result<()> {
    if let Err(rejection) = require_readable_files(&*connection.selected, &session_id) {
        return write_rejection(writer, request_id, rejection).await;
    }
    match connection
        .session_files
        .list_files(
            &session_id,
            &[StoredFileOrigin::Upload, StoredFileOrigin::Artifact],
        )
        .await
    {
        Ok(items) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::SessionFiles {
                    request_id,
                    session_id,
                    files: items
                        .into_iter()
                        .map(|(origin, file)| mobius::protocol::SessionFileRecord {
                            origin: match origin {
                                StoredFileOrigin::Upload => {
                                    mobius::protocol::SessionFileOrigin::User
                                }
                                StoredFileOrigin::Artifact | StoredFileOrigin::Observation => {
                                    mobius::protocol::SessionFileOrigin::Agent
                                }
                            },
                            file,
                        })
                        .collect(),
                }),
            )
            .await
        }
        Err(error) => write_rejection(writer, request_id, session_file_rejection(error)).await,
    }
}

async fn read_session_file(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
    file_id: String,
    offset: u64,
    max_bytes: usize,
) -> Result<()> {
    if let Err(rejection) = require_readable_files(&*connection.selected, &session_id) {
        return write_rejection(writer, request_id, rejection).await;
    }
    match connection
        .session_files
        .read_chunk(&session_id, &file_id, offset, max_bytes)
        .await
    {
        Ok(chunk) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::SessionFileChunk {
                    request_id,
                    session_id,
                    file_id,
                    offset: chunk.offset,
                    data: chunk.data,
                    next_offset: chunk.next_offset,
                }),
            )
            .await
        }
        Err(error) => write_rejection(writer, request_id, session_file_rejection(error)).await,
    }
}

async fn probe_git_credential(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    target: String,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway.probe_git_credential(&target).await {
        Ok(username) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::GitCredentialStatus {
                    request_id,
                    available: username.is_some(),
                    username,
                }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn approve_git_credential(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    target: String,
    username: String,
    token: String,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway
        .approve_git_credential(&target, &username, &token)
        .await
    {
        Ok(username) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::GitCredentialStatus {
                    request_id,
                    available: true,
                    username: Some(username),
                }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn list_ssh_identities(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway.ssh_identities().await {
        Ok(identities) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::SshIdentities {
                    request_id,
                    identities,
                }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn generate_ssh_identity(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway.generate_ssh_identity().await {
        Ok((identity, public_key)) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::SshIdentityGenerated {
                    request_id,
                    identity,
                    public_key,
                }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

struct GitDiffRequest {
    request_id: String,
    session_id: String,
    scope: GitDiffScope,
    /// Answer with line totals instead of the diff text.
    totals: bool,
}

async fn get_git_diff(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request: GitDiffRequest,
) -> Result<()> {
    let GitDiffRequest {
        request_id,
        session_id,
        scope,
        totals,
    } = request;
    let host = match require_selected(&*connection.selected, &session_id) {
        Ok(host) => host.clone(),
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
        let rejected = |request_id, rejection: Rejection| ServerMessage::Rejected {
            request_id,
            code: rejection.code.into(),
            message: rejection.message,
            fatal: rejection.fatal,
        };
        if totals {
            return match host.git_diff_totals(scope).await {
                Ok(totals) => ServerMessage::GitDiffTotals {
                    request_id,
                    session_id,
                    scope,
                    totals,
                },
                Err(rejection) => rejected(request_id, rejection),
            };
        }
        match host.git_diff(scope).await {
            Ok(diff) => ServerMessage::GitDiff {
                request_id,
                session_id,
                scope,
                diff,
            },
            Err(rejection) => rejected(request_id, rejection),
        }
    });
    Ok(())
}

async fn switch_git_branch(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
    branch: String,
) -> Result<()> {
    let host = match require_selected(&*connection.selected, &session_id) {
        Ok(host) => host,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    write_result(writer, request_id, host.switch_git_branch(branch).await).await
}

async fn list_workspace_files(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
    scope: WorkspaceFileScope,
) -> Result<()> {
    let host = match require_selected(&*connection.selected, &session_id) {
        Ok(host) => host,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    match host.workspace_files(scope).await {
        Ok(catalog) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::WorkspaceFiles {
                    request_id,
                    session_id,
                    files: catalog.files,
                    truncated: catalog.truncated,
                }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn read_workspace_file(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
    path: String,
    offset: u64,
    max_bytes: usize,
) -> Result<()> {
    let host = match require_selected(&*connection.selected, &session_id) {
        Ok(host) => host,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    match host
        .read_workspace_file(path.clone(), offset, max_bytes)
        .await
    {
        Ok(chunk) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::WorkspaceFileChunk {
                    request_id,
                    session_id,
                    path,
                    offset,
                    data: chunk.data,
                    next_offset: chunk.next_offset,
                }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn write_workspace_file(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
    path: String,
    content: String,
) -> Result<()> {
    let host = match require_selected(&*connection.selected, &session_id) {
        Ok(host) => host,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    let result = host.write_workspace_file(path, content).await;
    if result.is_ok() {
        connection.gateway.invalidate_storage_usage();
    }
    write_result(writer, request_id, result).await
}

async fn list_directories_response(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    path: PathBuf,
    include_files: bool,
) -> Result<()> {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(move || list_directories(&path, include_files)),
    )
    .await
    .map_err(|_| directory_rejection("folder access timed out; check filesystem availability and allow any folder-access prompt on the gateway computer"))
    .and_then(|result| result.map_err(|error| internal_rejection(error.to_string())))
    .and_then(std::convert::identity);
    match result {
        Ok(listing) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::Directories {
                    request_id,
                    listing,
                }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn clear_provider_credential(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    instance: String,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway.clear_credential(instance.clone()).await {
        Ok(()) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::ProviderCredentialCleared {
                    request_id,
                    instance,
                }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "credential wire fields stay explicit"
)]
async fn set_provider_credential(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    instance: String,
    provider: String,
    api_key: String,
    base_url: Option<String>,
    expires_at: Option<u64>,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway
        .set_credential(
            instance.clone(),
            provider.clone(),
            api_key,
            base_url,
            expires_at,
        )
        .await
    {
        Ok(()) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::ProviderCredentialSaved {
                    request_id,
                    instance,
                    provider,
                }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn create_pairing_code(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    auth: &AuthStore,
) -> Result<()> {
    match auth.create_pairing_code() {
        Ok(grant) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::PairingCode {
                    request_id,
                    code: grant.code,
                    expires_at: grant.expires_at,
                }),
            )
            .await
        }
        Err(error) => {
            write_rejection(writer, request_id, internal_rejection(error.to_string())).await
        }
    }
}

async fn list_routines(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    bot_id: Option<String>,
    bots: &BotStore,
) -> Result<()> {
    match bots.routine_records(bot_id.as_deref(), Utc::now().timestamp()) {
        Ok(routines) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::Routines {
                    request_id,
                    routines,
                }),
            )
            .await
        }
        Err(error) => write_rejection(writer, request_id, routine_rejection(error)).await,
    }
}

async fn list_routine_history(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    id: Option<String>,
    bots: &BotStore,
) -> Result<()> {
    match bots.history(id.as_deref()) {
        Ok(runs) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::RoutineHistory { request_id, runs }),
            )
            .await
        }
        Err(error) => write_rejection(writer, request_id, routine_rejection(error)).await,
    }
}

async fn get_routine_run_preview(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    id: String,
    before_sequence: Option<u64>,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway.routine_run_preview(&id, before_sequence).await {
        Ok(preview) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::RoutineRunPreview {
                    request_id,
                    preview,
                }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

pub(super) async fn handle_runtime_message(
    message: ClientMessage,
    gateway: &GatewayHost,
    operator: bool,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<Option<ClientMessage>> {
    let request_id = match message {
        ClientMessage::GetStorageUsage { request_id } => {
            let usage = match gateway.storage_usage_request().await {
                Ok(usage) => usage,
                Err(rejection) => {
                    return write_rejection(writer, request_id, rejection)
                        .await
                        .map(|()| None);
                }
            };
            return write_frame(
                writer,
                &ServerFrame::new(ServerMessage::StorageUsage { request_id, usage }),
            )
            .await
            .map(|()| None);
        }
        ClientMessage::GetTelemetry { request_id } => request_id,
        ClientMessage::ConfigureTelemetry {
            request_id,
            expected_revision,
            sinks,
            preserve_auth,
        } => {
            if !operator {
                return write_rejection(
                    writer,
                    request_id,
                    Rejection {
                        code: "operator_required",
                        message:
                            "telemetry collectors are configured locally by the gateway operator"
                                .into(),
                        fatal: false,
                    },
                )
                .await
                .map(|()| None);
            }
            if let Err(rejection) = gateway
                .configure_telemetry(expected_revision, sinks, &preserve_auth)
                .await
            {
                return write_rejection(writer, request_id, rejection)
                    .await
                    .map(|()| None);
            }
            request_id
        }
        ClientMessage::SendTelemetry {
            request_id,
            sink_id,
        } => {
            return write_result(
                writer,
                request_id,
                gateway
                    .schedule_telemetry(sink_id)
                    .await
                    .map_err(|error| internal_rejection(error.to_string())),
            )
            .await
            .map(|()| None);
        }
        other => return Ok(Some(other)),
    };
    let (revision, sinks) = match gateway.telemetry_report().await {
        Ok(report) => report,
        Err(error) => {
            return write_rejection(writer, request_id, internal_rejection(error.to_string()))
                .await
                .map(|()| None);
        }
    };
    write_frame(
        writer,
        &ServerFrame::new(ServerMessage::Telemetry {
            request_id,
            revision,
            sinks,
        }),
    )
    .await
    .map(|()| None)
}
