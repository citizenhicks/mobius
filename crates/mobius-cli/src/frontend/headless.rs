use std::sync::Arc;

use mobius::protocol::{
    EventMsg, MessageAuthor, MessageSubmission, ModelStepContentPhase, Op, ReviewDecision,
    Submission,
};
use mobius::{Error, Result};
use mobius_gateway::client::{GatewayEvents, GatewaySender};
use mobius_gateway::wire::{ClientMessage, ServerMessage};

use crate::gateway_error;

/// Runs one task through the headless gateway client.
/// # Errors
///
/// Returns an error if validation or an operation required to complete the request fails.
pub async fn run(
    sender: GatewaySender,
    mut events: GatewayEvents,
    session_id: Arc<str>,
    task: String,
) -> Result<Option<String>> {
    let submission_id: Arc<str> = crate::fresh_id();
    sender
        .send(ClientMessage::Submit {
            session_id: Arc::clone(&session_id),
            submission: Submission {
                id: Arc::clone(&submission_id),
                op: Op::Message {
                    message: MessageSubmission {
                        author: MessageAuthor::User,
                        text: task,
                        attachments: Vec::new(),
                        reply: None,
                        requested_delivery: None,
                        target_turn_id: None,
                    },
                },
            },
        })
        .await
        .map_err(gateway_error)?;

    let mut turn_id = None;
    let mut first_error = None;
    let mut approval_error = None;
    let mut last_assistant_message = None;
    loop {
        let frame =
            events.next().await.map_err(gateway_error)?.ok_or_else(|| {
                Error::Stopped("gateway disconnected before turn completion".into())
            })?;
        if let Some(error) = frame.message.response_error(Some(&submission_id)) {
            return Err(Error::Stopped(error.message.into()));
        }
        let event = match frame.message {
            ServerMessage::AgentEvent {
                session_id: actual,
                record,
                ..
            } if *actual == *session_id => record.event,
            _ => continue,
        };
        match event.msg {
            EventMsg::TurnStarted(turn)
                if event.submission_id.as_deref() == Some(&submission_id) =>
            {
                turn_id = Some(turn.turn_id);
            }
            EventMsg::ExecApprovalRequest(request)
                if turn_id.as_deref() == Some(&*request.turn_id) =>
            {
                approval_error = Some(Error::Config(
                    "headless run requested tool approval; configure a no-prompt gateway sandbox policy"
                        .into(),
                ));
                sender
                    .send(ClientMessage::Submit {
                        session_id: Arc::clone(&session_id),
                        submission: Submission {
                            id: crate::fresh_id(),
                            op: Op::ExecApproval {
                                id: request.id,
                                decision: ReviewDecision::Abort,
                            },
                        },
                    })
                    .await
                    .map_err(gateway_error)?;
            }
            EventMsg::Error(error) if event.submission_id.as_ref() == Some(&submission_id) => {
                first_error.get_or_insert(error.message);
            }
            EventMsg::AssistantMessage(message)
                if turn_id.as_deref() == Some(message.turn_id.as_ref()) =>
            {
                let final_answer = message
                    .content
                    .into_iter()
                    .filter(|item| item.phase == ModelStepContentPhase::FinalAnswer)
                    .map(|item| item.text)
                    .collect::<String>();
                if !final_answer.is_empty() {
                    last_assistant_message = Some(final_answer);
                }
            }
            EventMsg::TurnComplete(turn) if turn_id.as_deref() == Some(&*turn.turn_id) => {
                if let Some(error) = approval_error {
                    return Err(error);
                }
                if let Some(error) = first_error {
                    return Err(Error::Stopped(error.to_string()));
                }
                return Ok(last_assistant_message);
            }
            EventMsg::TurnAborted(turn) if turn_id.as_deref() == Some(&*turn.turn_id) => {
                return Err(approval_error.unwrap_or_else(|| {
                    Error::Stopped(first_error.unwrap_or(turn.reason).to_string())
                }));
            }
            _ => {}
        }
    }
}
