use super::*;

pub(super) async fn write_session_history(
    writer: &mut (impl AsyncWrite + Unpin),
    host: &HostHandle,
    request_id: String,
    session_id: String,
    before_sequence: Option<u64>,
) -> Result<()> {
    let page = match host.history_page(before_sequence).await {
        Ok(page) => page,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    let frame = ServerFrame::new(ServerMessage::SessionHistory {
        request_id: request_id.clone(),
        session_id,
        records: page.records,
        next_before_sequence: page.next_before_sequence,
    });
    if encoded_frame_fits(&frame)? {
        return write_frame(writer, &frame).await;
    }
    write_rejection(
        writer,
        request_id,
        Rejection {
            code: "history_turn_too_large",
            message: "the next durable turn exceeds the gateway frame limit".into(),
            fatal: false,
        },
    )
    .await
}

pub(super) fn encoded_frame_fits(frame: &ServerFrame) -> Result<bool> {
    Ok(serde_json::to_vec(frame)?.len() <= MAX_FRAME_BYTES)
}

pub(super) async fn write_client_inventory(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    current_client_id: &str,
    auth: &AuthStore,
    client_connections: &ClientConnections,
) -> Result<()> {
    write_frame(
        writer,
        &ServerFrame::new(ServerMessage::Clients {
            request_id,
            current_client_id: current_client_id.into(),
            clients: client_connections.snapshot(&auth.clients()?)?,
        }),
    )
    .await
}

pub(super) async fn open_selected(
    writer: &mut (impl AsyncWrite + Unpin),
    selected: &mut Option<SelectedChat>,
    request_id: String,
    host: HostHandle,
    last_sequence: Option<u64>,
) -> Result<()> {
    let broadcasts = host.subscribe();
    let snapshot = match host.snapshot(last_sequence).await {
        Ok(snapshot) => snapshot,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    let delivered_sequence = snapshot.ready.latest_sequence;
    write_frame(
        writer,
        &ServerFrame::new(ServerMessage::SessionOpened {
            request_id: request_id.clone(),
            payload: snapshot.ready,
        }),
    )
    .await?;
    for frame in snapshot.replay {
        write_frame(writer, &frame).await?;
    }
    write_frame(
        writer,
        &ServerFrame::new(ServerMessage::SessionReplayComplete {
            request_id,
            session_id: host.session_id().into(),
        }),
    )
    .await?;
    *selected = Some(SelectedChat {
        host,
        broadcasts,
        delivered_sequence,
    });
    Ok(())
}

pub(super) fn require_selected<'a>(
    selected: &'a Option<SelectedChat>,
    session_id: &str,
) -> std::result::Result<&'a HostHandle, Rejection> {
    let host = require_any_selected(selected)?;
    if host.session_id() != session_id {
        return Err(Rejection {
            code: "session_not_selected",
            message: "open this chat on the connection before controlling it".into(),
            fatal: false,
        });
    }
    Ok(host)
}

pub(super) fn require_uploads_enabled<'a>(
    selected: &'a Option<SelectedChat>,
    session_id: &str,
) -> std::result::Result<&'a HostHandle, Rejection> {
    let host = require_selected(selected, session_id)?;
    if !host.accepts_file_attachments() {
        return Err(uploads_disabled_rejection());
    }
    Ok(host)
}

pub(super) fn uploads_disabled_rejection() -> Rejection {
    Rejection {
        code: "uploads_disabled",
        message: "enable the optional attachments middleware for this chat first".into(),
        fatal: false,
    }
}

pub(super) fn require_any_selected(
    selected: &Option<SelectedChat>,
) -> std::result::Result<&HostHandle, Rejection> {
    selected
        .as_ref()
        .map(|selected| &selected.host)
        .ok_or_else(|| Rejection {
            code: "session_required",
            message: "create or open a chat first".into(),
            fatal: false,
        })
}

pub(super) async fn write_result(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    result: std::result::Result<(), Rejection>,
) -> Result<()> {
    match result {
        Ok(()) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::Accepted { request_id }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

pub(super) async fn write_gateway_result(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    result: std::result::Result<crate::wire::ReadyPayload, Rejection>,
) -> Result<()> {
    match result {
        Ok(payload) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::GatewayConfigured {
                    request_id,
                    payload,
                }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

pub(super) async fn write_rejection(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: String,
    rejection: Rejection,
) -> Result<()> {
    write_frame(
        writer,
        &ServerFrame::new(ServerMessage::Rejected {
            request_id,
            code: rejection.code.into(),
            message: rejection.message,
            fatal: rejection.fatal,
        }),
    )
    .await
}

pub(super) async fn write_server_error(
    writer: &mut (impl AsyncWrite + Unpin),
    code: impl Into<String>,
    message: impl Into<String>,
    fatal: bool,
) -> Result<()> {
    write_frame(
        writer,
        &ServerFrame::new(ServerMessage::Error {
            code: code.into(),
            message: message.into(),
            fatal,
        }),
    )
    .await
}

pub(super) fn internal_rejection(message: String) -> Rejection {
    Rejection {
        code: "gateway_error",
        message,
        fatal: false,
    }
}

pub(super) fn session_file_rejection(error: impl std::fmt::Display) -> Rejection {
    Rejection {
        code: "session_file_rejected",
        message: error.to_string(),
        fatal: false,
    }
}

pub(super) fn routine_rejection(error: Error) -> Rejection {
    Rejection {
        code: "invalid_routine",
        message: error.to_string(),
        fatal: false,
    }
}

pub(super) fn list_directories(
    path: &Path,
    include_files: bool,
) -> std::result::Result<DirectoryListing, Rejection> {
    let path = fs::canonicalize(path).map_err(directory_rejection)?;
    if !path.is_dir() {
        return Err(directory_rejection("path is not a directory"));
    }
    let mut entries = fs::read_dir(&path)
        .map_err(directory_rejection)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            if name == ".git" {
                return None;
            }
            let is_directory = entry.path().is_dir();
            if !is_directory && !include_files {
                return None;
            }
            Some(DirectoryEntry {
                name: name.to_owned(),
                path: entry.path().to_str().map(PathBuf::from)?,
                is_directory,
            })
        })
        .take(MAX_DIRECTORY_ENTRIES)
        .collect::<Vec<_>>();
    entries.sort_unstable_by_key(|entry| entry.name.to_lowercase());
    Ok(DirectoryListing {
        parent: path.parent().map(Path::to_path_buf),
        path,
        entries,
    })
}

pub(super) fn directory_rejection(error: impl std::fmt::Display) -> Rejection {
    Rejection {
        code: "invalid_directory",
        message: error.to_string(),
        fatal: false,
    }
}

pub(super) fn sequence(frame: &ServerFrame) -> Option<u64> {
    match frame.message {
        ServerMessage::AgentEvent { ref record, .. } => Some(record.sequence),
        _ => None,
    }
}
