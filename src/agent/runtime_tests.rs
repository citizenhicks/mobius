//! Shared fixtures for agent runtime tests.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use super::Agent;
use super::AgentConfig;
use super::EVENT_QUEUE_CAPACITY;
use super::EventRecorder;
use super::create_agent;
use super::send_event;
use super::submission_channel;
use super::try_send_event;
use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::backend::checkpoint::Checkpoint;
use crate::backend::checkpoint::CheckpointStore;
use crate::backend::checkpoint::EventPageRequest;
use crate::backend::checkpoint::ExecutionOutcome;
use crate::backend::checkpoint::ExecutionPageRequest;
use crate::backend::checkpoint::QueuedMessage;
use crate::backend::checkpoint::QueuedMessageBoundary;
use crate::backend::checkpoint::TranscriptPageRequest;
use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
use crate::backend::model::CompactOutput;
use crate::backend::model::CompactRequest;
use crate::backend::model::Model;
use crate::backend::model::ModelEventSink;
use crate::backend::model::ModelOutput;
use crate::backend::model::ModelRequest;
use crate::backend::model::ModelRouter;
use crate::backend::model::STREAM_RETRY_LIMIT;
use crate::backend::model::TOOL_ERROR_FIELD;
use crate::backend::model::ToolCall;
use crate::backend::model::ToolDefinition;
use crate::backend::sandbox::ApprovalPolicy;
use crate::backend::sandbox::Sandbox;
use crate::backend::sandbox::local::LocalSandbox;
use crate::middleware::CompactContext;
use crate::middleware::MessageSubmitContext;
use crate::middleware::Middleware;
use crate::middleware::MiddlewareStack;
use crate::middleware::ModelContext;
use crate::middleware::ModelRequestContext;
use crate::middleware::PostToolUseContext;
use crate::middleware::PreToolUseContext;
use crate::middleware::RuntimeContext;
use crate::middleware::SessionStartContext;
use crate::middleware::compaction::Compaction;
use crate::middleware::messages::Messages;
use crate::middleware::tools::ApprovalRequirement;
use crate::middleware::tools::Catalog;
use crate::middleware::tools::ToolContext;
use crate::middleware::tools::Tools;
use crate::middleware::tools::{Tool, ToolExposure};
use crate::protocol::ActiveMessageDelivery;
use crate::protocol::ErrorKind;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::FrontendEvent;
use crate::protocol::MAX_MESSAGE_BYTES;
use crate::protocol::MessageAuthor;
use crate::protocol::MessageDelivery;
use crate::protocol::MessageEvent;
use crate::protocol::MessageSubmission;
use crate::protocol::ModelEvent;
use crate::protocol::ModelStepContent;
use crate::protocol::ModelStepContentPhase;
use crate::protocol::ModelStepOutcome;
use crate::protocol::Op;
use crate::protocol::PromptCacheMode;
use crate::protocol::SessionContext;
use crate::protocol::SessionFileReference;
use crate::protocol::TokenUsage;
use crate::protocol::ToolCallEndEvent;
use crate::protocol::WarningEvent;
use crate::protocol::WebSearchAction;

async fn drain_until_notified(agent: &mut Agent, notification: &Notify) {
    loop {
        tokio::select! {
            () = notification.notified() => return,
            event = agent.next_event() => {
                event.expect("agent event while waiting");
            }
        }
    }
}
use crate::protocol::internal_message_kind;
use serde_json::Value;
use tokio::sync::Notify;

fn test_session_context() -> SessionContext {
    SessionContext {
        bot_id: "test-bot".into(),
        ..SessionContext::default()
    }
}

struct TestModel;

struct RetryableModel;

struct RecoveringStreamModel {
    calls: AtomicUsize,
    inputs: Mutex<Vec<Vec<Value>>>,
}

struct InterruptedStreamModel {
    calls: AtomicUsize,
    retry_after: Option<String>,
}

