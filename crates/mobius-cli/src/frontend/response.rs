use std::ops::ControlFlow;

use mobius::{Error, Result};
use mobius_gateway::client::GatewayEvents;
use mobius_gateway::wire::{ReadyPayload, ServerFrame, ServerMessage, apply_session_changes};

use crate::gateway_error;

/// Waits for one correlated response while preserving unrelated events and catalog updates.
/// # Errors
/// Returns transport errors, matching rejections, fatal errors, or disconnection.
pub async fn await_response<T>(
    events: &mut GatewayEvents,
    request_id: Option<&str>,
    mut gateway: Option<&mut ReadyPayload>,
    mut response: impl FnMut(ServerMessage) -> ControlFlow<T, ServerMessage>,
) -> Result<T> {
    let mut events = events.scoped();
    loop {
        let ServerFrame { version, message } =
            events.next().await.map_err(gateway_error)?.ok_or_else(|| {
                Error::Stopped("gateway disconnected while awaiting a response".into())
            })?;
        if request_id.is_none()
            && let ServerMessage::Rejected { message, .. } = message
        {
            return Err(Error::Stopped(message));
        }
        if let Some(error) = message.response_error(request_id) {
            return Err(Error::Stopped(error.message.into()));
        }
        let message = match response(message) {
            ControlFlow::Break(value) => return Ok(value),
            ControlFlow::Continue(message) => message,
        };
        match (gateway.as_deref_mut(), message) {
            (Some(gateway), ServerMessage::Ready { payload }) => gateway.update(payload),
            (Some(gateway), ServerMessage::Sessions { sessions, .. }) => {
                gateway.sessions = sessions
            }
            (Some(gateway), ServerMessage::SessionsChanged { sessions, .. }) => {
                apply_session_changes(&mut gateway.sessions, sessions);
            }
            (_, message) => events
                .defer(ServerFrame { version, message })
                .map_err(gateway_error)?,
        }
    }
}

/// Waits for the gateway's initial catalog.
/// # Errors
/// Returns a rejection, fatal error, transport failure, or disconnection.
pub async fn wait_ready(events: &mut GatewayEvents) -> Result<ReadyPayload> {
    await_response(events, None, None, |message| match message {
        ServerMessage::Ready { payload } => ControlFlow::Break(payload),
        message => ControlFlow::Continue(message),
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobius_gateway::client::GatewayClient;
    use mobius_gateway::wire::{ClientFrame, ClientKind, read_frame, write_frame};

    #[tokio::test]
    async fn response_wait_preserves_unrelated_frames_and_correlates_rejections() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let endpoint = format!("tcp://{}", listener.local_addr().expect("address"))
            .parse()
            .expect("endpoint");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let _: ClientFrame =
                read_frame(&mut mobius_gateway::wire::FrameReader::new(&mut stream))
                    .await
                    .expect("authentication")
                    .expect("frame");
            for message in [
                ServerMessage::Authenticated,
                ServerMessage::Rejected {
                    request_id: "other".into(),
                    code: "invalid".into(),
                    message: "unrelated".into(),
                    fatal: false,
                },
                ServerMessage::Accepted {
                    request_id: "owned".into(),
                },
                ServerMessage::Rejected {
                    request_id: "failed".into(),
                    code: "invalid".into(),
                    message: "matching rejection".into(),
                    fatal: false,
                },
            ] {
                write_frame(&mut stream, &ServerFrame::new(message))
                    .await
                    .expect("write");
            }
        });
        let client = GatewayClient::connect(&endpoint, "test", ClientKind::Cli)
            .await
            .expect("connect");
        let (_, mut events) = client.into_parts();
        await_response(&mut events, Some("owned"), None, |message| match message {
            ServerMessage::Accepted { request_id } if request_id == "owned" => {
                ControlFlow::Break(())
            }
            message => ControlFlow::Continue(message),
        })
        .await
        .expect("matching response");
        assert!(
            matches!(events.next().await.expect("event").expect("deferred").message,
            ServerMessage::Rejected { request_id, .. } if request_id == "other")
        );
        let error = await_response::<()>(&mut events, Some("failed"), None, ControlFlow::Continue)
            .await
            .expect_err("matching rejection");
        assert!(error.to_string().contains("matching rejection"));
        server.await.expect("server");
    }
}
