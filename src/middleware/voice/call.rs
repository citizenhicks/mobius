//! One live call: provider media, its private transcript, and workspace handoffs.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use super::transcript::{VoiceHandoff, VoiceTranscript};
use super::{VoiceConversation, delegated_task, instructions, reject_handoff, text};
use crate::agent::ValidatedSubmission;
use crate::backend::checkpoint::CheckpointStore;
use crate::backend::model::{
    ModelRouter, RealtimeVoiceCall, RealtimeVoiceCommand, RealtimeVoiceEvent,
};
use crate::middleware::FrontendEventSink;
use crate::protocol::{Event, EventMsg};
use crate::{Error, Result};

const TRANSCRIPT_FINALIZATION_TIMEOUT: Duration = Duration::from_secs(2);
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// The chat a host attaches one call to.
pub struct VoiceCallContext {
    /// The chat session that owns the call.
    pub session_id: Arc<str>,
    /// The chat's model router, which serves the voice route.
    pub router: Arc<ModelRouter>,
    /// The selected voice route.
    pub voice: String,
    /// The Bot's display name.
    pub bot_name: String,
    /// The Bot's handle.
    pub bot_handle: String,
    /// The Bot's prepared instructions.
    pub bot_instructions: String,
    /// The chat turn running when the call starts.
    pub active_turn_id: Option<String>,
    /// The chat's checkpoint store.
    pub checkpoints: Arc<dyn CheckpointStore>,
    /// The chat's frontend event sink.
    pub frontend: FrontendEventSink,
}

impl VoiceCallContext {
    /// The longest [`VoiceCall::close`] waits for the provider and the transcript.
    /// # Errors
    ///
    /// Returns an error if the voice route is not configured.
    pub fn shutdown_timeout(&self) -> Result<Duration> {
        Ok(self.finalization_timeout()? + TRANSCRIPT_FINALIZATION_TIMEOUT)
    }

    fn finalization_timeout(&self) -> Result<Duration> {
        let settings = self.router.transport_settings_for(&self.voice)?;
        // Provider shutdown allows one I/O window to send close and one to receive final events.
        Ok(Duration::from_millis(settings.voice_io_timeout_ms) * 2)
    }
}

/// Why [`VoiceCall::wait`] returned.
pub struct VoiceWake(Wake);

enum Wake {
    Preview,
    Provider(Option<Result<RealtimeVoiceEvent>>),
}

/// The sole owner of one call's provider media, transcript and handoffs.
pub struct VoiceCall {
    call: RealtimeVoiceCall,
    transcript: VoiceTranscript,
    conversation: VoiceConversation,
    handoffs: BTreeMap<String, VoiceHandoff>,
    router: Arc<ModelRouter>,
    voice: String,
    finalization_timeout: Duration,
}

impl VoiceCall {
    /// Negotiates the provider call, seeded with the chat's durable conversation, and
    /// returns it with the answer SDP for the frontend.
    /// # Errors
    ///
    /// Returns an error if the transcript, the chat, or provider negotiation fails.
    pub async fn start(context: VoiceCallContext, offer_sdp: String) -> Result<(Self, String)> {
        let transcript = VoiceTranscript::open(
            Arc::clone(&context.checkpoints),
            &context.session_id,
            Arc::clone(&context.frontend),
        )
        .await?;
        let parent = context
            .checkpoints
            .load(&context.session_id)
            .await?
            .ok_or_else(|| Error::Checkpoint("voice parent session disappeared".into()))?;
        let identity = format!(
            "{}\n\n{}",
            text::DEFINITION
                .identity
                .replace("{handle}", &context.bot_handle)
                .replace("{name}", &context.bot_name),
            context.bot_instructions
        );
        let instructions = instructions(
            &identity,
            &parent,
            transcript.session_id(),
            &transcript.task_context().await?,
        )?;
        let mut call = context
            .router
            .start_realtime_voice(
                Some(&context.voice),
                parent.session_id,
                offer_sdp,
                instructions,
            )
            .await?;
        let answer_sdp = std::mem::take(&mut call.answer_sdp);
        Ok((Self::attach(context, transcript, call).await?, answer_sdp))
    }

    /// Attaches an already negotiated provider call to the chat's transcript.
    /// # Errors
    ///
    /// Returns an error if the transcript cannot be opened or written.
    pub async fn with_call(context: VoiceCallContext, call: RealtimeVoiceCall) -> Result<Self> {
        let transcript = VoiceTranscript::open(
            Arc::clone(&context.checkpoints),
            &context.session_id,
            Arc::clone(&context.frontend),
        )
        .await?;
        Self::attach(context, transcript, call).await
    }

    async fn attach(
        context: VoiceCallContext,
        mut transcript: VoiceTranscript,
        call: RealtimeVoiceCall,
    ) -> Result<Self> {
        let finalization_timeout = context.finalization_timeout()?;
        transcript.start_call(&call.voice).await?;
        Ok(Self {
            conversation: VoiceConversation::new(
                transcript.session_id().into(),
                context.active_turn_id,
                context.bot_name,
            ),
            call,
            transcript,
            handoffs: BTreeMap::new(),
            router: context.router,
            voice: context.voice,
            finalization_timeout,
        })
    }

