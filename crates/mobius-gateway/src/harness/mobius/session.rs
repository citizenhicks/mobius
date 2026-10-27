//! One möbius `Agent` running a chat for the session actor.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use ::mobius::agent::AgentSender;
use ::mobius::backend::checkpoint::JournalEvent;
use ::mobius::backend::model::ModelRouter;
use ::mobius::backend::sandbox::Sandbox;
use ::mobius::middleware::{FrontendEventSink, FrontendExtensions};
use ::mobius::protocol::{
    ActiveMessageDelivery, EventMsg, ExecApprovalRequestEvent, FrontendContribution, RenderedBlock,
    SessionConfiguredEvent,
};
use ::mobius::{BoxFuture, SessionId};
use tokio::sync::mpsc;

use super::ValidatedSubmission;
use super::assembly::{BuiltAgent, PreparedBot, assemble, prepare_bot};
use crate::Result;
use crate::config::ChatSpec;
use crate::harness::{
    Admission, Harness, HarnessContext, HarnessSession, RealtimeCall, adopt_checkpoint_owner,
};
use crate::host::{Rejection, RenderEvents, SessionRuntimeState};
use crate::sandbox::GatewaySandbox;
use crate::wire::BotRecord;

/// The möbius framework harness.
pub(crate) struct Mobius;

impl Harness for Mobius {
    fn start<'a>(
        &'a self,
        context: &'a Arc<HarnessContext>,
        spec: &'a ChatSpec,
        session_id: &'a SessionId,
        origin_label: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn HarnessSession>>> {
        Box::pin(async move {
            let session: Box<dyn HarnessSession> =
                Box::new(start(context, spec, session_id, origin_label, None).await?);
            Ok(session)
        })
    }
}

/// A voice call's chat route and framework context.
pub(crate) struct RealtimeModel {
    /// The chat route used to prepare this call's instructions.
    pub(crate) route: String,
    pub(crate) call: ::mobius::middleware::voice::VoiceCallContext,
}

struct MobiusSession {
    context: Arc<HarnessContext>,
    session_id: SessionId,
    sender: Option<AgentSender>,
    events: mpsc::Receiver<JournalEvent>,
    model_router: Arc<ModelRouter>,
    sandbox: Arc<Sandbox>,
    frontend: FrontendExtensions,
    frontend_sink: FrontendEventSink,
    session: Arc<SessionConfiguredEvent>,
    gateway_sandbox: Arc<GatewaySandbox>,
    prepared: Arc<PreparedBot>,
}

impl RenderEvents for MobiusSession {
    fn render(&self, event: &EventMsg) -> Vec<RenderedBlock> {
        self.frontend.render(event)
    }

    fn preview_approvals(&self, request: &mut ExecApprovalRequestEvent) {
        self.frontend.preview_approvals(request);
    }
}

impl HarnessSession for MobiusSession {
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    fn session(&self) -> &Arc<SessionConfiguredEvent> {
        &self.session
    }

    fn bot(&self) -> &Arc<BotRecord> {
        &self.prepared.bot
    }

    fn is_current(&self, bot: &BotRecord) -> bool {
        self.prepared.matches_runtime(bot)
            && self.prepared.epoch == self.context.provider_epoch.load(Ordering::Acquire)
    }

    fn sandbox(&self) -> &Arc<GatewaySandbox> {
        &self.gateway_sandbox
    }

    fn submit(&self, submission: ValidatedSubmission) -> Option<::mobius::Result<()>> {
        Some(self.sender.as_ref()?.send_validated(submission))
    }

    fn admit(&self, submission: ValidatedSubmission) -> Option<::mobius::Result<Admission>> {
        Some(
            self.sender
                .as_ref()?
                .send_validated_with_admission(submission)
                .map(|admission| -> Admission { Box::pin(admission.wait()) }),
        )
    }

    fn events(&mut self) -> &mut mpsc::Receiver<JournalEvent> {
        &mut self.events
    }