#[derive(Default)]
struct NativeCompactionModel {
    responses: AtomicUsize,
    compactions: AtomicUsize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CompactStop {
    Before,
    After,
    SessionStart,
}

struct StoppingCompaction(CompactStop);

struct StoppingSessionStart;

struct RejectFirstPrompt(AtomicBool);

struct ScriptedModel {
    outputs: Mutex<VecDeque<ModelOutput>>,
    tool_counts: Mutex<Vec<usize>>,
    inputs: Mutex<Vec<Vec<Value>>>,
}

struct RequestOnlyMiddleware;

struct DurableBeforeModel;

struct FailingBeforeModel;

struct ApprovalRequiredTestTool;

struct ToolHookContext;

struct SaturatingMiddleware;

struct BlockingTailMiddleware {
    started: Arc<Notify>,
    release: Arc<Notify>,
    blocked: AtomicBool,
}

struct BlockingModel {
    started: Arc<Notify>,
    release: Arc<Notify>,
    calls: AtomicUsize,
}

fn user_submission(text: impl Into<String>) -> MessageSubmission {
    MessageSubmission {
        author: MessageAuthor::User,
        text: text.into(),
        attachments: Vec::new(),
        reply: None,
        requested_delivery: None,
        target_turn_id: None,
    }
}

fn user_op(text: impl Into<String>) -> Op {
    Op::Message {
        message: user_submission(text),
    }
}

fn user_op_with_attachments(text: impl Into<String>, attachments: Vec<SessionFileReference>) -> Op {
    let mut message = user_submission(text);
    message.attachments = attachments;
    Op::Message { message }
}

fn active_user_op(
    text: impl Into<String>,
    turn_id: impl Into<String>,
    delivery: ActiveMessageDelivery,
) -> Op {
    let mut message = user_submission(text);
    message.requested_delivery = Some(delivery);
    message.target_turn_id = Some(turn_id.into());
    Op::Message { message }
}

fn peer_op(
    message_id: impl Into<String>,
    session_id: impl Into<String>,
    handle: impl Into<String>,
    text: impl Into<String>,
) -> Op {
    Op::Message {
        message: MessageSubmission {
            author: MessageAuthor::Peer {
                message_id: message_id.into(),
                session_id: session_id.into(),
                handle: handle.into(),
            },
            text: text.into(),
            attachments: Vec::new(),
            reply: None,
            requested_delivery: None,
            target_turn_id: None,
        },
    }
}

fn queued_user_message(id: &str, text: &str, boundary: QueuedMessageBoundary) -> QueuedMessage {
    let event = MessageEvent {
        author: MessageAuthor::User,
        delivery: boundary.delivery(),
        text: text.into(),
        attachments: Vec::new(),
        reply: None,
        message_target: None,
    };
    QueuedMessage::new("messages", id, boundary, event).expect("queued message")
}

fn test_middleware(mut entries: Vec<Arc<dyn Middleware>>) -> MiddlewareStack {
    entries.insert(0, Arc::new(Messages::default()));
    MiddlewareStack::new(entries).expect("middleware")
}

impl Middleware for BlockingTailMiddleware {
    fn name(&self) -> &'static str {
        "blocking_tail"
    }

    fn pre_model<'a>(&'a self, _context: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if !self.blocked.swap(true, Ordering::SeqCst) {
                self.started.notify_one();
                self.release.notified().await;
            }
            Ok(())
        })
    }
}

impl Middleware for SaturatingMiddleware {
    fn name(&self) -> &'static str {
        "saturating"
    }

    fn session_start<'a>(
        &'a self,
        context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            for index in 0..=EVENT_QUEUE_CAPACITY {
                (context.runtime.frontend)(FrontendEvent::RemoveWidget {
                    capability: "saturating".into(),
                    id: index.to_string(),
                })?;
            }
            Ok(())
        })
    }
}

struct MetadataProbe {
    observed: Arc<Mutex<Option<std::collections::BTreeMap<String, serde_json::Value>>>>,
}

impl Middleware for MetadataProbe {
    fn name(&self) -> &'static str {
        "metadata_probe"
    }

    fn register(&self, _catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        *self.observed.lock().expect("metadata probe lock") = Some(runtime.metadata.clone());
        Ok(())
    }
}

impl Middleware for RequestOnlyMiddleware {
    fn name(&self) -> &'static str {
        "request_only"
    }

    fn model_request<'a>(
        &'a self,
        context: &'a mut ModelRequestContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut input = context.input().to_vec();
            input.push(crate::backend::model::internal_user_message(
                "request_only",
                "temporary",
            ));
            context.replace_input(input);
            Ok(())
        })
    }
}

impl Middleware for DurableBeforeModel {
    fn name(&self) -> &'static str {
        "durable_before_model"
    }

    fn pre_model<'a>(&'a self, context: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            context.push_input(crate::backend::model::internal_user_message(
                "settled", "durable",
            ))?;
            context.usage.push(scripted_usage());
            context.events.push(EventMsg::ContextCompacted);
            Ok(())
        })
    }
}