    /// Reports whether a later chat context still serves this call's voice.
    #[must_use]
    pub fn serves(&self, context: &VoiceCallContext) -> bool {
        Arc::ptr_eq(&self.router, &context.router) && self.voice == context.voice
    }

    /// Waits for provider speech, a handoff, or a due transcript preview. Cancel-safe.
    pub async fn wait(&mut self) -> VoiceWake {
        VoiceWake(tokio::select! {
            biased;
            () = self.transcript.wait_for_preview() => Wake::Preview,
            event = self.call.events.recv() => Wake::Provider(event),
        })
    }

    /// Handles a wake from [`Self::wait`], submitting a handoff through `submit`, which
    /// returns the rejection message when the chat does not accept it. Returns `false` once
    /// the provider has ended the call.
    /// # Errors
    ///
    /// Returns an error if the provider, the transcript or a reply fails.
    pub async fn handle(
        &mut self,
        wake: VoiceWake,
        submit: impl AsyncFnOnce(ValidatedSubmission) -> std::result::Result<(), String>,
    ) -> Result<bool> {
        let event = match wake.0 {
            Wake::Preview => {
                self.transcript.flush_preview().await?;
                return Ok(true);
            }
            Wake::Provider(None) => return Ok(false),
            Wake::Provider(Some(event)) => event?,
        };
        let replies = match event {
            RealtimeVoiceEvent::Transcript {
                id,
                role,
                text,
                complete,
            } => {
                self.transcript.record(&id, role, &text, complete).await?;
                Vec::new()
            }
            RealtimeVoiceEvent::Handoff { id, text } => {
                if self.conversation.has_handoff(&id) {
                    return Ok(true);
                }
                let context = self.transcript.handoff_context().await?;
                match delegated_task(text.as_deref(), &context.text) {
                    Ok(task) => match self.conversation.handoff(id, task)? {
                        Some(submission) => {
                            let submission_id = submission.submission().id.to_owned();
                            match submit(submission).await {
                                Ok(()) => {
                                    self.handoffs.insert(submission_id, context);
                                    Vec::new()
                                }
                                Err(message) => self.conversation.reject(&submission_id, &message),
                            }
                        }
                        None => Vec::new(),
                    },
                    Err(error) => vec![reject_handoff(id, &error.to_string())],
                }
            }
        };
        self.reply(replies).await?;
        Ok(true)
    }

    /// Settles handoffs from one committed chat event and forwards workspace progress.
    /// # Errors
    ///
    /// Returns an error if the transcript cursor or a reply fails.
    pub async fn observe(&mut self, event: &Event) -> Result<()> {
        if matches!(
            &event.msg,
            EventMsg::Message(_) | EventMsg::SubmissionRejected(_)
        ) && let Some(id) = &event.submission_id
            && let Some(context) = self.handoffs.remove(id.as_ref())
            && matches!(&event.msg, EventMsg::Message(_))
        {
            self.transcript.acknowledge(context).await?;
        }
        let mut commands = self.conversation.observe(event);
        if let Some(update) = self.conversation.progress(event) {
            commands.insert(0, update);
        }
        self.reply(commands).await
    }

    async fn reply(&self, replies: Vec<RealtimeVoiceCommand>) -> Result<()> {
        for reply in replies {
            tokio::time::timeout(REPLY_TIMEOUT, self.call.commands.send(reply))
                .await
                .map_err(|_| Error::Stopped("voice reply timed out".into()))?
                .map_err(|_| Error::Stopped("voice connection stopped accepting replies".into()))?;
        }
        Ok(())
    }

    /// Closes the provider call, keeps its final speech, and combines the call's `result`
    /// with any shutdown failure; a shutdown timeout takes precedence.
    /// # Errors
    ///
    /// Returns a shutdown timeout, `result`'s error, or the first shutdown failure.
    pub async fn close<E: From<Error>>(
        mut self,
        result: std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        let closed = tokio::time::timeout(self.finalization_timeout, async {
            if self
                .call
                .commands
                .send(RealtimeVoiceCommand::Close)
                .await
                .is_ok()
            {
                while let Some(event) = self.call.events.recv().await {
                    if let RealtimeVoiceEvent::Transcript {
                        id,
                        role,
                        text,
                        complete,
                    } = event?
                    {
                        self.transcript.record(&id, role, &text, complete).await?;
                    }
                }
            }
            Ok::<_, Error>(())
        })
        .await
        .map_err(|_| Error::Stopped("voice session finalization timed out".into()));
        let finalized =
            tokio::time::timeout(TRANSCRIPT_FINALIZATION_TIMEOUT, self.transcript.finish())
                .await
                .map_err(|_| Error::Stopped("voice transcript finalization timed out".into()))?;
        result
            .and(closed?.map_err(E::from))
            .and(finalized.map_err(E::from))
    }
}
