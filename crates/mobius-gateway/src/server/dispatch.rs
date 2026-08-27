use super::*;
use mobius::middleware::session_files::session_file_limits;
use mobius::protocol::Submission;

use crate::wire::{AgentComposition, GitDiffScope, WorkspaceFileScope};

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
    cron: &CronStore,
    client: &AuthenticatedClient<'_>,
    mut connection: ConnectionSessionState<'_>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
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
        } => {
            return create_session(writer, connection.selected, request_id, workspace, gateway)
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
            return rename_session(writer, &connection, request_id, session_id, title).await;
        }
        ClientMessage::SetSessionPinned {
            request_id,
            session_id,
            pinned,
        } => {
            return set_session_pinned(writer, &connection, request_id, session_id, pinned).await;
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
        ClientMessage::SubmitGlobalScratchpad {
            request_id,
            operation,
        } => return submit_global_scratchpad(writer, request_id, operation, gateway).await,
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
        } => return list_session_files(writer, &connection, request_id, session_id, gateway).await,
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
                gateway,
            )
            .await;
        }
        ClientMessage::ConfigureSession {
            request_id,
            session_id,
            expected_revision,
            config,
        } => {
            return configure_session(
                writer,
                &connection,
                request_id,
                session_id,
                expected_revision,
                config,
            )
            .await;
        }
        ClientMessage::ConfigureDefaultAgent {
            request_id,
            expected_revision,
            config,
        } => {
            return write_gateway_result(
                writer,
                request_id,
                gateway
                    .configure_default_agent(expected_revision, config)
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
            replace_existing_selections,
        } => {
            return write_gateway_result(
                writer,
                request_id,
                gateway
                    .register_provider(
                        config,
                        label,
                        tint,
                        model_ids,
                        reasoning_efforts,
                        replace_existing_selections,
                    )
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
        ClientMessage::CreateCron {
            request_id,
            task,
            source_session_id,
            schedule,
            ends_at,
        } => {
            return write_result(
                writer,
                request_id,
                gateway
                    .create_cron(&source_session_id, &task, schedule, ends_at)
                    .await,
            )
            .await;
        }
        ClientMessage::ListCron { request_id } => {
            return list_cron(writer, request_id, cron).await;
        }
        ClientMessage::UpdateCron {
            request_id,
            id,
            source_session_id,
            task,
            schedule,
            ends_at,
            enabled,
        } => {
            let result = gateway
                .update_cron(&id, &source_session_id, &task, schedule, ends_at, enabled)
                .await;
            return write_result(writer, request_id, result).await;
        }
        ClientMessage::DeleteCron { request_id, id } => {
            let result = cron.delete(&id).map(|_| ()).map_err(cron_rejection);
            return write_result(writer, request_id, result).await;
        }
        ClientMessage::RunCron { request_id, id } => {
            return write_result(writer, request_id, gateway.run_cron(id).await).await;
        }
        ClientMessage::ListCronHistory { request_id, id } => {
            return list_cron_history(writer, request_id, id, cron).await;
        }
        ClientMessage::GetCronRunPreview {
            request_id,
            id,
            before_sequence,
        } => {
            get_cron_run_preview(writer, request_id, id, before_sequence, gateway).await?;
        }
    }
    Ok(())
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

async fn create_session(
    writer: &mut (impl AsyncWrite + Unpin),
    selected: &mut Option<SelectedChat>,
    request_id: String,
    workspace: PathBuf,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway.create_session(&workspace).await {
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
    connection: &ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
    title: String,
) -> Result<()> {
    let host = match require_selected(&*connection.selected, &session_id) {
        Ok(host) => host,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    write_result(
        writer,
        request_id,
        host.rename_session(session_id, title).await,
    )
    .await
}

async fn set_session_pinned(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
    pinned: bool,
) -> Result<()> {
    let host = match require_selected(&*connection.selected, &session_id) {
        Ok(host) => host,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    write_result(
        writer,
        request_id,
        host.set_session_pinned(session_id, pinned).await,
    )
    .await
}

async fn configure_session(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
    expected_revision: u64,
    config: AgentComposition,
) -> Result<()> {
    let host = match require_selected(&*connection.selected, &session_id) {
        Ok(host) => host,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    write_result(
        writer,
        request_id,
        host.configure(expected_revision, config).await,
    )
    .await
}

async fn delete_session(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &mut ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
    gateway: &GatewayHost,
) -> Result<()> {
    if let Err(rejection) = require_selected(connection.selected, &session_id) {
        return write_rejection(writer, request_id, rejection).await;
    }
    connection
        .uploads
        .retain(|(upload_session_id, _), _| upload_session_id != &session_id);
    let result = gateway.delete_session(&session_id).await;
    if result.is_ok() {
        *connection.selected = None;
    }
    write_result(writer, request_id, result).await
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
    if let Op::UserInput {
        attachments: references,
        ..
    } = &submission.op
        && !references.is_empty()
    {
        if !host.accepts_file_attachments() {
            return write_rejection(writer, request_id, uploads_disabled_rejection()).await;
        }
        for reference in references {
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

async fn submit_global_scratchpad(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    operation: Op,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway.submit_global_scratchpad(operation).await {
        Ok(contribution) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::GlobalScratchpadChanged {
                    request_id,
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
    if let Err(rejection) = require_uploads_enabled(connection.selected, &session_id) {
        return write_rejection(writer, request_id, rejection).await;
    }
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
    if let Err(rejection) = require_uploads_enabled(connection.selected, &session_id) {
        connection.uploads.remove(&key);
        return write_rejection(writer, request_id, rejection).await;
    }
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
    if let Err(rejection) = require_uploads_enabled(connection.selected, &session_id) {
        connection.uploads.remove(&key);
        return write_rejection(writer, request_id, rejection).await;
    }
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

/// Files are readable for the selected chat, and for the session a scheduled run executed in.
/// A run session is never selected on a connection, so without this its artifacts are the one
/// part of a transcript the client can already read but not open.
async fn require_readable_files(
    selected: &Option<SelectedChat>,
    session_id: &str,
    gateway: &GatewayHost,
) -> std::result::Result<(), Rejection> {
    let Err(rejection) = require_selected(selected, session_id) else {
        return Ok(());
    };
    if gateway.is_cron_execution_session(session_id).await? {
        Ok(())
    } else {
        Err(rejection)
    }
}

async fn list_session_files(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
    gateway: &GatewayHost,
) -> Result<()> {
    if let Err(rejection) =
        require_readable_files(&*connection.selected, &session_id, gateway).await
    {
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

#[expect(
    clippy::too_many_arguments,
    reason = "wire read fields remain explicit at the dispatch boundary"
)]
async fn read_session_file(
    writer: &mut (impl AsyncWrite + Unpin),
    connection: &ConnectionSessionState<'_>,
    request_id: String,
    session_id: String,
    file_id: String,
    offset: u64,
    max_bytes: usize,
    gateway: &GatewayHost,
) -> Result<()> {
    if let Err(rejection) =
        require_readable_files(&*connection.selected, &session_id, gateway).await
    {
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

async fn list_cron(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    cron: &CronStore,
) -> Result<()> {
    match cron.records(Utc::now().timestamp()) {
        Ok(tasks) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::CronTasks { request_id, tasks }),
            )
            .await
        }
        Err(error) => write_rejection(writer, request_id, cron_rejection(error)).await,
    }
}

async fn list_cron_history(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    id: Option<String>,
    cron: &CronStore,
) -> Result<()> {
    match cron.history(id.as_deref()) {
        Ok(runs) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::CronHistory { request_id, runs }),
            )
            .await
        }
        Err(error) => write_rejection(writer, request_id, cron_rejection(error)).await,
    }
}

async fn get_cron_run_preview(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    id: String,
    before_sequence: Option<u64>,
    gateway: &GatewayHost,
) -> Result<()> {
    match gateway.cron_run_preview(&id, before_sequence).await {
        Ok(preview) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::CronRunPreview {
                    request_id,
                    preview,
                }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}
