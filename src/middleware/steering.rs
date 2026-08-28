//! Queued turn steering middleware.

use super::ActiveCommandContext;
use super::ActiveSubmissionContext;
use super::ActiveSubmissionResult;
use super::Middleware;
use super::ModelContext;
use super::SessionStartContext;
use super::SessionStartSource;
use super::TurnEndContext;
use super::manifest::{MiddlewareManifest, MiddlewareSettingManifest};
use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::backend::model::user_message;
use crate::protocol::EventMsg;
use crate::protocol::FrontendActiveInput;
use crate::protocol::FrontendContribution;
use crate::protocol::FrontendEvent;
use crate::protocol::FrontendSlot;
use crate::protocol::FrontendTone;
use crate::protocol::FrontendWidget;
use crate::protocol::MAX_CAPABILITY_INPUT_BYTES;
use crate::protocol::Op;
use crate::protocol::UserMessageEvent;

mod text {
    include!(concat!(env!("OUT_DIR"), "/src_middleware_steering_text.rs"));
}

const MAX_PENDING_MESSAGES: usize = 1_024;
const _: () = {
    assert!(text::DEFAULTS_MAX_PENDING >= 1);
    assert!(text::DEFAULTS_MAX_PENDING <= MAX_PENDING_MESSAGES as i64);
    assert!(text::SETTING_MAX_PENDING_STEP > 0);
};
/// Default number of queued steering messages retained during a turn.
pub const DEFAULT_MAX_PENDING: usize = text::DEFAULTS_MAX_PENDING as usize;
const SETTINGS: &[MiddlewareSettingManifest] = &[MiddlewareSettingManifest::Integer {
    id: "max_pending",
    label: text::SETTING_MAX_PENDING_LABEL,
    description: text::SETTING_MAX_PENDING_DESCRIPTION,
    min: 1,
    max: Some(MAX_PENDING_MESSAGES as i64),
    step: text::SETTING_MAX_PENDING_STEP,
    default: DEFAULT_MAX_PENDING as i64,
}];

/// Configuration and presentation metadata for turn steering.
pub const MANIFEST: MiddlewareManifest = MiddlewareManifest {
    id: "steering",
    label: text::MANIFEST_LABEL,
    description: text::MANIFEST_DESCRIPTION,
    required: true,
    default_enabled: true,
    settings: SETTINGS,
};
const OPERATION: &str = "steer";
const EDIT_COMMAND: &str = "edit";
const STALE_EDIT: &str = "steering message is no longer queued";
const INVALID_EDIT: &str = "steering edit requires non-empty text";
const OPERATIONS: &[&str] = &[OPERATION];

/// Injects queued steering exactly once at the next model boundary.
pub struct Steering {
    max_pending: usize,
}

impl Default for Steering {
    fn default() -> Self {
        Self {
            max_pending: DEFAULT_MAX_PENDING,
        }
    }
}

impl Steering {
    /// Creates steering with a bounded pending-message queue.
    pub fn new(max_pending: usize) -> Result<Self> {
        if max_pending == 0 || max_pending > MAX_PENDING_MESSAGES {
            return Err(Error::Config(format!(
                "steering queue limit must be between 1 and {MAX_PENDING_MESSAGES}"
            )));
        }
        Ok(Self { max_pending })
    }

    fn remove_widget(&self, id: &str) -> FrontendEvent {
        FrontendEvent::RemoveWidget {
            capability: self.name().into(),
            id: id.into(),
        }
    }

    fn queued_widget(&self, id: &str, text: &str) -> FrontendEvent {
        FrontendEvent::Widget {
            capability: self.name().into(),
            item: FrontendWidget {
                id: id.into(),
                slot: FrontendSlot::TranscriptTail,
                text: text.into(),
                tone: FrontendTone::Neutral,
                symbol: None,
                icon_only: false,
                progress: None,
                content: None,
                action: Some(Op::CapabilityCommand {
                    capability: self.name().into(),
                    command: EDIT_COMMAND.into(),
                    arguments: id.into(),
                    input: Some(text.into()),
                    target: None,
                }),
            },
        }
    }
}

