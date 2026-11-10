//! The command side of a running agent and its submission queue.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::Error;
use crate::Result;
use crate::backend::session_files::session_file_limits;
use crate::protocol::Op;
use crate::protocol::Submission;

pub(super) const SUBMISSION_QUEUE_CAPACITY: usize = 64;
const MAX_OPERATION_BYTES: usize = 256;

/// Cloneable command side of a running agent.
///
/// [`Self::send`] and [`Self::submit`] never wait for queue capacity: they return
/// [`Error::Busy`] when full and [`Error::Stopped`] when disconnected. Successful
/// enqueueing does not mean the operation was accepted; consume the event stream
/// for its correlated outcome.
///
/// Dropping all strong senders requests shutdown. Use [`Agent::into_parts`](super::Agent::into_parts) to
/// release the command half while draining [`AgentEvents`](super::AgentEvents) until it closes.
#[derive(Clone)]
pub struct AgentSender {
    pub(super) ingress: Arc<SubmissionIngress>,
}

impl std::fmt::Debug for AgentSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentSender")
            .finish_non_exhaustive()
    }
}

/// Durable outcome of admitting a message to its session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageAcceptance {
    /// The message was committed to the durable input queue.
    Accepted,
    /// This session already admitted the submission ID, including before restart.
    AlreadyAccepted,
}

/// Completion handle for a message's durable admission, separate from its turn.
#[derive(Debug)]
#[must_use = "wait for durable admission, or use AgentSender::send without an acknowledgment"]
pub struct MessageAdmission {
    receiver: oneshot::Receiver<Result<MessageAcceptance>>,
}

impl MessageAdmission {
    /// Waits until the message is committed, rejected, or the runtime stops.
    /// # Errors
    ///
    /// Returns an error when admission fails or the runtime stops before acknowledging it.
    pub async fn wait(self) -> Result<MessageAcceptance> {
        self.receiver.await.map_err(|error| {
            Error::Stopped(format!("agent stopped before message admission: {error}"))
        })?
    }
}

pub(super) type AdmissionReply = oneshot::Sender<Result<MessageAcceptance>>;

/// Non-owning command handle for middleware that must address its current agent.
#[derive(Clone)]
pub struct WeakAgentSender {
    ingress: Weak<SubmissionIngress>,
}

impl std::fmt::Debug for WeakAgentSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WeakAgentSender")
            .field("live", &(self.ingress.strong_count() > 0))
            .finish_non_exhaustive()
    }
}

/// A submission checked at ingress and kept immutable until it reaches the agent.
#[derive(Debug)]
pub struct ValidatedSubmission(Submission);

impl ValidatedSubmission {
    /// Checks a raw submission before any admission side effects.
    /// # Errors
    /// Returns an error when identifiers, message content, or operation fields are invalid.
    pub fn new(submission: Submission) -> Result<Self> {
        validate_submission(&submission)?;
        Ok(Self(submission))
    }

    /// Borrows the checked operation without allowing it to be changed.
    #[must_use]
    pub fn submission(&self) -> &Submission {
        &self.0
    }

    /// Recovers raw input for rewriting; the rewritten value must be checked again.
    #[must_use]
    pub fn into_submission(self) -> Submission {
        self.0
    }

    /// Asks a checked message to queue behind active work; delivery is not a validated field.
    #[must_use]
    pub fn with_queued_delivery(mut self) -> Self {
        if let Op::Message { message } = &mut self.0.op {
            message.requested_delivery = Some(crate::protocol::ActiveMessageDelivery::Queue);
        }
        self
    }
}

impl AgentSender {
    /// Returns a handle that does not keep the agent submission channel open.
    #[must_use]
    pub fn downgrade(&self) -> WeakAgentSender {
        WeakAgentSender {
            ingress: Arc::downgrade(&self.ingress),
        }
    }

    /// Sends a submission with a caller-controlled correlation ID.
    /// # Errors
    ///
    /// Returns an error if the transport fails or returns invalid data.
    pub fn send(&self, submission: Submission) -> Result<()> {
        self.send_validated(ValidatedSubmission::new(submission)?)
    }

    /// Enqueues a submission already checked at its ingress boundary.
    /// # Errors
    /// Returns an error if the queue is closed, full, or unavailable.
    pub fn send_validated(&self, submission: ValidatedSubmission) -> Result<()> {
        self.enqueue(submission, None)
    }

    /// Sends a message through the normal ingress and returns its durable admission handle.
    ///
    /// Acknowledgment covers input admission, not completion of the resulting turn.
    /// # Errors
    ///
    /// Returns an error for non-message operations, invalid input, or a full or closed queue.
    pub fn send_with_admission(&self, submission: Submission) -> Result<MessageAdmission> {
        self.send_validated_with_admission(ValidatedSubmission::new(submission)?)
    }

    /// Enqueues a checked message and returns its durable admission handle.
    /// # Errors
    /// Returns an error for a non-message operation or an unavailable queue.
    pub fn send_validated_with_admission(
        &self,
        submission: ValidatedSubmission,
    ) -> Result<MessageAdmission> {
        if !matches!(submission.submission().op, Op::Message { .. }) {
            return Err(Error::Config(
                "admission acknowledgments require a message".into(),
            ));
        }
        let (reply, receiver) = oneshot::channel();
        self.enqueue(submission, Some(reply))?;
        Ok(MessageAdmission { receiver })
    }