impl Middleware for StoppingCompaction {
    fn name(&self) -> &'static str {
        "stopping_compaction"
    }

    fn session_start<'a>(
        &'a self,
        context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self.0 == CompactStop::SessionStart
                && context.source() == crate::middleware::SessionStartSource::Compact
            {
                context.stop("session-start hook stopped the turn")?;
            }
            Ok(())
        })
    }

    fn pre_compact<'a>(&'a self, context: &'a mut CompactContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self.0 == CompactStop::Before {
                context.stop("pre-compact hook stopped the turn")?;
            }
            Ok(())
        })
    }

    fn post_compact<'a>(
        &'a self,
        context: &'a mut CompactContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self.0 == CompactStop::After {
                context.stop("post-compact hook stopped the turn")?;
            }
            Ok(())
        })
    }
}

impl Middleware for StoppingSessionStart {
    fn name(&self) -> &'static str {
        "stopping_session_start"
    }

    fn session_start<'a>(
        &'a self,
        context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { context.stop("session-start hook stopped the turn") })
    }
}

impl Middleware for RejectFirstPrompt {
    fn name(&self) -> &'static str {
        "reject_first_prompt"
    }

    fn message_submit<'a>(
        &'a self,
        context: &'a mut MessageSubmitContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if !self.0.swap(true, Ordering::SeqCst) {
                context.reject("prompt rejected by policy")?;
            }
            Ok(())
        })
    }
}

impl Middleware for FailingBeforeModel {
    fn name(&self) -> &'static str {
        "failing_before_model"
    }

    fn pre_model<'a>(&'a self, _context: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Err(Error::Provider("later middleware failed".into())) })
    }
}

impl Middleware for ToolHookContext {
    fn name(&self) -> &'static str {
        "tool_hook_context"
    }

    fn pre_tool_use<'a>(
        &'a self,
        context: &'a mut PreToolUseContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            context.push_input(crate::backend::model::internal_user_message(
                "pre_tool_hook",
                "before",
            ));
            Ok(())
        })
    }

    fn post_tool_use<'a>(
        &'a self,
        context: &'a mut PostToolUseContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            context.push_input(crate::backend::model::internal_user_message(
                "post_tool_hook",
                "after",
            ));
            Ok(())
        })
    }
}

impl Model for TestModel {
    fn respond<'a>(
        &'a self,
        _request: ModelRequest,
        _events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(async { Err(Error::Provider("response was not expected".into())) })
    }
}

impl Model for RetryableModel {
    fn respond<'a>(
        &'a self,
        _request: ModelRequest,
        _events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(async {
            Err(Error::Provider(crate::ProviderError::http(
                "quota exceeded",
                429,
                Some("5".into()),
            )))
        })
    }
}

impl Model for RecoveringStreamModel {
    fn respond<'a>(
        &'a self,
        request: ModelRequest,
        events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        self.inputs
            .lock()
            .expect("stream input lock")
            .push(request.input.to_vec());
        let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if attempt == 0 {
                events(ModelEvent::WebSearchStarted {
                    call_id: "search-1".into(),
                })?;
                events(ModelEvent::TextDelta("partial".into()))?;
                return Err(Error::Provider(crate::ProviderError::stream_interrupted(
                    None,
                )));
            }
            Ok(scripted_message("Recovered."))
        })
    }
}

impl Model for InterruptedStreamModel {
    fn respond<'a>(
        &'a self,
        _request: ModelRequest,
        _events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let retry_after = self.retry_after.clone();
        Box::pin(async move {
            Err(Error::Provider(crate::ProviderError::stream_interrupted(
                retry_after,
            )))
        })
    }
}

impl Model for NativeCompactionModel {
    fn prompt_cache_capability(&self) -> PromptCacheMode {
        PromptCacheMode::Explicit
    }

    fn respond<'a>(
        &'a self,
        _request: ModelRequest,
        _events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        self.responses.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(scripted_message("done")) })
    }

    fn compaction_endpoint(&self) -> bool {
        true
    }

    fn compact<'a>(&'a self, _request: CompactRequest<'a>) -> BoxFuture<'a, Result<CompactOutput>> {
        self.compactions.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            CompactOutput::from_output(
                vec![serde_json::json!({
                    "type": "compaction",
                    "encrypted_content": "opaque"
                })],
                scripted_usage(),
            )
        })
    }
}

impl Model for ScriptedModel {
    fn respond<'a>(
        &'a self,
        request: ModelRequest,
        _events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        self.tool_counts
            .lock()
            .expect("tool count lock")
            .push(request.tools.len());
        self.inputs
            .lock()
            .expect("input lock")
            .push(request.input.to_vec());
        let output = self
            .outputs
            .lock()
            .expect("scripted output lock")
            .pop_front()
            .ok_or_else(|| Error::Provider("scripted output exhausted".into()));
        Box::pin(async move { output })
    }
}

