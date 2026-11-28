//! One live call: provider media, its private transcript, and workspace handoffs.

use crate::SessionId;
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
    pub session_id: SessionId,
    /// The chat's model router, which serves the voice route.
    pub router: Arc<ModelRouter>,
    /// The selected voice route.
    pub voice: Arc<str>,
    /// The Bot's display name.
    pub bot_name: Arc<str>,
    /// The Bot's handle.
    pub bot_handle: Arc<str>,
    /// The Bot's prepared instructions.
    pub bot_instructions: String,
    /// The chat turn running when the call starts.
    pub active_turn_id: Option<Arc<str>>,
    /// The chat's checkpoint store.
    pub checkpoints: Arc<dyn CheckpointStore>,
    /// The chat's frontend event sink.
    pub frontend: FrontendEventSink,
}

impl std::fmt::Debug for VoiceCallContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VoiceCallContext")
            .field("session_id", &self.session_id)
            .field("voice", &self.voice)
            .field("bot_name", &self.bot_name)
            .field("active_turn_id", &self.active_turn_id)
            .finish_non_exhaustive()
    }
}

impl VoiceCallContext {
    /// The longest [`VoiceCall::close`] waits for the provider and the transcript.
    /// # Errors
    ///
    /// Returns an error if the voice route is not configured.
    pub fn shutdown_timeout(&self) -> Result<Duration> {
        Ok(finalization_timeout(&self.router, &self.voice)? + TRANSCRIPT_FINALIZATION_TIMEOUT)
    }
}

fn finalization_timeout(router: &ModelRouter, voice: &str) -> Result<Duration> {
    let settings = router.transport_settings_for(voice)?;
    // Provider shutdown allows one I/O window to send close and one to receive final events.
    Ok(Duration::from_millis(settings.voice_io_timeout_ms) * 2)
}

/// Why [`VoiceCall::wait`] returned.
#[derive(Debug)]
pub struct VoiceWake(Wake);

#[derive(Debug)]
enum Wake {
    Preview,
    Provider(Option<Result<RealtimeVoiceEvent>>),
}

/// The sole owner of one call's provider media, transcript and handoffs.
pub struct VoiceCall {
    call: RealtimeVoiceCall,
    transcript: VoiceTranscript,
    conversation: VoiceConversation,
    handoffs: BTreeMap<Arc<str>, VoiceHandoff>,
    router: Arc<ModelRouter>,
    voice: Arc<str>,
    finalization_timeout: Duration,
}

impl std::fmt::Debug for VoiceCall {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VoiceCall")
            .field("transcript", &self.transcript)
            .field("handoffs", &self.handoffs)
            .field("voice", &self.voice)
            .finish_non_exhaustive()
    }
}

impl VoiceCall {
    /// Negotiates the provider call, seeded with the chat's durable conversation, and
    /// returns it with the answer SDP for the frontend.
    /// # Errors
    ///
    /// Returns an error if the transcript, the chat, or provider negotiation fails.
    pub async fn start(context: VoiceCallContext, offer_sdp: String) -> Result<(Self, String)> {
        let VoiceCallContext {
            session_id,
            router,
            voice,
            bot_name,
            bot_handle,
            bot_instructions,
            active_turn_id,
            checkpoints,
            frontend,
        } = context;
        let transcript = VoiceTranscript::open(checkpoints, &session_id, frontend).await?;
        let parent = transcript
            .checkpoints()
            .load(&session_id)
            .await?
            .ok_or_else(|| Error::Checkpoint("voice parent session disappeared".into()))?;
        let identity = format!(
            "{}\n\n{}",
            text::DEFINITION
                .identity
                .replace("{handle}", &bot_handle)
                .replace("{name}", &bot_name),
            bot_instructions
        );
        let instructions = instructions(
            &identity,
            &parent,
            transcript.session_id(),
            &transcript.task_context().await?,
        )?;
        let mut call = router
            .start_realtime_voice(Some(&voice), parent.session_id, offer_sdp, instructions)
            .await?;
        let answer_sdp = std::mem::take(&mut call.answer_sdp);
        Ok((
            Self::attach(router, voice, active_turn_id, bot_name, transcript, call).await?,
            answer_sdp,
        ))
    }

    /// Attaches an already negotiated provider call to the chat's transcript.
    /// # Errors
    ///
    /// Returns an error if the transcript cannot be opened or written.
    pub async fn with_call(context: VoiceCallContext, call: RealtimeVoiceCall) -> Result<Self> {
        let VoiceCallContext {
            session_id,
            router,
            voice,
            bot_name,
            active_turn_id,
            checkpoints,
            frontend,
            ..
        } = context;
        let transcript = VoiceTranscript::open(checkpoints, &session_id, frontend).await?;
        Self::attach(router, voice, active_turn_id, bot_name, transcript, call).await
    }

    async fn attach(
        router: Arc<ModelRouter>,
        voice: Arc<str>,
        active_turn_id: Option<Arc<str>>,
        bot_name: Arc<str>,
        mut transcript: VoiceTranscript,
        call: RealtimeVoiceCall,
    ) -> Result<Self> {
        let finalization_timeout = finalization_timeout(&router, &voice)?;
        transcript.start_call(&call.voice).await?;
        Ok(Self {
            conversation: VoiceConversation::new(
                transcript.session_id().share(),
                active_turn_id,
                bot_name,
            ),
            call,
            transcript,
            handoffs: BTreeMap::new(),
            router,
            voice,
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
        submit: impl AsyncFnOnce(ValidatedSubmission) -> std::result::Result<(), Arc<str>>,
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
                            let submission_id = Arc::clone(&submission.submission().id);
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
                .map_err(|error| Error::Stopped(format!("voice reply timed out: {error}")))?
                .map_err(|error| {
                    Error::Stopped(format!(
                        "voice connection stopped accepting replies: {error}"
                    ))
                })?;
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
        .map_err(|error| Error::Stopped(format!("voice session finalization timed out: {error}")));
        let finalized =
            tokio::time::timeout(TRANSCRIPT_FINALIZATION_TIMEOUT, self.transcript.finish())
                .await
                .map_err(|error| {
                    Error::Stopped(format!("voice transcript finalization timed out: {error}"))
                })?;
        let closed = closed?.map_err(E::from);
        let finalized = finalized.map_err(E::from);
        result.and(closed).and(finalized)
    }
}
