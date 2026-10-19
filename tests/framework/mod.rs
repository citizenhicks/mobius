use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use mobius::BoxFuture;
use mobius::Error;
use mobius::Result;
use mobius::agent::AgentConfig;
use mobius::agent::create_agent;
use mobius::backend::checkpoint::Checkpoint;
use mobius::backend::checkpoint::CheckpointStore;
use mobius::backend::checkpoint::EventPage;
use mobius::backend::checkpoint::EventPageRequest;
use mobius::backend::checkpoint::ExecutionRecord;
use mobius::backend::checkpoint::JournalEvent;
use mobius::backend::checkpoint::SessionPageRequest;
use mobius::backend::checkpoint::TimestampedEvent;
use mobius::backend::checkpoint::TranscriptPageRequest;
use mobius::backend::checkpoint::sqlite::SqliteCheckpoint;
use mobius::backend::model::Model;
use mobius::backend::model::ModelEventSink;
use mobius::backend::model::ModelOutput;
use mobius::backend::model::ModelRequest;
use mobius::backend::model::ModelRouter;
use mobius::backend::model::ToolDefinition;
use mobius::backend::sandbox::ApprovalPolicy;
use mobius::backend::sandbox::CommandOutputSink;
use mobius::backend::sandbox::NetworkAccess;
use mobius::backend::sandbox::Sandbox;
use mobius::backend::sandbox::SandboxBackend;
use mobius::backend::sandbox::SandboxMode;
use mobius::backend::sandbox::local::LocalSandbox;
use mobius::backend::session_files::{SessionFileStore, session_file_limits};
use mobius::middleware::Middleware;
use mobius::middleware::MiddlewareStack;
use mobius::middleware::attachments::Attachments;
use mobius::middleware::compaction::Compaction;
use mobius::middleware::messages::Messages;
use mobius::middleware::tools::Tools;
use mobius::protocol::AssistantMessageEvent;
use mobius::protocol::Event;
use mobius::protocol::EventMsg;
use mobius::protocol::MessageAuthor;
use mobius::protocol::MessageSubmission;
use mobius::protocol::ModelEvent;
use mobius::protocol::ModelStepContentPhase;
use mobius::protocol::Op;
use mobius::protocol::ReviewDecision;
use mobius::protocol::SessionContext;
use mobius::protocol::SessionFileReference;
use mobius::protocol::TokenUsage;
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::Notify;

mod agent_loop;
mod attachments;
mod compaction;
mod storage;

struct ScriptedModel {
    responses: Mutex<VecDeque<ModelOutput>>,
    requests: Mutex<Vec<RecordedRequest>>,
    image_input: bool,
}

#[derive(Clone)]
struct RecordedRequest {
    allow_hosted_tools: bool,
    input: Vec<Value>,
    tools: Vec<Arc<ToolDefinition>>,
}

fn test_session_context() -> SessionContext {
    SessionContext {
        owner_id: "test-bot".into(),
        ..SessionContext::default()
    }
}

impl ScriptedModel {
    fn new(responses: Vec<ModelOutput>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
            image_input: false,
        }
    }

    fn with_image_input(mut self) -> Self {
        self.image_input = true;
        self
    }
}

impl Model for ScriptedModel {
    fn supports_tool_image_input(&self) -> bool {
        self.image_input
    }
    fn supports_image_input(&self) -> bool {
        self.image_input
    }

    fn respond<'a>(
        &'a self,
        request: ModelRequest<'a>,
        events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(async move {
            self.requests
                .lock()
                .expect("requests")
                .push(RecordedRequest {
                    allow_hosted_tools: request.allow_hosted_tools,
                    input: request.input.iter().cloned().collect(),
                    tools: request.tools.to_vec(),
                });
            let output = self
                .responses
                .lock()
                .expect("responses")
                .pop_front()
                .ok_or_else(|| Error::Provider("script exhausted".into()))?;
            if !output.text().is_empty() {
                events(ModelEvent::TextDelta(output.text().into())).await?;
            }
            Ok(output)
        })
    }
}

struct GatedModel {
    inner: Arc<ScriptedModel>,
    first: AtomicBool,
    entered: Notify,
    release: Notify,
}