impl Model for BlockingModel {
    fn respond<'a>(
        &'a self,
        _request: ModelRequest,
        _events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        let should_wait = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            if should_wait {
                self.started.notify_one();
                self.release.notified().await;
            }
            Ok(scripted_message("done"))
        })
    }
}

impl Tool for ApprovalRequiredTestTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "approval_required".into(),
            description: "performs one reviewed mutation".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Always
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        _arguments: serde_json::Value,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async { Ok("executed".into()) })
    }
}

fn scripted_usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 1,
        total_tokens: 1,
        ..TokenUsage::default()
    }
}

fn scripted_tool_call() -> ModelOutput {
    ModelOutput::from_output(
        vec![serde_json::json!({
            "type": "function_call",
            "call_id": "reviewed-call",
            "name": "approval_required",
            "arguments": "{}"
        })],
        false,
        scripted_usage(),
    )
    .expect("tool output")
}

fn scripted_message(text: &str) -> ModelOutput {
    ModelOutput::from_output(
        vec![serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}]
        })],
        true,
        scripted_usage(),
    )
    .expect("message output")
}

fn scripted_continuation(text: &str) -> ModelOutput {
    ModelOutput::from_output(
        vec![serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}]
        })],
        false,
        scripted_usage(),
    )
    .expect("continuation output")
}

fn config(
    workspace: &Path,
    checkpoints: Arc<dyn CheckpointStore>,
    session_id: &str,
) -> AgentConfig {
    config_with_route(workspace, checkpoints, session_id, "test")
}

fn config_with_route(
    workspace: &Path,
    checkpoints: Arc<dyn CheckpointStore>,
    session_id: &str,
    route: &str,
) -> AgentConfig {
    config_with_model(
        workspace,
        checkpoints,
        session_id,
        route,
        Arc::new(TestModel),
    )
}

fn config_with_model(
    workspace: &Path,
    checkpoints: Arc<dyn CheckpointStore>,
    session_id: &str,
    route: &str,
    model: Arc<dyn Model>,
) -> AgentConfig {
    AgentConfig::new(
        Arc::new(ModelRouter::new(route, model)),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace).expect("local sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoints,
        test_middleware(Vec::new()),
        "test prompt",
    )
    .session_context(test_session_context())
    .session_id(session_id)
}

fn config_with_two_routes(
    workspace: &Path,
    checkpoints: Arc<dyn CheckpointStore>,
    session_id: &str,
    default: &str,
    alternate: &str,
) -> AgentConfig {
    let mut models = ModelRouter::new(default, Arc::new(TestModel));
    models
        .register(alternate, Arc::new(TestModel))
        .expect("alternate route");
    AgentConfig::new(
        Arc::new(models),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace).expect("local sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoints,
        test_middleware(Vec::new()),
        "test prompt",
    )
    .session_context(test_session_context())
    .session_id(session_id)
}

fn config_with_metadata_probe(
    workspace: &Path,
    checkpoints: Arc<dyn CheckpointStore>,
    session_id: &str,
    metadata: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    observed: Arc<Mutex<Option<std::collections::BTreeMap<String, serde_json::Value>>>>,
) -> AgentConfig {
    let config = AgentConfig::new(
        Arc::new(ModelRouter::new("test", Arc::new(TestModel))),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace).expect("local sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoints,
        test_middleware(vec![Arc::new(MetadataProbe { observed })]),
        "test prompt",
    )
    .session_context(test_session_context())
    .session_id(session_id);
    match metadata {
        Some(metadata) => config.metadata(metadata),
        None => config,
    }
}

#[path = "runtime_tests/configuration.rs"]
mod configuration;
#[path = "runtime_tests/input_validation.rs"]
mod input_validation;
#[path = "runtime_tests/message_delivery.rs"]
mod message_delivery;
#[path = "runtime_tests/model_steps.rs"]
mod model_steps;
#[path = "runtime_tests/peer_messages.rs"]
mod peer_messages;
#[path = "runtime_tests/recorder.rs"]
mod recorder;
#[path = "runtime_tests/resume_and_recovery.rs"]
mod resume_and_recovery;
#[path = "runtime_tests/tool_discovery.rs"]
mod tool_discovery;
#[path = "runtime_tests/usage_and_approval.rs"]
mod usage_and_approval;
