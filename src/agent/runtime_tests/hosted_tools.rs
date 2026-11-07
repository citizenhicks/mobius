//! Hosted execution policy through the actual agent event sink.

use super::*;
use crate::ProviderError;
use crate::protocol::{HostedCallId, HostedToolExecution};

#[derive(Clone, Copy)]
enum SearchMode {
    Actual,
    PresentationOnly,
    Interrupted,
    InterruptedBeforeResult,
    Steps,
}

struct HostedSearchModel {
    mode: SearchMode,
    calls: AtomicUsize,
    executions: AtomicUsize,
}

impl Model for HostedSearchModel {
    fn info(&self) -> crate::protocol::ModelInfo {
        crate::protocol::ModelInfo {
            model: "hosted-search".into(),
            reasoning_effort: None,
        }
    }

    fn respond<'a>(
        &'a self,
        request: ModelRequest<'a>,
        events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(async move {
            let step = self.calls.fetch_add(1, Ordering::SeqCst);
            if matches!(self.mode, SearchMode::Steps) {
                assert_eq!(
                    request
                        .input
                        .iter()
                        .filter(|item| crate::protocol::internal_message_kind(item)
                            == Some("tool_progress"))
                        .count(),
                    // One append-only delta per earlier step; none are removed from the prefix.
                    step
                );
                assert_eq!(
                    request
                        .input
                        .iter()
                        .any(|item| crate::protocol::internal_message_kind(item)
                            == Some("tool_repeat")),
                    step >= 12
                );
            }
            let batch = if matches!(self.mode, SearchMode::Steps) {
                1
            } else {
                25
            };
            for index in 0..batch {
                let call_id: Arc<str> = format!("search-{index}").into();
                events(ModelEvent::WebSearchStarted {
                    call_id: Arc::clone(&call_id),
                })
                .await?;
                events(ModelEvent::WebSearchCompleted {
                    call_id: Arc::clone(&call_id),
                    action: WebSearchAction::Search {
                        queries: vec!["framework docs".into()],
                    },
                })
                .await?;
                if !matches!(self.mode, SearchMode::PresentationOnly) {
                    let hosted_id = HostedCallId::new(&call_id)?;
                    events(ModelEvent::HostedToolStarted {
                        call_id: hosted_id.clone(),
                    })
                    .await?;
                    if matches!(self.mode, SearchMode::InterruptedBeforeResult) {
                        return Err(Error::Provider(ProviderError::stream_interrupted(None)));
                    }
                    self.executions.fetch_add(1, Ordering::SeqCst);
                    events(ModelEvent::HostedToolCompleted(HostedToolExecution::new(
                        hosted_id,
                        "web_search",
                        Some(serde_json::json!({"query":"framework docs"})),
                        None,
                        ["https://example.com/docs"],
                    )?))
                    .await?;
                }
                if matches!(self.mode, SearchMode::Interrupted) {
                    return Err(Error::Provider(ProviderError::stream_interrupted(None)));
                }
            }
            if matches!(self.mode, SearchMode::Steps) {
                ModelOutput::from_output(
                    scripted_message("Checking sources.").output,
                    false,
                    TokenUsage::default(),
                )
            } else {
                Ok(scripted_message("done"))
            }
        })
    }
}

async fn run_hosted(mode: SearchMode) -> (Arc<HostedSearchModel>, Vec<EventMsg>, Checkpoint) {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoint.sqlite3")).expect("store"),
    );
    let model = Arc::new(HostedSearchModel {
        mode,
        calls: AtomicUsize::new(0),
        executions: AtomicUsize::new(0),
    });
    let mut agent = create_agent(config_with_model(
        workspace.path(),
        Arc::<SqliteCheckpoint>::clone(&checkpoints),
        "hosted-policy",
        "test",
        Arc::<HostedSearchModel>::clone(&model),
    ))
    .await
    .expect("agent");
    agent.sender().submit(user_op("research")).expect("submit");
    let events = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut events = Vec::new();
        while let Some(event) = agent.next_event().await {
            let done = matches!(
                event.msg,
                EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
            );
            events.push(event.msg);
            if done {
                break;
            }
        }
        events
    })
    .await
    .expect("bounded turn");
    let saved = checkpoints
        .load(&"hosted-policy".parse().expect("ID"))
        .await
        .expect("load")
        .expect("checkpoint");
    (model, events, saved)
}

#[tokio::test]
async fn hosted_tools_use_default_policy_without_tools_middleware_but_presentation_does_not_count()
{
    for (mode, guarded, requests) in [
        (SearchMode::Actual, true, 1),
        (SearchMode::Steps, true, 21),
        (SearchMode::PresentationOnly, false, 1),
    ] {
        let (model, events, saved) = run_hosted(mode).await;
        assert_eq!(model.calls.load(Ordering::SeqCst), requests);
        assert_eq!(
            model.executions.load(Ordering::SeqCst),
            if guarded { 21 } else { 0 }
        );
        assert_eq!(
            events
                .iter()
                .any(|event| matches!(event, EventMsg::TurnAborted(_))),
            guarded
        );
        assert_eq!(
            saved
                .context
                .iter()
                .any(|item| crate::protocol::internal_message_kind(item) == Some("tool_repeat")),
            guarded
        );
        let progress = saved
            .context
            .iter()
            .find(|item| crate::protocol::internal_message_kind(item) == Some("tool_progress"));
        assert_eq!(progress.is_some(), guarded);
        if let Some(progress) = progress {
            assert!(progress.to_string().contains("framework docs"));
            assert!(progress.to_string().contains("https://example.com/docs"));
        }
    }
}

#[tokio::test]
async fn an_interrupted_response_after_hosted_execution_is_not_replayed_and_keeps_its_history() {
    for (mode, completed) in [
        (SearchMode::Interrupted, true),
        (SearchMode::InterruptedBeforeResult, false),
    ] {
        let (model, events, saved) = run_hosted(mode).await;
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            model.executions.load(Ordering::SeqCst),
            usize::from(completed)
        );
        assert!(!events.iter().any(|event| matches!(event, EventMsg::ModelStepCompleted(step) if step.outcome == ModelStepOutcome::Retrying)));
        assert!(matches!(events.last(), Some(EventMsg::TurnAborted(_))));
        assert_eq!(
            saved
                .context
                .iter()
                .any(|item| crate::protocol::internal_message_kind(item) == Some("tool_progress")),
            completed
        );
    }
}
