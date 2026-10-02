use super::*;
use crate::computer_runtime::remote_desktop::DesktopStream;

pub(super) struct PendingControl {
    request_id: String,
    future: std::pin::Pin<Box<dyn Future<Output = std::result::Result<(), Rejection>> + Send>>,
}

pub(super) enum DesktopUpdate {
    Data(Result<Option<Vec<u8>>>),
    Changed(std::result::Result<(), broadcast::error::RecvError>),
    Control(String, std::result::Result<(), Rejection>),
}

pub(super) async fn next_update(
    stream: &mut Option<DesktopStream>,
    changed: &mut broadcast::Receiver<()>,
    control: &mut Option<PendingControl>,
) -> DesktopUpdate {
    tokio::select! {
        data = next_data(stream) => DesktopUpdate::Data(data),
        changed = changed.recv() => DesktopUpdate::Changed(changed),
        (id, result) = next_control(control) => DesktopUpdate::Control(id, result),
    }
}

pub(super) async fn write_update(
    update: DesktopUpdate,
    gateway: &GatewayHost,
    client: &AuthenticatedClient<'_>,
    pending_requests: &mut JoinSet<ServerMessage>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    match update {
        DesktopUpdate::Data(data) => write_data(data, writer).await,
        DesktopUpdate::Changed(changed) => {
            write_change(
                changed,
                gateway,
                client.connection_id,
                client.desktop_transport,
                pending_requests,
                writer,
            )
            .await
        }
        DesktopUpdate::Control(id, result) => {
            write_control_result(gateway, client.connection_id, id, result, writer).await
        }
    }
}

pub(super) async fn next_control(
    pending: &mut Option<PendingControl>,
) -> (String, std::result::Result<(), Rejection>) {
    let Some(control) = pending.as_mut() else {
        return std::future::pending().await;
    };
    let result = control.future.as_mut().await;
    let request_id = std::mem::take(&mut control.request_id);
    pending.take();
    (request_id, result)
}

pub(super) async fn next_data(stream: &mut Option<DesktopStream>) -> Result<Option<Vec<u8>>> {
    match stream {
        Some(stream) => stream.read().await,
        None => std::future::pending().await,
    }
}

pub(super) async fn write_control_state(
    gateway: &GatewayHost,
    connection: Uuid,
    request_id: Option<String>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    let (enabled, is_owner, session_id) = gateway.remote_desktop.control_state(connection);
    write_frame(
        writer,
        &ServerFrame::new(ServerMessage::DesktopControlState {
            request_id,
            enabled,
            is_owner,
            session_id,
        }),
    )
    .await
}

pub(super) async fn write_control_result(
    gateway: &GatewayHost,
    connection: Uuid,
    request_id: String,
    result: std::result::Result<(), Rejection>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    match result {
        Ok(()) => write_control_state(gateway, connection, Some(request_id), writer).await,
        Err(error) => write_result(writer, request_id, Err(error)).await,
    }
}

pub(super) async fn write_change(
    changed: std::result::Result<(), broadcast::error::RecvError>,
    gateway: &GatewayHost,
    connection: Uuid,
    encrypted: bool,
    pending_requests: &mut JoinSet<ServerMessage>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    if changed.is_ok() || matches!(changed, Err(broadcast::error::RecvError::Lagged(_))) {
        if gateway.remote_desktop.check_execution().is_err() {
            pending_requests.abort_all();
        }
        if encrypted {
            write_control_state(gateway, connection, None, writer).await?;
        }
    }
    Ok(())
}

pub(super) async fn write_data(
    data: Result<Option<Vec<u8>>>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    match data? {
        Some(data) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::DesktopData { data }),
            )
            .await
        }
        None => Err(Error::Protocol("desktop stream ended".into())),
    }
}