    fn runtime_state(&self) -> BoxFuture<'_, Result<Option<SessionRuntimeState>>> {
        Box::pin(SessionRuntimeState::load(
            self.context.checkpoints.as_ref(),
            &self.session_id,
        ))
    }

    fn is_idle(&self) -> BoxFuture<'_, Result<bool>> {
        Box::pin(async move {
            if self.sandbox.has_background_commands(&self.session_id)? {
                return Ok(false);
            }
            Ok(!self.frontend.has_background_work().await?)
        })
    }

    fn contributions(&self) -> Result<Vec<FrontendContribution>> {
        Ok(self.frontend.contributions()?)
    }

    fn context_limit(&self, window: i64) -> i64 {
        self.frontend.context_limit(window)
    }

    fn active_message_delivery(&self) -> ActiveMessageDelivery {
        self.prepared.active_message_delivery
    }

    fn realtime(
        &self,
        call: RealtimeCall,
    ) -> Option<std::result::Result<RealtimeModel, Rejection>> {
        let RealtimeCall {
            bot,
            active_turn_id,
        } = call;
        let router = &self.model_router;
        let model = (|| {
            let selected = self.prepared.voice.as_ref().ok_or_else(|| {
                Rejection::new("realtime_voice", "voice is turned off for this Bot")
            })?;
            let voice = router.voice_choice(selected.as_deref()).map_err(|error| {
                Rejection::new(
                    "realtime_voice",
                    format!("{error}; select an available voice"),
                )
            })?;
            Ok(RealtimeModel {
                route: router.default_provider().into(),
                call: ::mobius::middleware::voice::VoiceCallContext {
                    session_id: self.session_id.share(),
                    router: Arc::clone(router),
                    voice: Arc::clone(&voice.route),
                    bot_name: bot.name,
                    bot_handle: bot.handle,
                    bot_instructions: self.prepared.instructions(),
                    active_turn_id,
                    checkpoints: Arc::clone(&self.context.checkpoints),
                    frontend: Arc::clone(&self.frontend_sink),
                },
            })
        })();
        Some(model)
    }

    fn restart<'a>(
        &'a self,
        spec: &'a ChatSpec,
        origin_label: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn HarnessSession>>> {
        Box::pin(async move {
            let session: Box<dyn HarnessSession> = Box::new(
                start(
                    &self.context,
                    spec,
                    &self.session_id,
                    origin_label,
                    Some(Arc::clone(&self.prepared)),
                )
                .await?,
            );
            Ok(session)
        })
    }

    fn shutdown(&mut self) {
        drop(self.sender.take());
    }
}

async fn start(
    context: &Arc<HarnessContext>,
    spec: &ChatSpec,
    session_id: &SessionId,
    origin_label: &str,
    prepared: Option<Arc<PreparedBot>>,
) -> Result<MobiusSession> {
    let prepared = match prepared {
        Some(prepared) => prepared,
        None => prepared_bot(context, spec).await?,
    };
    adopt_checkpoint_owner(context.checkpoints.as_ref(), session_id, spec).await?;
    let BuiltAgent {
        agent,
        model_router,
        sandbox,
        gateway_sandbox,
    } = assemble(
        Arc::clone(&context.gateway),
        spec,
        &context.store,
        Arc::clone(&context.checkpoints),
        Arc::clone(&context.memory),
        Arc::clone(&context.session_files),
        Arc::clone(&context.discovery_gate),
        Arc::clone(&context.desktop),
        Arc::clone(&context.remote_desktop),
        Some(session_id.share()),
        origin_label,
        Arc::clone(&prepared),
        Some(Arc::clone(&context.live_chats)),
        Some(Arc::clone(&context.host_access)),
    )
    .await?;
    let frontend_sink = agent.frontend_sink();
    let (sender, events, session, frontend) = agent.into_recorded_parts();
    let session_id = session.session_id.share();
    Ok(MobiusSession {
        context: Arc::clone(context),
        session_id,
        sender: Some(sender),
        events,
        model_router,
        sandbox,
        frontend,
        frontend_sink,
        session,
        gateway_sandbox,
        prepared,
    })
}

/// The Bot's cached preparation, or a fresh one that no refresh overtook.
async fn prepared_bot(context: &HarnessContext, spec: &ChatSpec) -> Result<Arc<PreparedBot>> {
    let bots = &context.bots;
    loop {
        let generation = bots.preparation_generation.load(Ordering::Acquire);
        let runtime_bot = bots.runtime_bot(&spec.bot_id)?;
        let bot = Arc::clone(&runtime_bot.record);
        let epoch = context.provider_epoch.load(Ordering::Acquire);
        let cached = bots
            .prepared
            .lock()
            .await
            .get(&*bot.id)
            .filter(|prepared| prepared.matches_runtime(&bot) && prepared.epoch == epoch)
            .map(Arc::clone);
        if let Some(prepared) = cached {
            return Ok(prepared);
        }
        let preparation = {
            let config = crate::config::lock(&context.gateway)?;
            prepare_bot(
                &config,
                runtime_bot,
                &context.store,
                &context.credentials,
                Arc::clone(&context.session_files),
                epoch,
                Arc::clone(context.remote_desktop.configuration()),
            )
        };
        let prepared = preparation.await;
        let mut cache = bots.prepared.lock().await;
        let current_bot = bots.bot(&spec.bot_id)?;
        // A refresh can finish while preparation waits for external resources.
        if generation != bots.preparation_generation.load(Ordering::Acquire)
            || epoch != context.provider_epoch.load(Ordering::Acquire)
            || !crate::harness::same_runtime(
                &bot,
                &current_bot.description,
                &current_bot.config.config,
            )
        {
            continue;
        }
        let cached = cache
            .get(&*bot.id)
            .filter(|cached| cached.matches_runtime(&bot) && cached.epoch == epoch)
            .map(Arc::clone);
        let prepared = if let Some(cached) = cached {
            cached
        } else {
            let prepared = Arc::new(prepared?);
            if let Some(previous) = cache.insert(Arc::clone(&bot.id), Arc::clone(&prepared)) {
                previous.invalidate();
            }
            prepared
        };
        drop(cache);
        return Ok(prepared);
    }
}
