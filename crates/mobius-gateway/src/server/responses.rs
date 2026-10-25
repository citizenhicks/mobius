use super::{
    Arc, AsyncWrite, AuthStore, ClientConnections, ClientView, DirectoryEntry, DirectoryListing,
    Error, HostHandle, MAX_DIRECTORY_ENTRIES, Path, PathBuf, Rejection, Result, SelectedChat,
    ServerFrame, ServerMessage, SessionHistoryPage, fs, write_frame,
};
use crate::host::OpenReplay;
use crate::wire::{encode_bounded_frame, write_encoded};
use mobius::SessionId;

pub(super) async fn write_session_history(
    writer: &mut (impl AsyncWrite + Unpin),
    page: std::result::Result<SessionHistoryPage, Rejection>,
    request_id: Arc<str>,
    session_id: SessionId,
) -> Result<()> {
    let page = match page {
        Ok(page) => page,
        Err(rejection) => return write_rejection(writer, request_id, rejection).await,
    };
    let frame = ServerFrame::new(ServerMessage::SessionHistory {
        request_id: Arc::clone(&request_id),
        session_id,
        page: page.page,
        next: page.next,
    });
    if let Some(encoded) = encode_bounded_frame(&frame)? {
        return write_encoded(writer, &encoded).await;
    }
    write_rejection(
        writer,
        request_id,
        Rejection::new(
            "history_turn_too_large",
            "the next durable turn exceeds the gateway frame limit",
        ),
    )
    .await
}

pub(super) async fn write_client_inventory(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    current_client_id: &str,
    auth: &AuthStore,
    client_connections: &ClientConnections,
) -> Result<()> {
    write_frame(
        writer,
        &ServerFrame::new(ServerMessage::Clients {
            request_id,
            current_client_id: current_client_id.into(),
            clients: client_connections.snapshot(auth.clients()?)?,
        }),
    )
    .await
}

pub(super) async fn open_selected(
    writer: &mut (impl AsyncWrite + Unpin),
    selected: &mut Option<SelectedChat>,
    request_id: Arc<str>,
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
            request_id: Arc::clone(&request_id),
            payload: snapshot.ready,
        }),
    )
    .await?;
    match snapshot.replay {
        OpenReplay::Frames(frames) => {
            for frame in frames {
                frame.write(writer).await?;
            }
        }
        OpenReplay::Transcript {
            through_sequence,
            page,
            next,
        } => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::SessionTranscript {
                    request_id: Arc::clone(&request_id),
                    session_id: host.session_id().share(),
                    through_sequence,
                    page,
                    next,
                }),
            )
            .await?;
        }
    }
    write_frame(
        writer,
        &ServerFrame::new(ServerMessage::SessionReplayComplete {
            request_id,
            session_id: host.session_id().share(),
        }),
    )
    .await?;
    *selected = Some(SelectedChat {
        host,
        broadcasts: Some(broadcasts),
        delivered_sequence,
    });
    Ok(())
}

pub(super) fn require_selected<'a>(
    selected: Option<&'a SelectedChat>,
    session_id: &SessionId,
) -> std::result::Result<&'a HostHandle, Rejection> {
    let host = require_any_selected(selected)?;
    if host.session_id() != session_id {
        return Err(Rejection::new(
            "session_not_selected",
            "open this chat on the connection before controlling it",
        ));
    }
    Ok(host)
}

pub(super) async fn require_uploads_enabled<'a>(
    selected: Option<&'a SelectedChat>,
    session_id: &SessionId,
) -> std::result::Result<&'a HostHandle, Rejection> {
    let host = require_selected(selected, session_id)?;
    if !host.accepts_file_attachments().await? {
        return Err(uploads_disabled_rejection());
    }
    Ok(host)
}

pub(super) fn uploads_disabled_rejection() -> Rejection {
    Rejection::new(
        "uploads_disabled",
        "turn on file attachments for this chat first",
    )
}

pub(super) fn require_any_selected(
    selected: Option<&SelectedChat>,
) -> std::result::Result<&HostHandle, Rejection> {
    selected
        .map(|selected| &selected.host)
        .ok_or_else(|| Rejection::new("session_required", "create or open a chat first"))
}

pub(super) async fn write_result(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: impl Into<Arc<str>>,
    result: std::result::Result<(), Rejection>,
) -> Result<()> {
    let request_id = request_id.into();
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
    view: &mut ClientView,
    request_id: Arc<str>,
    result: std::result::Result<crate::wire::ReadyPayload, Rejection>,
) -> Result<()> {
    match result {
        Ok(payload) => {
            view.write_catalog(
                writer,
                ServerFrame::new(ServerMessage::GatewayConfigured {
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
    request_id: impl Into<Arc<str>>,
    rejection: Rejection,
) -> Result<()> {
    write_frame(
        writer,
        &ServerFrame::new(ServerMessage::Rejected {
            request_id: request_id.into(),
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
    message: impl Into<Arc<str>>,
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
    Rejection::new("gateway_error", message)
}

pub(super) fn routine_rejection(error: &Error) -> Rejection {
    Rejection::new("invalid_routine", error.to_string())
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
        .filter_map(std::result::Result::ok)
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
    Rejection::new("invalid_directory", error.to_string())
}

pub(super) fn sequence(frame: &ServerFrame) -> Option<u64> {
    match frame.message {
        ServerMessage::AgentEvent { ref record, .. } => Some(record.sequence),
        _ => None,
    }
}
