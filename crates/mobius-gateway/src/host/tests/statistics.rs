//! Committed execution statistics and terminal transcript totals agree after real tools.

use super::bots::gateway_with_bot;
use super::hooks::install_model;
use super::*;
use crate::wire::TranscriptRowKind;
use mobius::backend::model::{Model, ModelEventSink, ModelOutput, ModelRequest};

#[tokio::test]
async fn committed_statistics_follow_actual_tool_results_and_match_terminal_work() {
    #[derive(Default)]
    struct PausedReads {
        release: tokio::sync::Notify,
        calls: AtomicU64,
    }
    impl Model for PausedReads {
        fn respond<'a>(
            &'a self,
            _: ModelRequest<'a>,
            _: ModelEventSink,
        ) -> mobius::BoxFuture<'a, mobius::Result<ModelOutput>> {
            let first = self.calls.fetch_add(1, Ordering::Relaxed) == 0;
            Box::pin(async move {
                let output = if first {
                    self.release.notified().await;
                    ["present.txt", "missing.txt"]
                        .into_iter()
                        .map(|path| {
                            serde_json::json!({
                                "type":"function_call", "call_id":path, "name":"read_file",
                                "arguments":serde_json::json!({"path":path}).to_string()
                            })
                        })
                        .collect()
                } else {
                    vec![
                        serde_json::json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"Done."}]}),
                    ]
                };
                ModelOutput::from_output(output, !first, TokenUsage::default())
            })
        }
    }
    let (root, gateway, bot) = gateway_with_bot().await;
    let model = Arc::new(PausedReads::default());
    install_model(&gateway, &bot, Arc::clone(&model)).await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("present.txt"), "fixture").unwrap();
    let chat = gateway.create_session(&workspace, &bot.id).await.unwrap();
    let mut events = chat.subscribe();
    chat.submit(Submission {
        id: "read".into(),
        op: Op::Message {
            message: MessageSubmission {
                author: MessageAuthor::User,
                text: "Read both files".into(),
                attachments: Vec::new(),
                reply: None,
                requested_delivery: None,
                target_turn_id: None,
            },
        },
    })
    .await
    .unwrap();
    let active = next_statistics(&mut events, |stats| {
        stats
            .active
            .as_ref()
            .is_some_and(|run| run.model_calls == 1)
    })
    .await;
    assert_eq!(active.completed.run_count, 0);
    assert_eq!(active.active.as_ref().unwrap().tool_calls, 0);
    model.release.notify_one();
    let completed = next_statistics(&mut events, |stats| {
        stats.completed.run_count == 1 && stats.active.is_none()
    })
    .await;
    assert_eq!(completed.completed.model_calls, 2);
    assert_eq!(completed.completed.tool_calls, 2);
    assert_eq!(completed.completed.failed_tool_calls, 1);
    assert_eq!(model.calls.load(Ordering::Relaxed), 2);
    assert_eq!(completed, chat.ready().await.unwrap().run_stats);
    let page = chat.history_page(None).await.unwrap().page;
    let counts = page
        .rows
        .iter()
        .find_map(|row| match &row.kind {
            TranscriptRowKind::Worked { counts, .. } => Some(counts),
            _ => None,
        })
        .expect("terminal work summary");
    assert_eq!(u64::from(counts.tool_calls), completed.completed.tool_calls);
    assert_eq!(
        u64::from(counts.errors),
        completed.completed.failed_tool_calls
    );
    gateway.shutdown().await;
}

async fn next_statistics(
    events: &mut broadcast::Receiver<SharedFrame>,
    ready: fn(&RunStats) -> bool,
) -> Arc<RunStats> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let frame = events.recv().await.unwrap();
            if let ServerMessage::AgentEvent {
                run_stats: Some(stats),
                ..
            } = &frame.message
                && ready(stats)
            {
                break Arc::clone(stats);
            }
        }
    })
    .await
    .expect("committed statistics arrive without reopening the chat")
}