    fn enqueue(
        &self,
        submission: ValidatedSubmission,
        admission: Option<AdmissionReply>,
    ) -> Result<()> {
        let submission = submission.0;
        let mut last_sequence =
            self.ingress.last_sequence.lock().map_err(|error| {
                Error::Stopped(format!("agent submission queue poisoned: {error}"))
            })?;
        let sequence = last_sequence
            .checked_add(1)
            .ok_or_else(|| Error::Busy("agent submission sequence exhausted".into()))?;
        self.ingress
            .sender
            .try_send(SequencedSubmission {
                sequence,
                submission,
                admission,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    Error::Busy("agent submission queue is full".into())
                }
                mpsc::error::TrySendError::Closed(_) => {
                    Error::Stopped("agent submission channel closed".into())
                }
            })?;
        *last_sequence = sequence;
        drop(last_sequence);
        Ok(())
    }

    /// Submits a command and returns its correlation ID.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn submit(&self, op: Op) -> Result<Arc<str>> {
        let id: Arc<str> = Uuid::new_v4().to_string().into();
        self.send(Submission {
            id: Arc::clone(&id),
            op,
        })?;
        Ok(id)
    }
}

impl WeakAgentSender {
    /// Returns a live sender while another owner keeps the agent connected.
    #[must_use]
    pub fn upgrade(&self) -> Option<AgentSender> {
        self.ingress
            .upgrade()
            .map(|ingress| AgentSender { ingress })
    }
}

pub(super) struct SubmissionIngress {
    sender: mpsc::Sender<SequencedSubmission>,
    last_sequence: Arc<Mutex<u64>>,
}

struct SequencedSubmission {
    sequence: u64,
    submission: Submission,
    admission: Option<AdmissionReply>,
}

pub(super) struct ReceivedSubmission {
    pub(super) submission: Submission,
    pub(super) admission: Option<AdmissionReply>,
}

pub(super) struct SubmissionInbox {
    receiver: mpsc::Receiver<SequencedSubmission>,
    last_sent_sequence: Arc<Mutex<u64>>,
    pub(super) last_sequence: u64,
}

impl SubmissionInbox {
    pub(super) async fn recv(&mut self) -> Option<ReceivedSubmission> {
        let queued = self.receiver.recv().await?;
        self.last_sequence = queued.sequence;
        Some(ReceivedSubmission {
            submission: queued.submission,
            admission: queued.admission,
        })
    }

    pub(super) fn cutoff(&self) -> Result<u64> {
        self.last_sent_sequence
            .lock()
            .map(|sequence| *sequence)
            .map_err(|error| Error::Stopped(format!("agent submission queue poisoned: {error}")))
    }
}

pub(super) fn submission_channel(capacity: usize) -> (AgentSender, SubmissionInbox) {
    let (sender, receiver) = mpsc::channel(capacity);
    let last_sequence = Arc::new(Mutex::new(0));
    let ingress = Arc::new(SubmissionIngress {
        sender,
        last_sequence: Arc::clone(&last_sequence),
    });
    (
        AgentSender { ingress },
        SubmissionInbox {
            receiver,
            last_sent_sequence: last_sequence,
            last_sequence: 0,
        },
    )
}

#[cfg(test)]
pub(crate) fn test_sender() -> WeakAgentSender {
    submission_channel(1).0.downgrade()
}

/// Validates one submission before callers perform more expensive boundary work.
/// # Errors
///
/// Returns an error if the supplied value is invalid.
pub fn validate_submission(submission: &Submission) -> Result<()> {
    crate::validate_identifier("submission ID", &submission.id, crate::MAX_IDENTIFIER_BYTES)?;
    match &submission.op {
        Op::Message { message } => message.validate(session_file_limits()),
        Op::Interrupt { turn_id } => {
            crate::validate_identifier("turn ID", turn_id, crate::MAX_IDENTIFIER_BYTES)
        }
        Op::ExecApproval { id, .. } => {
            crate::validate_identifier("approval ID", id, crate::MAX_IDENTIFIER_BYTES)
        }
        Op::CapabilityCommand {
            capability,
            command,
            arguments,
            input,
            ..
        } => {
            crate::validate_identifier("capability ID", capability, MAX_OPERATION_BYTES)?;
            crate::validate_identifier("command", command, MAX_OPERATION_BYTES)?;
            if arguments.len() > crate::protocol::MAX_CAPABILITY_INPUT_BYTES {
                return Err(Error::Config(
                    "middleware command arguments exceed size limit".into(),
                ));
            }
            if input
                .as_ref()
                .is_some_and(|input| input.len() > crate::protocol::MAX_CAPABILITY_INPUT_BYTES)
            {
                return Err(Error::Config(
                    "middleware command input exceeds size limit".into(),
                ));
            }
            Ok(())
        }
        Op::SetModel { route } => {
            crate::validate_identifier("model route", route, crate::MAX_IDENTIFIER_BYTES)
        }
        Op::ResumeSession { .. } => Ok(()),
    }
}