pub(super) async fn handle_message(
    message: ClientMessage,
    gateway: &GatewayHost,
    client: &AuthenticatedClient<'_>,
    connection: &mut ConnectionSessionState<'_>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<Option<ClientMessage>> {
    match message {
        ClientMessage::OpenComputer {
            request_id,
            session_id,
        } => {
            if !computer_available(gateway, client) {
                write_result(writer, request_id, Err(desktop_unavailable())).await?;
            } else if connection.requests.len() >= super::dispatch::MAX_PENDING_REQUESTS {
                write_result(
                    writer,
                    request_id,
                    Err(Rejection {
                        code: "computer_busy",
                        message: "connection requests are already in progress; try again shortly"
                            .into(),
                        fatal: false,
                    }),
                )
                .await?;
            } else {
                // The same connection must keep pumping the renderer's page request and reply.
                let gateway = gateway.clone();
                connection.requests.spawn(async move {
                    match gateway.open_computer(&session_id).await {
                        Ok(()) => ServerMessage::ComputerOpened {
                            request_id,
                            session_id,
                        },
                        Err(error) => ServerMessage::Rejected {
                            request_id,
                            code: error.code.into(),
                            message: error.message,
                            fatal: error.fatal,
                        },
                    }
                });
            }
        }
        ClientMessage::SetDesktopStream {
            request_id,
            enabled,
        } => {
            if !client.desktop_transport || !gateway.remote_desktop.available() {
                write_result(writer, request_id, Err(desktop_unavailable())).await?;
            } else {
                if enabled && connection.remote_desktop.is_none() {
                    match gateway.remote_desktop.stream(client.connection_id).await {
                        Ok(stream) => *connection.remote_desktop = Some(stream),
                        Err(error) => {
                            write_result(
                                writer,
                                request_id,
                                Err(Rejection {
                                    code: "desktop_unavailable",
                                    message: error.to_string(),
                                    fatal: false,
                                }),
                            )
                            .await?;
                            return Ok(None);
                        }
                    }
                } else if !enabled {
                    connection.pending_desktop_control.take();
                    gateway
                        .remote_desktop
                        .release_control(client.connection_id)
                        .await?;
                    connection.remote_desktop.take();
                }
                write_frame(
                    writer,
                    &ServerFrame::new(ServerMessage::DesktopStreamState {
                        request_id,
                        enabled,
                    }),
                )
                .await?;
                write_control_state(gateway, client.connection_id, None, writer).await?;
            }
        }
        ClientMessage::DesktopData { data } => {
            if !client.desktop_transport || connection.remote_desktop.is_none() {
                write_server_error(
                    writer,
                    "desktop_unavailable",
                    "open an encrypted desktop stream first",
                    false,
                )
                .await?;
            } else if let Some(stream) = connection.remote_desktop.as_mut() {
                stream.write(&data).await?;
            }
        }
        ClientMessage::SetDesktopControl {
            request_id,
            enabled,
            session_id,
        } => {
            if !client.desktop_transport || connection.remote_desktop.is_none() {
                write_result(writer, request_id, Err(desktop_unavailable())).await?;
            } else {
                if enabled {
                    if connection.pending_desktop_control.is_some() {
                        write_result(
                            writer,
                            request_id,
                            Err(Rejection {
                                code: "desktop_busy",
                                message: "desktop control is already pending".into(),
                                fatal: false,
                            }),
                        )
                        .await?;
                    } else {
                        let gateway = gateway.clone();
                        let connection_id = client.connection_id;
                        *connection.pending_desktop_control = Some(PendingControl {
                            request_id,
                            future: Box::pin(async move {
                                gateway
                                    .set_desktop_control(connection_id, true, session_id)
                                    .await
                            }),
                        });
                    }
                    return Ok(None);
                }
                connection.pending_desktop_control.take();
                let result = gateway
                    .set_desktop_control(client.connection_id, enabled, session_id)
                    .await;
                write_control_result(gateway, client.connection_id, request_id, result, writer)
                    .await?;
            }
        }
        message => return Ok(Some(message)),
    }
    Ok(None)
}

fn desktop_unavailable() -> Rejection {
    Rejection {
        code: "desktop_unavailable",
        message: "desktop streaming requires an enabled Linux desktop and an encrypted connection"
            .into(),
        fatal: false,
    }
}

fn computer_available(gateway: &GatewayHost, client: &AuthenticatedClient<'_>) -> bool {
    if gateway.remote_desktop.available() {
        client.desktop_transport
    } else {
        client.local && gateway.remote_desktop.browser_available()
    }
}