impl Model for GatedModel {
    fn supports_image_input(&self) -> bool {
        self.inner.supports_image_input()
    }

    fn respond<'a>(
        &'a self,
        request: ModelRequest<'a>,
        events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(async move {
            if self.first.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.inner.respond(request, events).await
        })
    }
}

#[derive(Default)]
struct MemoryCheckpoints {
    sessions: Mutex<BTreeMap<Arc<str>, Checkpoint>>,
    receipts: Mutex<std::collections::BTreeSet<(Arc<str>, String)>>,
    state: Mutex<BTreeMap<(String, String), Value>>,
}

impl CheckpointStore for MemoryCheckpoints {
    fn message_accepted<'a>(
        &'a self,
        session_id: &'a str,
        submission_id: &'a str,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let _sessions = self.sessions.lock().expect("checkpoint store");
            Ok(self
                .receipts
                .lock()
                .expect("receipts")
                .contains(&(session_id.into(), submission_id.into())))
        })
    }

    fn load<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<Option<Checkpoint>>> {
        Box::pin(async move {
            Ok(self
                .sessions
                .lock()
                .expect("checkpoint store")
                .get(session_id)
                .cloned())
        })
    }

    fn delete_sessions<'a>(&'a self, session_ids: &'a [String]) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let mut sessions = self.sessions.lock().expect("checkpoint store");
            if session_ids
                .iter()
                .any(|session_id| !sessions.contains_key(session_id.as_str()))
            {
                return Ok(false);
            }
            for session_id in session_ids {
                sessions.remove(session_id.as_str());
            }
            drop(sessions);
            self.receipts
                .lock()
                .expect("receipts")
                .retain(|(session_id, _)| {
                    !session_ids.iter().any(|deleted| **deleted == **session_id)
                });
            Ok(true)
        })
    }

    fn save_with_events(
        &self,
        checkpoint: Arc<Checkpoint>,
        _transcript_delta: Vec<Arc<Value>>,
        _execution: Option<ExecutionRecord>,
        events: Vec<TimestampedEvent>,
    ) -> BoxFuture<'_, Result<Vec<JournalEvent>>> {
        Box::pin(async move {
            self.receipts.lock().expect("receipts").extend(
                checkpoint
                    .pending_messages
                    .iter()
                    .map(|message| (Arc::clone(&checkpoint.session_id), message.id().into())),
            );
            self.sessions.lock().expect("checkpoint store").insert(
                Arc::clone(&checkpoint.session_id),
                Checkpoint::clone(&checkpoint),
            );
            let mut records = Vec::with_capacity(events.len());
            for event in events {
                records.push(
                    self.append_event(&checkpoint.session_id, event.recorded_at_ms, event.event)
                        .await?,
                );
            }
            Ok(records)
        })
    }

    fn append_event<'a>(
        &'a self,
        _session_id: &'a str,
        recorded_at_ms: i64,
        event: Event,
    ) -> BoxFuture<'a, Result<JournalEvent>> {
        Box::pin(async move {
            Ok(JournalEvent {
                sequence: 1,
                recorded_at_ms,
                event,
                stream_metrics: Vec::new(),
            })
        })
    }

    fn event_page<'a>(
        &'a self,
        _session_id: &'a str,
        _request: EventPageRequest,
    ) -> BoxFuture<'a, Result<EventPage>> {
        Box::pin(async { Ok(EventPage::default()) })
    }

    fn load_state<'a>(
        &'a self,
        scope: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(async move {
            Ok(self
                .state
                .lock()
                .expect("checkpoint state")
                .get(&(scope.to_owned(), key.to_owned()))
                .cloned())
        })
    }

    fn save_state<'a>(
        &'a self,
        scope: &'a str,
        key: &'a str,
        value: &'a Value,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.state
                .lock()
                .expect("checkpoint state")
                .insert((scope.to_owned(), key.to_owned()), value.clone());
            Ok(())
        })
    }
}

fn test_config<M>(
    workspace: &std::path::Path,
    model: Arc<M>,
    middleware: Vec<Arc<dyn Middleware>>,
) -> AgentConfig
where
    M: Model + 'static,
{
    let model: Arc<dyn Model> = model;
    test_config_with_router(workspace, ModelRouter::new("test", model), middleware)
}

