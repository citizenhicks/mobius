use super::*;
use mobius::backend::checkpoint::{Checkpoint, CheckpointStore, sqlite::SqliteCheckpoint};
use mobius::backend::model::{Model, ModelEventSink, ModelOutput, ModelRequest, ModelRouter};
use mobius::protocol::ConversationRole;

struct ExtractionModel(mpsc::Sender<(String, oneshot::Sender<String>)>);

impl Model for ExtractionModel {
    fn respond<'a>(
        &'a self,
        request: ModelRequest<'a>,
        _events: ModelEventSink,
    ) -> mobius::BoxFuture<'a, mobius::Result<ModelOutput>> {
        Box::pin(async move {
            assert!(request.tools.is_empty() && !request.allow_hosted_tools);
            assert!(!request.allow_continuation);
            let (answer, received) = oneshot::channel();
            self.0
                .send((serde_json::to_string(request.input)?, answer))
                .await
                .expect("request observed");
            let text = received.await.expect("test response");
            ModelOutput::from_output(
                vec![
                    serde_json::json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}),
                ],
                true,
                TokenUsage::default(),
            )
        })
    }
}

#[tokio::test]
async fn queued_voice_tasks_keep_received_context_run_in_order_and_cancel_on_stop() {
    let directory = tempfile::tempdir().expect("directory");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoint.sqlite3")).expect("store"),
    );
    let mut parent = Checkpoint::empty("parent");
    parent.session_context.bot_id = "bot".into();
    checkpoints.save(&parent, &[], None).await.expect("parent");
    let frontend: mobius::middleware::FrontendEventSink = Arc::new(|_| Ok(()));
    let mut transcript =
        VoiceTranscript::open(Arc::clone(&checkpoints), "parent", Arc::clone(&frontend))
            .await
            .expect("voice");
    transcript
        .record(
            "decision",
            ConversationRole::Assistant,
            "Use blue; preserve toolbar.",
            false,
        )
        .await
        .expect("discussion");
    let (requests, mut received) = mpsc::channel(2);
    let model = crate::host::RealtimeModel {
        router: Arc::new(ModelRouter::new(
            "test",
            Arc::new(ExtractionModel(requests)),
        )),
        voice: None,
        route: "test".into(),
        provider_instance: "test".into(),
        active_turn_id: None,
        checkpoints: Arc::clone(&checkpoints),
        frontend,
    };
    let mut pending = VecDeque::new();
    for id in ["first", "second"] {
        pending.push_back(PendingVoiceTask {
            id: id.into(),
            text: format!("Do {id} task"),
            context: transcript.task_context().await.expect("snapshot"),
        });
    }
    // Finalization replaces the original deltas; a cursor reread would lose the agreed decision.
    transcript
        .record(
            "decision",
            ConversationRole::Assistant,
            "A later correction",
            true,
        )
        .await
        .expect("final");
    let mut resolving = JoinSet::new();
    start_next_task(&mut resolving, &mut pending, &model, "parent");
    start_next_task(&mut resolving, &mut pending, &model, "parent");
    assert_eq!(pending.len(), 1);
    assert_eq!(resolving.len(), 1);
    let (input, answer) = tokio::time::timeout(Duration::from_secs(2), received.recv())
        .await
        .expect("first starts")
        .expect("first request");
    assert!(input.contains("Do first task") && input.contains("Use blue; preserve toolbar."));
    assert!(!input.contains("A later correction"));
    answer
        .send(r#"{"task":"First complete task"}"#.into())
        .expect("answer");
    let (id, result) = tokio::time::timeout(Duration::from_secs(2), resolving.join_next())
        .await
        .expect("first resolves")
        .expect("task")
        .expect("join");
    assert_eq!(id, "first");
    assert_eq!(result.expect("extraction").0, "First complete task");
    start_next_task(&mut resolving, &mut pending, &model, "parent");
    let (input, mut answer) = tokio::time::timeout(Duration::from_secs(2), received.recv())
        .await
        .expect("second starts")
        .expect("second request");
    assert!(input.contains("Do second task") && input.contains("Use blue; preserve toolbar."));
    drop(resolving);
    tokio::time::timeout(Duration::from_secs(2), answer.closed())
        .await
        .expect("call stop cancels inference");
    let unchanged = checkpoints
        .load("parent")
        .await
        .expect("parent")
        .expect("exists");
    assert_eq!(unchanged.context, parent.context);
    assert_eq!(unchanged.sequence, parent.sequence);
}