impl Middleware for Steering {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn active_operations(&self) -> &'static [&'static str] {
        OPERATIONS
    }

    fn frontend(&self) -> FrontendContribution {
        FrontendContribution {
            capability: self.name().into(),
            active_input: Some(FrontendActiveInput {
                operation: OPERATION.into(),
            }),
            ..FrontendContribution::default()
        }
    }

    fn active_submission(
        &self,
        context: &mut ActiveSubmissionContext<'_>,
    ) -> Result<ActiveSubmissionResult> {
        if context.operation != OPERATION {
            return Err(Error::Config("steering received another operation".into()));
        }
        if context.target_turn_id != context.active_turn_id {
            return Ok(ActiveSubmissionResult::Rejected(
                "steering targeted a stale turn".into(),
            ));
        }
        if context.text.len() > MAX_CAPABILITY_INPUT_BYTES {
            return Ok(ActiveSubmissionResult::Rejected(
                "steering message exceeds editable size limit".into(),
            ));
        }
        if context.queued_input.count() >= self.max_pending {
            return Ok(ActiveSubmissionResult::Rejected(
                "steering queue is full".into(),
            ));
        }
        if !context
            .queued_input
            .enqueue(context.submission_id, context.text)?
        {
            return Ok(ActiveSubmissionResult::Rejected(
                "steering message could not be queued".into(),
            ));
        }
        context.events.push(EventMsg::Frontend(
            self.queued_widget(context.submission_id, context.text),
        ));
        Ok(ActiveSubmissionResult::Accepted)
    }

    fn active_command<'a>(
        &'a self,
        context: &'a mut ActiveCommandContext<'_>,
    ) -> BoxFuture<'a, Result<Option<ActiveSubmissionResult>>> {
        Box::pin(async move {
            if context.command != EDIT_COMMAND {
                return Ok(None);
            }
            let Some(input) = context.input.filter(|input| !input.trim().is_empty()) else {
                return Ok(Some(ActiveSubmissionResult::Rejected(INVALID_EDIT.into())));
            };
            if input.len() > MAX_CAPABILITY_INPUT_BYTES {
                return Ok(Some(ActiveSubmissionResult::Rejected(
                    "steering message exceeds editable size limit".into(),
                )));
            }
            if !context
                .queued_input
                .replace(context.arguments, context.submission_id, input)?
            {
                return Ok(Some(ActiveSubmissionResult::Rejected(STALE_EDIT.into())));
            }
            context
                .events
                .push(EventMsg::Frontend(self.remove_widget(context.arguments)));
            context.events.push(EventMsg::Frontend(
                self.queued_widget(context.submission_id, input),
            ));
            Ok(Some(ActiveSubmissionResult::Accepted))
        })
    }

    fn session_start<'a>(
        &'a self,
        context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if context.source() == SessionStartSource::Compact {
                return Ok(());
            }
            for message in context.queued_input().views() {
                (context.runtime.frontend)(self.queued_widget(message.id(), message.text()))?;
            }
            Ok(())
        })
    }

    fn pre_model<'a>(&'a self, context: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let queued = context.queued_input.drain();
            for message in &queued {
                context
                    .events
                    .push(EventMsg::Frontend(self.remove_widget(message.id())));
            }
            for message in queued {
                let message = message.into_text();
                let item = user_message(&message);
                let message_target = context.push_input(item)?;
                context.events.push(EventMsg::UserMessage(UserMessageEvent {
                    message,
                    attachments: Vec::new(),
                    message_target: Some(message_target),
                }));
            }
            Ok(())
        })
    }

    fn turn_end<'a>(&'a self, context: &'a mut TurnEndContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let removals = context
                .queued_input()
                .map(|message| EventMsg::Frontend(self.remove_widget(message.id())))
                .collect::<Vec<_>>();
            context.events.extend(removals);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;
    use std::sync::Mutex;

    use super::*;
    use crate::backend::checkpoint::CheckpointStore;
    use crate::backend::checkpoint::ExecutionOutcome;
    use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
    use crate::backend::model::{Model, ModelEventSink, ModelOutput, ModelRequest, ModelRouter};
    use crate::middleware::DurableQueuedInput;
    use crate::middleware::MiddlewareStack;
    use crate::middleware::QueuedInputBaseline;
    use crate::middleware::QueuedInputQueue;
    use crate::middleware::RuntimeContext;
    use crate::middleware::SessionStartSource;
    use crate::middleware::tools::Catalog;
    use crate::protocol::SessionContext;

    fn item(id: &str, text: &str) -> DurableQueuedInput {
        DurableQueuedInput::new(MANIFEST.id, id, text).expect("valid queued input")
    }

    fn queue(items: &mut Vec<DurableQueuedInput>) -> QueuedInputQueue<'_> {
        let mut queue = QueuedInputQueue::new(items, QueuedInputBaseline::default());
        queue.scope(MANIFEST.id);
        queue
    }

    struct UnusedModel;

    impl Model for UnusedModel {
        fn respond<'a>(
            &'a self,
            _request: ModelRequest<'a>,
            _events: ModelEventSink,
        ) -> BoxFuture<'a, Result<ModelOutput>> {
            Box::pin(async { Err(Error::Provider("unused test model".into())) })
        }
    }

    #[test]
    fn steering_rejects_queue_sizes_outside_its_manifest_bounds() {
        assert!(Steering::new(0).is_err());
        assert!(Steering::new(MAX_PENDING_MESSAGES + 1).is_err());
    }

    #[test]
    fn edit_action_is_not_a_catalog_command() {
        assert!(Steering::default().frontend().commands.is_empty());
    }

    #[tokio::test]
    async fn session_start_restores_every_owned_queue_widget() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
            SqliteCheckpoint::new(temporary.path().join("checkpoints.sqlite3"))
                .expect("checkpoint store"),
        );
        let frontend_events = Arc::new(Mutex::new(Vec::new()));
        let sink_events = Arc::clone(&frontend_events);
        let runtime = RuntimeContext {
            checkpoints,
            session_id: "session".into(),
            model_route: "model".into(),
            model: "model".into(),
            approval_policy: crate::backend::sandbox::ApprovalPolicy::Ask,
            session_context: SessionContext::default(),
            metadata: BTreeMap::new(),
            role: crate::agent::AgentRole::Main,
            frontend: Arc::new(move |event| {
                sink_events.lock().expect("frontend events").push(event);
                Ok(())
            }),
        };
        let pending = vec![
            DurableQueuedInput::new("other", "private", "hidden").expect("other item"),
            item("steering-1", "older"),
            item("steering-2", "latest"),
        ];
        let stack = MiddlewareStack::new(vec![Arc::new(Steering::default())]).expect("stack");
        let mut input = Vec::new();

        stack
            .session_start(&runtime, &pending, SessionStartSource::Startup, &mut input)
            .await
            .expect("start middleware");

        let events = frontend_events.lock().expect("frontend events");
        let [
            FrontendEvent::Widget { item: older, .. },
            FrontendEvent::Widget { item: latest, .. },
        ] = events.as_slice()
        else {
            panic!("expected both restored widgets");
        };
        assert_eq!(
            (older.id.as_str(), older.text.as_str()),
            ("steering-1", "older")
        );
        assert_eq!(
            (latest.id.as_str(), latest.text.as_str()),
            ("steering-2", "latest")
        );
        assert!(matches!(
            &latest.action,
            Some(Op::CapabilityCommand { arguments, .. }) if arguments == "steering-2"
        ));
    }

    #[test]
    fn active_submission_rejects_text_above_the_editable_limit() {
        let steering = Steering::default();
        let mut queued = Vec::new();
        let mut events = Vec::new();
        let text = "x".repeat(MAX_CAPABILITY_INPUT_BYTES + 1);

        let result = steering
            .active_submission(&mut ActiveSubmissionContext {
                submission_id: "steering-1",
                operation: OPERATION,
                active_turn_id: "turn-1",
                target_turn_id: "turn-1",
                text: &text,
                queued_input: queue(&mut queued),
                events: &mut events,
            })
            .expect("active submission");

        assert_eq!(
            result,
            ActiveSubmissionResult::Rejected("steering message exceeds editable size limit".into())
        );
        assert!(queued.is_empty());
        assert!(events.is_empty());
    }

    #[test]
    fn active_submission_queues_its_id_and_exact_edit_widget() {
        let steering = Steering::default();
        let mut queued = Vec::new();
        let mut events = Vec::new();
        let text = "keep this exact\nincluding the second line";

        let result = steering
            .active_submission(&mut ActiveSubmissionContext {
                submission_id: "steering-1",
                operation: OPERATION,
                active_turn_id: "turn-1",
                target_turn_id: "turn-1",
                text,
                queued_input: queue(&mut queued),
                events: &mut events,
            })
            .expect("active submission");

        assert_eq!(result, ActiveSubmissionResult::Accepted);
        assert_eq!(queued, vec![item("steering-1", text)]);
        let [EventMsg::Frontend(FrontendEvent::Widget { capability, item })] = events.as_slice()
        else {
            panic!("expected queued widget");
        };
        assert_eq!(capability, MANIFEST.id);
        assert_eq!(item.id, "steering-1");
        assert_eq!(item.slot, FrontendSlot::TranscriptTail);
        assert_eq!(item.text, text);
        assert_eq!(
            item.action,
            Some(Op::CapabilityCommand {
                capability: MANIFEST.id.into(),
                command: EDIT_COMMAND.into(),
                arguments: "steering-1".into(),
                input: Some(text.into()),
                target: None,
            })
        );
    }

    #[test]
    fn active_submissions_publish_distinct_widgets() {
        let steering = Steering::default();
        let mut queued = Vec::new();
        let mut events = Vec::new();

        for (submission_id, text) in [("steering-1", "older"), ("steering-2", "latest")] {
            let result = steering
                .active_submission(&mut ActiveSubmissionContext {
                    submission_id,
                    operation: OPERATION,
                    active_turn_id: "turn-1",
                    target_turn_id: "turn-1",
                    text,
                    queued_input: queue(&mut queued),
                    events: &mut events,
                })
                .expect("active submission");
            assert_eq!(result, ActiveSubmissionResult::Accepted);
        }

        assert_eq!(
            queued,
            vec![item("steering-1", "older"), item("steering-2", "latest")]
        );
        assert!(matches!(
            events.as_slice(),
            [
                EventMsg::Frontend(FrontendEvent::Widget { item: older, .. }),
                EventMsg::Frontend(FrontendEvent::Widget { item: latest, .. })
            ] if older.id == "steering-1" && latest.id == "steering-2"
        ));
    }

    #[test]
    fn replayed_active_submission_is_rejected_without_ending_the_session() {
        let steering = Steering::default();
        let mut queued = vec![item("steering-1", "original")];
        let original = queued.clone();
        let mut events = Vec::new();

        let result = steering
            .active_submission(&mut ActiveSubmissionContext {
                submission_id: "steering-1",
                operation: OPERATION,
                active_turn_id: "turn-1",
                target_turn_id: "turn-1",
                text: "replayed",
                queued_input: queue(&mut queued),
                events: &mut events,
            })
            .expect("active submission");

        assert_eq!(
            result,
            ActiveSubmissionResult::Rejected("steering message could not be queued".into())
        );
        assert_eq!(queued, original);
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn active_command_replaces_the_selected_queued_message() {
        let steering = Steering::default();
        let mut queued = vec![item("steering-1", "older"), item("steering-2", "latest")];
        let mut events = Vec::new();

        let result = steering
            .active_command(&mut ActiveCommandContext {
                submission_id: "edit-1",
                session_id: "session-1",
                metadata: &std::collections::BTreeMap::new(),
                active_turn_id: "turn-1",
                command: EDIT_COMMAND,
                arguments: "steering-1",
                input: Some("edited older"),
                target: None,
                queued_input: queue(&mut queued),
                events: &mut events,
            })
            .await
            .expect("active command");

        assert_eq!(result, Some(ActiveSubmissionResult::Accepted));
        assert_eq!(
            queued,
            vec![item("edit-1", "edited older"), item("steering-2", "latest")]
        );
        let [
            EventMsg::Frontend(FrontendEvent::RemoveWidget { capability, id }),
            EventMsg::Frontend(FrontendEvent::Widget {
                item: edited_widget,
                ..
            }),
        ] = events.as_slice()
        else {
            panic!("expected the edited widget to be replaced");
        };
        assert_eq!(
            (capability.as_str(), id.as_str()),
            (MANIFEST.id, "steering-1")
        );
        assert_eq!(
            (edited_widget.id.as_str(), edited_widget.text.as_str()),
            ("edit-1", "edited older")
        );
    }

    #[tokio::test]
    async fn edited_message_reaches_the_next_model_request() {
        let steering = Steering::default();
        let metadata = BTreeMap::new();
        let mut queued = vec![item("steering-1", "original")];
        let mut edit_events = Vec::new();

        let result = steering
            .active_command(&mut ActiveCommandContext {
                submission_id: "edit-1",
                session_id: "session-1",
                metadata: &metadata,
                active_turn_id: "turn-1",
                command: EDIT_COMMAND,
                arguments: "steering-1",
                input: Some("edited text"),
                target: None,
                queued_input: queue(&mut queued),
                events: &mut edit_events,
            })
            .await
            .expect("edit queued message");
        assert_eq!(result, Some(ActiveSubmissionResult::Accepted));

        let model = ModelRouter::new("unused", Arc::new(UnusedModel));
        let session_context = SessionContext::default();
        let tools = Catalog::default();
        let mut request_input = Vec::new();
        let mut available_tools = BTreeSet::new();
        let mut durable_input = Vec::new();
        let mut transcript_delta = Vec::new();
        let mut context_epoch = 0;
        let mut compaction_count = 0;
        let mut rewrite_reasons = Vec::new();
        let mut turn_stop = None;
        let mut events = Vec::new();
        let mut usage = Vec::new();
        let mut checkpoint_changed = false;
        let temporary = tempfile::tempdir().expect("temporary directory");
        let runtime = RuntimeContext {
            checkpoints: Arc::new(
                SqliteCheckpoint::new(temporary.path().join("runtime.sqlite3"))
                    .expect("checkpoint store"),
            ),
            session_id: "session-1".into(),
            model_route: "unused".into(),
            model: "model".into(),
            approval_policy: crate::backend::sandbox::ApprovalPolicy::Ask,
            session_context: session_context.clone(),
            metadata: metadata.clone(),
            role: crate::agent::AgentRole::Main,
            frontend: Arc::new(|_| Ok(())),
        };
        let hooks = MiddlewareStack::new(Vec::new()).expect("empty hook stack");

        steering
            .pre_model(&mut ModelContext {
                model: &model,
                provider: "unused",
                session_id: "session-1",
                session_context: &session_context,
                metadata: &metadata,
                turn_id: "turn-1",
                model_step: 1,
                context_window: 1_000,
                instructions: "",
                checkpoint_sequence: 0,
                request_input: &mut request_input,
                available_tools: &mut available_tools,
                durable_input: &mut durable_input,
                transcript_delta: &mut transcript_delta,
                context_epoch: &mut context_epoch,
                compaction_count: &mut compaction_count,
                rewrite_reasons: &mut rewrite_reasons,
                turn_stop: &mut turn_stop,
                queued_input: queue(&mut queued),
                last_usage: None,
                tools: &tools,
                events: &mut events,
                usage: &mut usage,
                checkpoint_changed: &mut checkpoint_changed,
                runtime: &runtime,
                hooks: &hooks,
            })
            .await
            .expect("inject queued steering");

        let expected = user_message("edited text");
        assert!(queued.is_empty());
        assert_eq!(request_input.as_slice(), std::slice::from_ref(&expected));
        assert_eq!(durable_input.as_slice(), std::slice::from_ref(&expected));
        assert_eq!(transcript_delta, [expected]);
        assert!(matches!(
            events.as_slice(),
            [
                EventMsg::Frontend(FrontendEvent::RemoveWidget { id, .. }),
                EventMsg::UserMessage(UserMessageEvent { message, .. })
            ] if id == "edit-1" && message == "edited text"
        ));
    }

    #[tokio::test]
    async fn turn_end_removes_each_pending_widget_by_id() {
        let stack = MiddlewareStack::new(vec![Arc::new(Steering::default())]).expect("stack");
        let queued = vec![
            DurableQueuedInput::new("other", "private", "hidden").expect("other item"),
            item("steering-1", "older"),
            item("steering-2", "latest"),
        ];
        let mut events = Vec::new();

        stack
            .turn_end(TurnEndContext {
                session_id: "session-1",
                turn_id: "turn-1",
                outcome: ExecutionOutcome::Completed,
                queued_input: &queued,
                owner: None,
                events: &mut events,
            })
            .await
            .expect("turn ended");

        assert_eq!(
            events,
            vec![
                EventMsg::Frontend(FrontendEvent::RemoveWidget {
                    capability: MANIFEST.id.into(),
                    id: "steering-1".into(),
                }),
                EventMsg::Frontend(FrontendEvent::RemoveWidget {
                    capability: MANIFEST.id.into(),
                    id: "steering-2".into(),
                })
            ]
        );
    }

    #[tokio::test]
    async fn active_command_rejects_a_stale_id_without_mutation() {
        let steering = Steering::default();
        let mut queued = vec![item("steering-2", "latest")];
        let original = queued.clone();
        let mut events = Vec::new();

        let result = steering
            .active_command(&mut ActiveCommandContext {
                submission_id: "edit-1",
                session_id: "session-1",
                metadata: &std::collections::BTreeMap::new(),
                active_turn_id: "turn-1",
                command: EDIT_COMMAND,
                arguments: "steering-1",
                input: Some("latest"),
                target: None,
                queued_input: queue(&mut queued),
                events: &mut events,
            })
            .await
            .expect("active command");

        assert_eq!(
            result,
            Some(ActiveSubmissionResult::Rejected(STALE_EDIT.into()))
        );
        assert_eq!(queued, original);
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn second_edit_from_the_same_widget_loses_the_revision_race() {
        let steering = Steering::default();
        let mut queued = vec![item("steering-1", "original")];
        let mut first_events = Vec::new();
        let first = steering
            .active_command(&mut ActiveCommandContext {
                submission_id: "edit-1",
                session_id: "session-1",
                metadata: &std::collections::BTreeMap::new(),
                active_turn_id: "turn-1",
                command: EDIT_COMMAND,
                arguments: "steering-1",
                input: Some("original"),
                target: None,
                queued_input: queue(&mut queued),
                events: &mut first_events,
            })
            .await
            .expect("first edit");
        let mut stale_events = Vec::new();
        let stale = steering
            .active_command(&mut ActiveCommandContext {
                submission_id: "edit-2",
                session_id: "session-1",
                metadata: &std::collections::BTreeMap::new(),
                active_turn_id: "turn-1",
                command: EDIT_COMMAND,
                arguments: "steering-1",
                input: Some("original"),
                target: None,
                queued_input: queue(&mut queued),
                events: &mut stale_events,
            })
            .await
            .expect("stale edit");

        assert_eq!(first, Some(ActiveSubmissionResult::Accepted));
        assert_eq!(
            stale,
            Some(ActiveSubmissionResult::Rejected(STALE_EDIT.into()))
        );
        assert_eq!(queued, vec![item("edit-1", "original")]);
        assert!(matches!(
            first_events.as_slice(),
            [
                EventMsg::Frontend(FrontendEvent::RemoveWidget { .. }),
                EventMsg::Frontend(FrontendEvent::Widget { .. })
            ]
        ));
        assert!(stale_events.is_empty());
    }

    #[tokio::test]
    async fn active_command_rejects_an_already_consumed_message() {
        let steering = Steering::default();
        let mut queued = Vec::new();
        let mut events = Vec::new();

        let result = steering
            .active_command(&mut ActiveCommandContext {
                submission_id: "edit-1",
                session_id: "session-1",
                metadata: &std::collections::BTreeMap::new(),
                active_turn_id: "turn-1",
                command: EDIT_COMMAND,
                arguments: "steering-1",
                input: Some("too late"),
                target: None,
                queued_input: queue(&mut queued),
                events: &mut events,
            })
            .await
            .expect("active command");

        assert_eq!(
            result,
            Some(ActiveSubmissionResult::Rejected(STALE_EDIT.into()))
        );
        assert!(events.is_empty());
    }
}