fn test_config_with_router(
    workspace: &std::path::Path,
    model: ModelRouter,
    mut middleware: Vec<Arc<dyn Middleware>>,
) -> AgentConfig {
    middleware.insert(0, Arc::new(Messages::default()));
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(MemoryCheckpoints::default());
    let sandbox = Arc::new(Sandbox::new(
        Arc::new(LocalSandbox::new(workspace).expect("local sandbox")),
        ApprovalPolicy::Ask,
    ));
    AgentConfig::new(
        Arc::new(model.session_files(SessionFileStore::new(workspace, None))),
        sandbox,
        checkpoints,
        MiddlewareStack::new(middleware).expect("middleware"),
        "test system prompt",
    )
    .session_context(test_session_context())
}

fn user_message(text: impl Into<String>) -> Op {
    user_message_with_attachments(text, Vec::new())
}

fn user_message_with_attachments(
    text: impl Into<String>,
    attachments: Vec<SessionFileReference>,
) -> Op {
    Op::Message {
        message: MessageSubmission {
            author: MessageAuthor::User,
            text: text.into(),
            attachments,
            reply: None,
            requested_delivery: None,
            target_turn_id: None,
        },
    }
}

async fn final_message(agent: &mut mobius::agent::Agent) -> String {
    let mut message = String::new();
    while let Some(event) = agent.next_event().await {
        match event.msg {
            EventMsg::AssistantMessage(event) => message = assistant_final_text(event),
            EventMsg::TurnComplete(_) => return message,
            EventMsg::Error(error) => panic!("{}", error.message),
            _ => {}
        }
    }
    panic!("agent disconnected")
}

fn assistant_final_text(event: AssistantMessageEvent) -> String {
    event
        .content
        .into_iter()
        .filter(|item| item.phase == ModelStepContentPhase::FinalAnswer)
        .map(|item| item.text)
        .collect()
}

async fn failed_turn(agent: &mut mobius::agent::Agent) -> std::sync::Arc<str> {
    let mut message = None;
    while let Some(event) = agent.next_event().await {
        match event.msg {
            EventMsg::Error(error) => message = Some(error.message),
            EventMsg::TurnAborted(_) => return message.expect("failed turn error"),
            EventMsg::TurnComplete(_) => panic!("turn unexpectedly completed"),
            _ => {}
        }
    }
    panic!("agent disconnected")
}

async fn upload_attachment(
    store: &SessionFileStore,
    session_id: &str,
    name: &str,
    media_type: &str,
    bytes: &[u8],
) -> SessionFileReference {
    let mut pending = store
        .begin_upload(
            session_id,
            name.into(),
            u64::try_from(bytes.len()).expect("attachment size"),
            media_type.into(),
        )
        .await
        .expect("begin attachment upload");
    let mut offset = 0_u64;
    for chunk in bytes.chunks(session_file_limits().max_upload_chunk_bytes) {
        offset = pending
            .append(offset, chunk)
            .await
            .expect("append attachment chunk");
    }
    pending.finish().await.expect("finish attachment upload")
}

fn request_image_count(input: &[Value]) -> usize {
    input
        .iter()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("input_image"))
        .count()
}

fn tool_response(call_id: &str, name: &str, arguments: &Value) -> ModelOutput {
    ModelOutput::from_output(
        vec![serde_json::json!({
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": arguments.to_string()
        })],
        false,
        usage(10),
    )
    .expect("valid tool response")
}

fn text_response(text: &str) -> ModelOutput {
    text_response_with_usage(text, usage(10))
}

fn text_response_with_usage(text: &str, usage: TokenUsage) -> ModelOutput {
    ModelOutput::from_output(
        vec![serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}]
        })],
        true,
        usage,
    )
    .expect("valid text response")
}

fn usage(input_tokens: i64) -> TokenUsage {
    TokenUsage {
        input_tokens,
        total_tokens: input_tokens + 1,
        output_tokens: 1,
        ..TokenUsage::default()
    }
}
