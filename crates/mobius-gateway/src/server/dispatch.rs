use super::*;

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
    connection: ConnectionSessionState<'_>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    let ConnectionSessionState {
        selected,
        session_files,
        uploads,
    } = connection;
    let message = match normalize_capability_command(message, selected) {
        Ok(message) => message,
        Err((request_id, rejection)) => {
            return write_rejection(writer, request_id, rejection).await;
        }
    };
    match message {
        ClientMessage::Pair { .. } | ClientMessage::Authenticate { .. } => {
            write_server_error(
                writer,
                "already_authenticated",
                "this connection is already authenticated",
                false,
            )
            .await
        }
        ClientMessage::ListClients { request_id } => {
            write_client_inventory(writer, request_id, client.id, auth, client.connections).await
        }
        ClientMessage::UnpairClient {
            request_id,
            client_id,
        } => match auth.unpair_client(client.id, &client_id) {
            Ok(true) => {
                let _ = client.revocations.send(client_id);
                write_client_inventory(writer, request_id, client.id, auth, client.connections)
                    .await
            }
            Ok(false) => {
                write_rejection(
                    writer,
                    request_id,
                    Rejection {
                        code: "unpair_rejected",
                        message: "that paired device cannot be unpaired from this connection"
                            .into(),
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
        },
        ClientMessage::ListSessions { request_id } => match gateway.sessions().await {
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
        },
        ClientMessage::CreateSession {
            request_id,
            workspace,
        } => match gateway.create_session(&workspace).await {
            Ok(host) => open_selected(writer, selected, request_id, host, None).await,
            Err(rejection) => write_rejection(writer, request_id, rejection).await,
        },
        ClientMessage::CreateWorkspaceDirectory {
            request_id,
            parent,
            name,
        } => {
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
        ClientMessage::OpenSession {
            request_id,
            session_id,
            last_sequence,
        } => match gateway.open_session(&session_id).await {
            Ok(host) => open_selected(writer, selected, request_id, host, last_sequence).await,
            Err(rejection) => write_rejection(writer, request_id, rejection).await,
        },
        ClientMessage::GetSessionHistory {
            request_id,
            session_id,
            before_sequence,
        } => {
            let host = match require_selected(selected, &session_id) {
                Ok(host) => host,
                Err(rejection) => return write_rejection(writer, request_id, rejection).await,
            };
            write_session_history(writer, host, request_id, session_id, before_sequence).await
        }
        ClientMessage::RenameSession {
            request_id,
            session_id,
            title,
        } => {
            let host = match require_selected(selected, &session_id) {
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
        ClientMessage::SetSessionPinned {
            request_id,
            session_id,
            pinned,
        } => {
            let host = match require_selected(selected, &session_id) {
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
        ClientMessage::DeleteSession {
            request_id,
            session_id,
        } => {
            if let Err(rejection) = require_selected(selected, &session_id) {
                return write_rejection(writer, request_id, rejection).await;
            }
            uploads.retain(|(upload_session_id, _), _| upload_session_id != &session_id);
            let result = gateway.delete_session(&session_id).await;
            if result.is_ok() {
                *selected = None;
            }
            write_result(writer, request_id, result).await
        }
        ClientMessage::Submit {
            session_id,
            submission,
        } => {
            let request_id = submission.id.clone();
            let host = match require_selected(selected, &session_id) {
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
                    if let Err(error) = session_files.verify_upload(&session_id, reference).await {
                        return write_rejection(writer, request_id, session_file_rejection(error))
                            .await;
                    }
                }
            }
            write_result(writer, request_id, host.submit(submission).await).await
        }
        ClientMessage::BeginSessionFileUpload {
            request_id,
            session_id,
            name,
            size,
            media_type,
        } => {
            if let Err(rejection) = require_uploads_enabled(selected, &session_id) {
                return write_rejection(writer, request_id, rejection).await;
            }
            if uploads.len() >= MAX_PENDING_UPLOADS {
                return write_rejection(
                    writer,
                    request_id,
                    session_file_rejection(format!(
                        "a connection cannot hold more than {MAX_PENDING_UPLOADS} pending uploads"
                    )),
                )
                .await;
            }
            match session_files
                .begin_upload(&session_id, name, size, media_type)
                .await
            {
                Ok(upload) => {
                    let upload_id = upload.id().to_string();
                    uploads.insert((session_id.clone(), upload_id.clone()), upload);
                    write_frame(
                        writer,
                        &ServerFrame::new(ServerMessage::SessionFileUploadReady {
                            request_id,
                            session_id,
                            upload_id,
                            max_chunk_bytes: MAX_UPLOAD_CHUNK_BYTES,
                        }),
                    )
                    .await
                }
                Err(error) => {
                    write_rejection(writer, request_id, session_file_rejection(error)).await
                }
            }
        }
        ClientMessage::UploadSessionFileChunk {
            request_id,
            session_id,
            upload_id,
            offset,
            data,
        } => {
            let key = (session_id.clone(), upload_id.clone());
            if let Err(rejection) = require_uploads_enabled(selected, &session_id) {
                uploads.remove(&key);
                return write_rejection(writer, request_id, rejection).await;
            }
            let Some(upload) = uploads.get_mut(&key) else {
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
                    uploads.remove(&key);
                    write_rejection(writer, request_id, session_file_rejection(error)).await
                }
            }
        }
        ClientMessage::FinishSessionFileUpload {
            request_id,
            session_id,
            upload_id,
        } => {
            let key = (session_id.clone(), upload_id);
            if let Err(rejection) = require_uploads_enabled(selected, &session_id) {
                uploads.remove(&key);
                return write_rejection(writer, request_id, rejection).await;
            }
            let Some(upload) = uploads.remove(&key) else {
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
                Err(error) => {
                    write_rejection(writer, request_id, session_file_rejection(error)).await
                }
            }
        }
        ClientMessage::ListSessionFiles {
            request_id,
            session_id,
        } => {
            if let Err(rejection) = require_selected(selected, &session_id) {
                return write_rejection(writer, request_id, rejection).await;
            }
            match session_files.list_files(&session_id).await {
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
                Err(error) => {
                    write_rejection(writer, request_id, session_file_rejection(error)).await
                }
            }
        }
        ClientMessage::ReadSessionFile {
            request_id,
            session_id,
            file_id,
            offset,
            max_bytes,
        } => {
            if let Err(rejection) = require_selected(selected, &session_id) {
                return write_rejection(writer, request_id, rejection).await;
            }
            match session_files
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
                Err(error) => {
                    write_rejection(writer, request_id, session_file_rejection(error)).await
                }
            }
        }
        ClientMessage::ConfigureSession {
            request_id,
            session_id,
            expected_revision,
            config,
        } => {
            let host = match require_selected(selected, &session_id) {
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
        ClientMessage::ConfigureDefaultAgent {
            request_id,
            expected_revision,
            config,
        } => {
            write_gateway_result(
                writer,
                request_id,
                gateway
                    .configure_default_agent(expected_revision, config)
                    .await,
            )
            .await
        }
        ClientMessage::InstallExtension {
            request_id,
            source,
            reference,
            subdirectory,
        } => {
            write_gateway_result(
                writer,
                request_id,
                gateway
                    .install_extension(source, reference, subdirectory)
                    .await,
            )
            .await
        }
        ClientMessage::UpdateExtension { request_id, id } => {
            write_gateway_result(writer, request_id, gateway.update_extension(id).await).await
        }
        ClientMessage::UninstallExtension { request_id, id } => {
            write_gateway_result(writer, request_id, gateway.uninstall_extension(id).await).await
        }
        ClientMessage::TrustExtensionHooks {
            request_id,
            id,
            expected_digest,
        } => {
            write_gateway_result(
                writer,
                request_id,
                gateway
                    .set_extension_hooks_trusted(id, expected_digest, true)
                    .await,
            )
            .await
        }
        ClientMessage::RevokeExtensionHooksTrust {
            request_id,
            id,
            expected_digest,
        } => {
            write_gateway_result(
                writer,
                request_id,
                gateway
                    .set_extension_hooks_trusted(id, expected_digest, false)
                    .await,
            )
            .await
        }
        ClientMessage::ProbeGitCredential { request_id, target } => {
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
        ClientMessage::ApproveGitCredential {
            request_id,
            target,
            username,
            token,
        } => match gateway
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
        },
        ClientMessage::ListSshIdentities { request_id } => match gateway.ssh_identities().await {
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
        },
        ClientMessage::GenerateSshIdentity { request_id } => {
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
        ClientMessage::GetGitDiff {
            request_id,
            session_id,
            scope,
        } => match require_selected(selected, &session_id) {
            Err(rejection) => write_rejection(writer, request_id, rejection).await,
            Ok(host) => match host.git_diff(scope).await {
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
            },
        },
        ClientMessage::SwitchGitBranch {
            request_id,
            session_id,
            branch,
        } => {
            let host = match require_selected(selected, &session_id) {
                Ok(host) => host,
                Err(rejection) => return write_rejection(writer, request_id, rejection).await,
            };
            write_result(writer, request_id, host.switch_git_branch(branch).await).await
        }
        ClientMessage::ListWorkspaceFiles {
            request_id,
            session_id,
            scope,
        } => {
            let host = match require_selected(selected, &session_id) {
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
        ClientMessage::ReadWorkspaceFile {
            request_id,
            session_id,
            path,
            offset,
            max_bytes,
        } => {
            let host = match require_selected(selected, &session_id) {
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
        ClientMessage::WriteWorkspaceFile {
            request_id,
            session_id,
            path,
            content,
        } => {
            let host = match require_selected(selected, &session_id) {
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
        ClientMessage::ListDirectories {
            request_id,
            path,
            include_files,
        } => {
            let result =
                tokio::task::spawn_blocking(move || list_directories(&path, include_files))
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
        ClientMessage::SetProviderCredential {
            request_id,
            instance,
            provider,
            api_key,
        } => {
            match gateway
                .set_credential(instance.clone(), provider.clone(), api_key, None)
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
        ClientMessage::SetProviderEndpointCredential {
            request_id,
            instance,
            provider,
            base_url,
            api_key,
        } => {
            match gateway
                .set_credential(instance.clone(), provider.clone(), api_key, Some(base_url))
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
        ClientMessage::RegisterProvider {
            request_id,
            config,
            label,
            tint,
            model_ids,
            reasoning_efforts,
            replace_existing_selections,
        } => {
            write_gateway_result(
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
            .await
        }
        ClientMessage::RemoveProvider {
            request_id,
            instance,
        } => {
            write_gateway_result(writer, request_id, gateway.remove_provider(instance).await).await
        }
        ClientMessage::CreatePairingCode { request_id } => match auth.create_pairing_code() {
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
        },
        ClientMessage::StartProviderLogin {
            request_id,
            provider,
        } => {
            write_result(
                writer,
                request_id.clone(),
                gateway.start_provider_login(request_id, provider).await,
            )
            .await
        }
        ClientMessage::GetProfile { request_id } => match gateway.profile().await {
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
        },
        ClientMessage::StartCronSetup {
            request_id,
            session_id,
            task,
        } => {
            let host = match require_selected(selected, &session_id) {
                Ok(host) => host,
                Err(rejection) => return write_rejection(writer, request_id, rejection).await,
            };
            write_result(writer, request_id, host.start_cron_setup(task).await).await
        }
        ClientMessage::ListCron {
            request_id,
            session_id,
        } => match cron.list(&session_id) {
            Ok(tasks) => {
                write_frame(
                    writer,
                    &ServerFrame::new(ServerMessage::CronTasks {
                        request_id,
                        session_id,
                        tasks,
                    }),
                )
                .await
            }
            Err(error) => write_rejection(writer, request_id, cron_rejection(error)).await,
        },
        ClientMessage::RescheduleCron {
            request_id,
            session_id,
            id,
            schedule,
        } => {
            let result = cron
                .reschedule(&session_id, &id, &schedule)
                .map(|_| ())
                .map_err(cron_rejection);
            write_result(writer, request_id, result).await
        }
        ClientMessage::DeleteCron {
            request_id,
            session_id,
            id,
        } => {
            let result = cron
                .delete(&session_id, &id)
                .map(|_| ())
                .map_err(cron_rejection);
            write_result(writer, request_id, result).await
        }
        ClientMessage::RunCron {
            request_id,
            session_id,
            id,
        } => write_result(writer, request_id, gateway.run_cron(session_id, id).await).await,
        ClientMessage::ListCronHistory {
            request_id,
            session_id,
            id,
        } => match cron.history(&session_id, id.as_deref()) {
            Ok(runs) => {
                write_frame(
                    writer,
                    &ServerFrame::new(ServerMessage::CronHistory {
                        request_id,
                        session_id,
                        runs,
                    }),
                )
                .await
            }
            Err(error) => write_rejection(writer, request_id, cron_rejection(error)).await,
        },
    }
}

fn normalize_capability_command(
    message: ClientMessage,
    selected: &Option<SelectedChat>,
) -> std::result::Result<ClientMessage, (String, Rejection)> {
    let (session_id, submission) = match message {
        ClientMessage::Submit {
            session_id,
            submission,
        } => (session_id, submission),
        message => return Ok(message),
    };
    let Op::CapabilityCommand {
        capability,
        command,
        arguments,
        input,
        target,
    } = &submission.op
    else {
        return Ok(ClientMessage::Submit {
            session_id,
            submission,
        });
    };
    let request_id = submission.id.clone();
    if capability != mobius::middleware::cron::MANIFEST.id
        || command != mobius::middleware::cron::MANIFEST.id
    {
        return Ok(ClientMessage::Submit {
            session_id,
            submission,
        });
    }
    validate_submission(&submission).map_err(|error| {
        (
            request_id.clone(),
            Rejection {
                code: "invalid_submission",
                message: error.to_string(),
                fatal: false,
            },
        )
    })?;
    require_selected(selected, &session_id).map_err(|rejection| (request_id.clone(), rejection))?;
    if input.is_some() || target.is_some() {
        return Err((
            request_id,
            Rejection {
                code: "invalid_submission",
                message: "gateway capability commands do not accept input or a message target"
                    .into(),
                fatal: false,
            },
        ));
    }
    crate::cron::command_message(request_id.clone(), session_id, arguments)
        .map_err(|error| (request_id, cron_rejection(error)))
}
