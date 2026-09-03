use super::*;
use mobius::middleware::session_files::session_file_limits;
use mobius::protocol::{MessageAuthor, Submission};

use crate::wire::{GitDiffScope, WorkspaceFileScope};

pub(super) struct SelectedChat {
    pub(super) host: HostHandle,
    pub(super) broadcasts: broadcast::Receiver<ServerFrame>,
    pub(super) delivered_sequence: u64,
}

pub(super) struct AuthenticatedClient<'a> {
    pub(super) id: &'a str,
    pub(super) connections: &'a ClientConnections,
    pub(super) revocations: &'a broadcast::Sender<String>,
}

pub(super) struct ConnectionSessionState<'a> {
    pub(super) selected: &'a mut Option<SelectedChat>,
    pub(super) session_files: &'a SessionFileStore,
    pub(super) bots: &'a BotStore,
    pub(super) uploads: &'a mut BTreeMap<(String, String), PendingSessionFileWrite>,
}

pub(super) async fn selected_broadcast(
    selected: &mut Option<SelectedChat>,
) -> std::result::Result<ServerFrame, broadcast::error::RecvError> {
    match selected {
        Some(active) => active.broadcasts.recv().await,
        None => std::future::pending().await,
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
    if let Err(rejection) = gateway.reconcile_pending_bot_deletion().await {
        return write_server_error(writer, "bot_deletion_recovery", rejection.message, false).await;
    }
    let Some(message) = handle_collaboration_message(message, writer, gateway).await? else {
        return Ok(());
    };
    match message {
        ClientMessage::Pair { .. } | ClientMessage::Authenticate { .. } => {
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
        } => return unpair_client(writer, request_id, client_id, auth, client).await,
        ClientMessage::ListSessions { request_id } => {
            return list_sessions(writer, request_id, gateway).await;
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
                &connection,
                request_id,
                session_id,
                before_sequence,
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
        ClientMessage::SetSessionPinned {
            request_id,
            session_id,
            pinned,
        } => {
            return set_session_pinned(writer, gateway, request_id, session_id, pinned).await;
        }
        ClientMessage::DeleteSession {
            request_id,
            session_id,
        } => {
            return delete_session(writer, &mut connection, request_id, session_id, gateway).await;
        }
        ClientMessage::Submit {
            session_id,
            submission,
        } => return submit(writer, &connection, session_id, submission).await,
        ClientMessage::SubmitScratchpad {
            request_id,
            scope,
            operation,
        } => return submit_scratchpad(writer, request_id, scope, operation, gateway).await,
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
        ClientMessage::DeleteSessionFile {
            request_id,
            session_id,
            file_id,
        } => {
            return delete_session_file(writer, &mut connection, request_id, session_id, file_id)
                .await;
        }
        ClientMessage::ListSessionFiles {
            request_id,
            session_id,
        } => return list_session_files(writer, &connection, request_id, session_id).await,
        ClientMessage::ReadSessionFile {
            request_id,
            session_id,
            file_id,
            offset,
            max_bytes,
        } => {
            return read_session_file(
                writer,
                &connection,
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
            return write_bot_result(
                writer,
                request_id,
                gateway.create_bot(&name, &description).await,
                gateway,
            )
            .await;
        }
        ClientMessage::ListBots { request_id } => {
            return list_bots(writer, request_id, gateway).await;
        }
        ClientMessage::UpdateBot {
            request_id,
            id,
            expected_revision,
            name,
            description,
            tint,
            config,
        } => {
            return write_bot_result(
                writer,
                request_id,
                gateway
                    .update_bot(&id, expected_revision, &name, &description, tint, config)
                    .await,
                gateway,
            )
            .await;
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
            return write_gateway_result(
                writer,
                request_id,
                gateway
                    .configure_bot_defaults(expected_revision, config)
                    .await,
            )
            .await;
        }
        ClientMessage::InstallExtension {
            request_id,
            source,
            reference,
            subdirectory,
        } => {
            return write_gateway_result(
                writer,
                request_id,
                gateway
                    .install_extension(source, reference, subdirectory)
                    .await,
            )
            .await;
        }
        ClientMessage::UpdateExtension { request_id, id } => {
            return write_gateway_result(writer, request_id, gateway.update_extension(id).await)
                .await;
        }
        ClientMessage::UninstallExtension { request_id, id } => {
            return write_gateway_result(writer, request_id, gateway.uninstall_extension(id).await)
                .await;
        }
        ClientMessage::TrustExtensionHooks {
            request_id,
            id,
            expected_digest,
        } => {
            return write_gateway_result(
                writer,
                request_id,
                gateway
                    .set_extension_hooks_trusted(id, expected_digest, true)
                    .await,
            )
            .await;
        }
        ClientMessage::RevokeExtensionHooksTrust {
            request_id,
            id,
            expected_digest,
        } => {
            return write_gateway_result(
                writer,
                request_id,
                gateway
                    .set_extension_hooks_trusted(id, expected_digest, false)
                    .await,
            )
            .await;
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
        } => return get_git_diff(writer, &connection, request_id, session_id, scope).await,
        ClientMessage::SwitchGitBranch {
            request_id,
            session_id,
            branch,
        } => {
            return switch_git_branch(writer, &connection, request_id, session_id, branch).await;
        }
        ClientMessage::ListWorkspaceFiles {
            request_id,
            session_id,
            scope,
        } => {
            return list_workspace_files(writer, &connection, request_id, session_id, scope).await;
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
                &connection,
                request_id,
                session_id,
                path,
                offset,
                max_bytes,
            )
            .await;
        }
        ClientMessage::WriteWorkspaceFile {
            request_id,
            session_id,
            path,
            content,
        } => {
            return write_workspace_file(
                writer,
                &connection,
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
        ClientMessage::SetProviderCredential {
            request_id,
            instance,
            provider,
            api_key,
        } => {
            return set_provider_credential(
                writer, request_id, instance, provider, api_key, None, gateway,
            )
            .await;
        }
        ClientMessage::SetProviderEndpointCredential {
            request_id,
            instance,
            provider,
            base_url,
            api_key,
        } => {
            return set_provider_credential(
                writer,
                request_id,
                instance,
                provider,
                api_key,
                Some(base_url),
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
        } => {
            return write_gateway_result(
                writer,
                request_id,
                gateway
                    .register_provider(config, label, tint, model_ids, reasoning_efforts)
                    .await,
            )
            .await;
        }
        ClientMessage::RemoveProvider {
            request_id,
            instance,
        } => {
            return write_gateway_result(
                writer,
                request_id,
                gateway.remove_provider(instance).await,
            )
            .await;
        }
        ClientMessage::CreatePairingCode { request_id } => {
            return create_pairing_code(writer, request_id, auth).await;
        }
        ClientMessage::StartProviderLogin {
            request_id,
            provider,
        } => {
            return write_result(
                writer,
                request_id.clone(),
                gateway.start_provider_login(request_id, provider).await,
            )
            .await;
        }
        ClientMessage::GetProfile { request_id } => {
            return get_profile(writer, request_id, gateway).await;
        }
        ClientMessage::CreateRoutine {
            request_id,
            bot_id,
            workspace,
            instructions,
            schedule,
            ends_at,
        } => {
            return write_result(
                writer,
                request_id,
                gateway
                    .create_routine(&bot_id, &workspace, &instructions, schedule, ends_at)
                    .await,
            )
            .await;
        }
        ClientMessage::ListRoutines { request_id, bot_id } => {
            return list_routines(writer, request_id, bot_id, bots).await;
        }
        ClientMessage::UpdateRoutine {
            request_id,
            id,
            bot_id,
            workspace,
            instructions,
            schedule,
            ends_at,
            enabled,
        } => {
            let result = gateway
                .update_routine(
                    &id,
                    &bot_id,
                    &workspace,
                    &instructions,
                    schedule,
                    ends_at,
                    enabled,
                )
                .await;
            return write_result(writer, request_id, result).await;
        }
        ClientMessage::DeleteRoutine { request_id, id } => {
            return write_result(writer, request_id, gateway.delete_routine(&id).await).await;
        }
        ClientMessage::RunRoutine { request_id, id } => {
            return write_result(writer, request_id, gateway.run_routine(id).await).await;
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
        ClientMessage::ListBotSessions { .. }
        | ClientMessage::CreateSwarm { .. }
        | ClientMessage::AddSwarmMember { .. }
        | ClientMessage::LeaveSwarm { .. }
        | ClientMessage::RenameSwarm { .. }
        | ClientMessage::DisbandSwarm { .. }
        | ClientMessage::PostSwarmMessage { .. } => {
            unreachable!("collaboration messages are handled before general dispatch")
        }
    }
    Ok(())
}

async fn handle_collaboration_message(
    message: ClientMessage,
    writer: &mut (impl AsyncWrite + Unpin),
    gateway: &GatewayHost,
) -> Result<Option<ClientMessage>> {
    match message {
        ClientMessage::ListBotSessions { request_id, bot_id } => {
            list_bot_sessions(writer, request_id, bot_id, gateway).await?;
        }
        ClientMessage::CreateSwarm {
            request_id,
            title,
            leader_bot_id,
            member_bot_ids,
        } => {
            write_swarms_result(
                writer,
                request_id,
                gateway
                    .create_swarm(title, leader_bot_id, member_bot_ids)
                    .await,
            )
            .await?;
        }
        ClientMessage::AddSwarmMember {
            request_id,
            swarm_id,
            bot_id,
        } => {
            write_swarms_result(
                writer,
                request_id,
                gateway.add_swarm_member(&swarm_id, bot_id).await,
            )
            .await?;
        }
        ClientMessage::LeaveSwarm {
            request_id,
            swarm_id,
            bot_id,
        } => {
            write_swarms_result(
                writer,
                request_id,
                gateway.leave_swarm(&swarm_id, &bot_id).await,
            )
            .await?;
        }
        ClientMessage::RenameSwarm {
            request_id,
            swarm_id,
            title,
        } => {
            write_swarms_result(
                writer,
                request_id,
                gateway.rename_swarm(&swarm_id, title).await,
            )
            .await?;
        }
        ClientMessage::DisbandSwarm {
            request_id,
            swarm_id,
        } => {
            write_swarms_result(writer, request_id, gateway.disband_swarm(&swarm_id).await).await?;
        }
        ClientMessage::PostSwarmMessage {
            request_id,
            swarm_id,
            text,
        } => {
            write_swarms_result(
                writer,
                request_id,
                gateway.post_swarm_message(&swarm_id, text).await,
            )
            .await?;
        }
        message => return Ok(Some(message)),
    }
    Ok(None)
}

async fn unpair_client(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    client_id: String,
    auth: &AuthStore,
    client: &AuthenticatedClient<'_>,
) -> Result<()> {
    match auth.unpair_client(client.id, &client_id) {
        Ok(true) => {
            let _ = client.revocations.send(client_id);
            write_client_inventory(writer, request_id, client.id, auth, client.connections).await
        }
        Ok(false) => {
            write_rejection(
                writer,
                request_id,
                Rejection {
                    code: "unpair_rejected",
                    message: "that paired device cannot be unpaired from this connection".into(),
                    fatal: false,
                },
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
    request_id: String,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway.sessions().await {
        Ok(sessions) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::Sessions {
                    request_id: Some(request_id),
                    sessions,
                }),
            )
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

async fn write_swarms_result(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    result: std::result::Result<Vec<crate::wire::SwarmRecord>, Rejection>,
) -> Result<()> {
    match result {
        Ok(swarms) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::Swarms {
                    request_id: Some(request_id),
                    swarms,
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
    let result = match gateway.create_workspace_directory(&parent, &name).await {
        Ok(path) => tokio::task::spawn_blocking(move || list_directories(&path, false))
            .await
            .map_err(|error| internal_rejection(error.to_string()))
            .and_then(std::convert::identity),
        Err(rejection) => Err(rejection),
    };
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

async fn get_session_history(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &ConnectionSessionState<'_>,
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

async fn list_bots(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway.bots().await {
        Ok(bots) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::Bots {
                    request_id: Some(request_id),
                    bots,
                }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn write_bot_result(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    result: std::result::Result<crate::wire::BotRecord, Rejection>,
    gateway: &GatewayHost,
) -> Result<()> {
    match result {
        Ok(_) => {
            let bots = match gateway.bots().await {
                Ok(bots) => bots,
                Err(rejection) => return write_rejection(writer, request_id, rejection).await,
            };
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::Bots {
                    request_id: Some(request_id),
                    bots,
                }),
            )
            .await
        }
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
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::Bots {
                    request_id: Some(request_id),
                    bots,
                }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn delete_session(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway.delete_session(&session_id).await {
        Ok(deleted) => {
            forget_deleted_sessions(connection, &deleted);
            write_result(writer, request_id, Ok(())).await
        }
        Err(rejection) => write_result(writer, request_id, Err(rejection)).await,
    }
}

fn forget_deleted_sessions(connection: &mut ConnectionSessionState<'_>, deleted: &[String]) {
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
    connection: &ConnectionSessionState<'_>,
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
            MessageAuthor::Peer { .. } => {
                return write_rejection(
                    writer,
                    request_id,
                    Rejection {
                        code: "invalid_submission",
                        message: "peer messages are gateway-owned".into(),
                        fatal: false,
                    },
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
            Rejection {
                code: "invalid_submission",
                message: error.to_string(),
                fatal: false,
            },
        )
        .await;
    }
    if let Some(message) = user_message
        && !message.attachments.is_empty()
    {
        if !host.accepts_file_attachments() {
            return write_rejection(writer, request_id, uploads_disabled_rejection()).await;
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

async fn submit_scratchpad(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    scope: crate::wire::ScratchpadScope,
    operation: Op,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway.submit_scratchpad(&scope, operation).await {
        Ok(contribution) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::ScratchpadChanged {
                    request_id,
                    scope,
                    contribution,
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
    let host = match require_uploads_enabled(connection.selected, &session_id) {
        Ok(host) => host,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    let _mutation = match host.begin_session_file_mutation(connection.bots) {
        Ok(mutation) => mutation,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    if connection.uploads.len() >= MAX_PENDING_UPLOADS {
        return write_rejection(
            writer,
            request_id,
            session_file_rejection(format!(
                "a connection cannot hold more than {MAX_PENDING_UPLOADS} pending uploads"
            )),
        )
        .await;
    }
    match connection
        .session_files
        .begin_upload(&session_id, name, size, media_type)
        .await
    {
        Ok(upload) => {
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
    let host = match require_uploads_enabled(connection.selected, &session_id) {
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
            session_file_rejection("session file upload is not active"),
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
    let host = match require_uploads_enabled(connection.selected, &session_id) {
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
            session_file_rejection("session file upload is not active"),
        )
        .await;
    };
    match upload.finish().await {
        Ok(file) => {
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

async fn delete_session_file(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
    file_id: String,
) -> Result<()> {
    let host = match require_selected(&*connection.selected, &session_id) {
        Ok(host) => host,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    if connection
        .uploads
        .remove(&(session_id.clone(), file_id.clone()))
        .is_some()
    {
        return write_result(writer, request_id, Ok(())).await;
    }
    let _mutation = match host.begin_session_file_mutation(connection.bots) {
        Ok(mutation) => mutation,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    match connection
        .session_files
        .delete_upload(&session_id, &file_id)
        .await
    {
        Ok(()) => write_result(writer, request_id, Ok(())).await,
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
    connection: &ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
) -> Result<()> {
    if let Err(rejection) = require_readable_files(&*connection.selected, &session_id) {
        return write_rejection(writer, request_id, rejection).await;
    }
    match connection.session_files.list_files(&session_id).await {
        Ok(items) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::SessionFiles {
                    request_id,
                    session_id,
                    files: items,
                }),
            )
            .await
        }
        Err(error) => write_rejection(writer, request_id, session_file_rejection(error)).await,
    }
}

async fn read_session_file(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &ConnectionSessionState<'_>,
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

async fn get_git_diff(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
    scope: GitDiffScope,
) -> Result<()> {
    let host = match require_selected(&*connection.selected, &session_id) {
        Ok(host) => host,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    match host.git_diff(scope).await {
        Ok(diff) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::GitDiff {
                    request_id,
                    session_id,
                    scope,
                    diff,
                }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn switch_git_branch(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &ConnectionSessionState<'_>,
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
    connection: &ConnectionSessionState<'_>,
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
    connection: &ConnectionSessionState<'_>,
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
    connection: &ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
    path: String,
    content: String,
) -> Result<()> {
    let host = match require_selected(&*connection.selected, &session_id) {
        Ok(host) => host,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    write_result(
        writer,
        request_id,
        host.write_workspace_file(path, content).await,
    )
    .await
}

async fn list_directories_response(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    path: PathBuf,
    include_files: bool,
) -> Result<()> {
    let result = tokio::task::spawn_blocking(move || list_directories(&path, include_files))
        .await
        .map_err(|error| internal_rejection(error.to_string()))
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

async fn set_provider_credential(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    instance: String,
    provider: String,
    api_key: String,
    base_url: Option<String>,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway
        .set_credential(instance.clone(), provider.clone(), api_key, base_url)
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

async fn get_profile(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway.profile().await {
        Ok(profile) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::Profile {
                    request_id,
                    profile,
                }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
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
