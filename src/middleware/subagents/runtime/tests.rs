use std::collections::BTreeSet;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::*;
use crate::BoxFuture;
use crate::backend::checkpoint::Checkpoint;
use crate::backend::checkpoint::EventPage;
use crate::backend::checkpoint::EventPageRequest;
use crate::backend::checkpoint::ExecutionRecord;
use crate::backend::checkpoint::JournalEvent;
use crate::backend::checkpoint::TimestampedEvent;
use crate::protocol::Event;

struct FailOnceStore {
    fail_next_save: AtomicBool,
    saved_state: StdMutex<Option<Value>>,
}

struct BlockingRetryStore {
    saves: AtomicUsize,
    retry_started: Notify,
    release_retry: Notify,
}

fn test_presentation() -> AgentPresentation {
    AgentPresentation {
        model: "test".into(),
        spawn_context: String::new(),
    }
}

#[test]
fn status_widget_owns_its_picker() {
    let mut tree = Tree::default();
    tree.agents.insert(
        "/root/team/reviewer".into(),
        AgentRecord {
            parent: String::new(),
            session_id: "child-1".into(),
            depth: 1,
            model: "openai::gpt-5::high".into(),
            spawn_context: "Full context".into(),
            active_turn_id: None,
            status: AgentStatus::Running,
            last_message: None,
        },
    );

    let widget = status_widget(&tree);
    assert_eq!(widget.symbol, Some(FrontendSymbol::Agent));
    assert_eq!(widget.text, "1");
    assert!(!widget.icon_only);
    assert!(matches!(
        widget.content,
        Some(FrontendWidgetContent::Picker { title, options })
            if title == "Subagents"
                && options.len() == 1
                && options[0].label == "reviewer"
                && options[0].description == "running"
                && options[0].detail == "openai::gpt-5::high"
                && options[0].symbol == Some(FrontendSymbol::Agent)
                && !options[0].shows_detail
                && matches!(
                    &options[0].op,
                    Op::CapabilityCommand { arguments, .. }
                        if arguments == "/root/team/reviewer"
                )
    ));
}

#[test]
fn persisted_tree_rejects_an_oversized_last_message() {
    let mut tree = Tree::default();
    tree.agents.insert(
        "/root/reviewer".into(),
        AgentRecord {
            parent: "/root".into(),
            session_id: "child".into(),
            depth: 1,
            model: "test".into(),
            spawn_context: String::new(),
            active_turn_id: None,
            status: AgentStatus::Errored,
            last_message: Some("x".repeat(MAX_MESSAGE_BYTES + 1)),
        },
    );

    assert!(matches!(validate_tree(&tree, 2), Err(Error::Config(_))));
}

#[tokio::test]
async fn errored_subagent_preview_ends_with_its_terminal_message() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        crate::backend::checkpoint::sqlite::SqliteCheckpoint::new(
            workspace.path().join("checkpoints.sqlite3"),
        )
        .expect("checkpoint store"),
    );
    let root = Checkpoint::empty("root");
    checkpoints.save(&root, &[], None).await.expect("save root");
    let child = Checkpoint::empty("child");
    checkpoints
        .save(&child, &[], None)
        .await
        .expect("save child");
    for (recorded_at_ms, msg) in [
        EventMsg::TurnStarted(crate::protocol::TurnStartedEvent {
            turn_id: "turn-1".into(),
            model_context_window: None,
        }),
        EventMsg::Message(crate::protocol::MessageEvent {
            author: crate::protocol::MessageAuthor::User,
            delivery: crate::protocol::MessageDelivery::Turn,
            text: "review this".into(),
            attachments: Vec::new(),
            message_target: None,
        }),
    ]
    .into_iter()
    .enumerate()
    {
        checkpoints
            .append_event(
                "child",
                i64::try_from(recorded_at_ms).expect("timestamp"),
                &Event {
                    submission_id: None,
                    msg,
                },
            )
            .await
            .expect("append child event");
    }
    let shared = test_shared();
    shared
        .session_start(test_context(checkpoints, Arc::new(|_| Ok(()))))
        .await
        .expect("initialize runtime");
    shared
        .reserve(
            "root",
            "/root/reviewer",
            "/root",
            "child".into(),
            1,
            test_presentation(),
        )
        .await
        .expect("reserve child");
    shared
        .finished(
            "root",
            "/root/reviewer",
            AgentStatus::Errored,
            Some("provider error: servers are currently overloaded".into()),
        )
        .await
        .expect("fail child");

    let preview = shared
        .preview("root", "/root/reviewer", None)
        .await
        .expect("preview errored child");
    let events = preview
        .events
        .iter()
        .map(|event| event.event.clone())
        .collect::<Vec<_>>();

    assert!(matches!(
        events.as_slice(),
        [
            EventMsg::TurnStarted(started),
            EventMsg::Message(message),
            EventMsg::Frontend(FrontendEvent::Render { capability, block }),
        ] if started.turn_id == "turn-1"
            && message.text == "review this"
            && capability == "subagents"
            && block.title == "Subagent error"
            && block.text == "provider error: servers are currently overloaded"
            && block.symbol == Some(FrontendSymbol::Agent)
            && block.tone == FrontendTone::Error
    ));
}

impl CheckpointStore for BlockingRetryStore {
    fn load<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, Result<Option<Checkpoint>>> {
        Box::pin(async { Ok(None) })
    }

    fn delete_session<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn save<'a>(
        &'a self,
        _checkpoint: &'a Checkpoint,
        _transcript_delta: &'a [Value],
        _execution: Option<&'a ExecutionRecord>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn save_with_events<'a>(
        &'a self,
        checkpoint: &'a Checkpoint,
        transcript_delta: &'a [Value],
        execution: Option<&'a ExecutionRecord>,
        events: &'a [TimestampedEvent],
    ) -> BoxFuture<'a, Result<Vec<JournalEvent>>> {
        Box::pin(async move {
            self.save(checkpoint, transcript_delta, execution).await?;
            let mut records = Vec::with_capacity(events.len());
            for event in events {
                records.push(
                    self.append_event(&checkpoint.session_id, event.recorded_at_ms, &event.event)
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
        event: &'a Event,
    ) -> BoxFuture<'a, Result<JournalEvent>> {
        let event = event.clone();
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
        _scope: &'a str,
        _key: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(async { Ok(None) })
    }

    fn save_state<'a>(
        &'a self,
        _scope: &'a str,
        _key: &'a str,
        _value: &'a Value,
    ) -> BoxFuture<'a, Result<()>> {
        let save = self.saves.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            match save {
                1 => Err(Error::Checkpoint("forced state save failure".into())),
                2 => {
                    self.retry_started.notify_one();
                    self.release_retry.notified().await;
                    Ok(())
                }
                _ => Ok(()),
            }
        })
    }
}

impl CheckpointStore for FailOnceStore {
    fn load<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, Result<Option<Checkpoint>>> {
        Box::pin(async { Ok(None) })
    }

    fn delete_session<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn save<'a>(
        &'a self,
        _checkpoint: &'a Checkpoint,
        _transcript_delta: &'a [Value],
        _execution: Option<&'a ExecutionRecord>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn save_with_events<'a>(
        &'a self,
        checkpoint: &'a Checkpoint,
        transcript_delta: &'a [Value],
        execution: Option<&'a ExecutionRecord>,
        events: &'a [TimestampedEvent],
    ) -> BoxFuture<'a, Result<Vec<JournalEvent>>> {
        Box::pin(async move {
            self.save(checkpoint, transcript_delta, execution).await?;
            let mut records = Vec::with_capacity(events.len());
            for event in events {
                records.push(
                    self.append_event(&checkpoint.session_id, event.recorded_at_ms, &event.event)
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
        event: &'a Event,
    ) -> BoxFuture<'a, Result<JournalEvent>> {
        let event = event.clone();
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
        _scope: &'a str,
        _key: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(async { Ok(None) })
    }

    fn save_state<'a>(
        &'a self,
        _scope: &'a str,
        _key: &'a str,
        value: &'a Value,
    ) -> BoxFuture<'a, Result<()>> {
        let fail = self.fail_next_save.swap(false, Ordering::SeqCst);
        if !fail {
            *self.saved_state.lock().expect("saved state") = Some(value.clone());
        }
        Box::pin(async move {
            if fail {
                Err(Error::Checkpoint("forced state save failure".into()))
            } else {
                Ok(())
            }
        })
    }
}

#[tokio::test]
async fn failed_persist_does_not_mutate_runtime_state() {
    let shared = test_shared();
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(FailOnceStore {
        fail_next_save: AtomicBool::new(true),
        saved_state: StdMutex::new(None),
    });
    shared
        .session_start(test_context(checkpoints, Arc::new(|_| Ok(()))))
        .await
        .expect("initialize runtime");

    let failed = shared
        .reserve(
            "root",
            "/root/child",
            "/root",
            "child".into(),
            1,
            test_presentation(),
        )
        .await
        .is_err();
    let after_failure = shared.list("root", None).await.expect("list agents").len();
    let retried = shared
        .reserve(
            "root",
            "/root/child",
            "/root",
            "child".into(),
            1,
            test_presentation(),
        )
        .await
        .is_ok();
    let after_retry = shared.list("root", None).await.expect("list agents").len();

    assert_eq!(
        (failed, after_failure, retried, after_retry),
        (true, 0, true, 1)
    );
}

#[tokio::test]
async fn empty_initial_tree_is_silent_and_empty_transition_removes_widget() {
    let shared = test_shared();
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(FailOnceStore {
        fail_next_save: AtomicBool::new(false),
        saved_state: StdMutex::new(None),
    });
    let frontend_events = Arc::new(StdMutex::new(Vec::new()));
    let events = Arc::clone(&frontend_events);
    shared
        .session_start(test_context(
            checkpoints,
            Arc::new(move |event| {
                events.lock().expect("frontend events").push(event);
                Ok(())
            }),
        ))
        .await
        .expect("initialize runtime");
    assert!(frontend_events.lock().expect("frontend events").is_empty());

    shared
        .reserve(
            "root",
            "/root/child",
            "/root",
            "child".into(),
            1,
            test_presentation(),
        )
        .await
        .expect("reserve child");
    shared
        .remove("root", "/root/child")
        .await
        .expect("remove child");

    let events = frontend_events.lock().expect("frontend events");
    assert!(matches!(
        events.as_slice(),
        [
            FrontendEvent::Widget { capability, .. },
            FrontendEvent::RemoveWidget {
                capability: removed_capability,
                id,
            },
        ] if capability == "subagents"
            && removed_capability == "subagents"
            && id == "status"
    ));
}

#[tokio::test]
async fn wait_returns_immediately_without_an_active_peer() {
    let shared = test_shared();
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(FailOnceStore {
        fail_next_save: AtomicBool::new(false),
        saved_state: StdMutex::new(None),
    });
    shared
        .session_start(test_context(checkpoints, Arc::new(|_| Ok(()))))
        .await
        .expect("initialize runtime");
    shared
        .reserve(
            "root",
            "/root/child",
            "/root",
            "child".into(),
            1,
            test_presentation(),
        )
        .await
        .expect("reserve child");
    shared
        .rollback("root", "/root/child", AgentStatus::Completed)
        .await
        .expect("complete child");

    let updates = tokio::time::timeout(
        Duration::from_millis(100),
        shared.wait("root", "/root", Duration::from_secs(10)),
    )
    .await
    .expect("wait should not sleep without active peers")
    .expect("wait for updates");

    assert!(updates.is_empty());
}

#[tokio::test]
async fn reserve_enforces_configured_concurrency_including_root() {
    let shared = Shared::new(3, 4).expect("valid limits");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(FailOnceStore {
        fail_next_save: AtomicBool::new(false),
        saved_state: StdMutex::new(None),
    });
    shared
        .session_start(test_context(checkpoints, Arc::new(|_| Ok(()))))
        .await
        .expect("initialize runtime");
    for index in 0..2 {
        shared
            .reserve(
                "root",
                &format!("/root/child_{index}"),
                "/root",
                format!("child-{index}"),
                1,
                test_presentation(),
            )
            .await
            .expect("reserve within concurrency limit");
    }

    let error = shared
        .reserve(
            "root",
            "/root/overflow",
            "/root",
            "overflow".into(),
            1,
            test_presentation(),
        )
        .await
        .expect_err("reject agent beyond concurrency limit");

    assert_eq!(
        error.to_string(),
        "agent stopped: subagent concurrency limit 3 (including root) reached"
    );
}

#[tokio::test]
async fn reserve_enforces_configured_agent_limit_including_root() {
    let shared = Shared::new(2, 3).expect("valid limits");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(FailOnceStore {
        fail_next_save: AtomicBool::new(false),
        saved_state: StdMutex::new(None),
    });
    shared
        .session_start(test_context(checkpoints, Arc::new(|_| Ok(()))))
        .await
        .expect("initialize runtime");
    for index in 0..2 {
        let path = format!("/root/child_{index}");
        shared
            .reserve(
                "root",
                &path,
                "/root",
                format!("child-{index}"),
                1,
                test_presentation(),
            )
            .await
            .expect("reserve within agent limit");
        shared
            .rollback("root", &path, AgentStatus::Completed)
            .await
            .expect("complete child");
    }

    let error = shared
        .reserve(
            "root",
            "/root/overflow",
            "/root",
            "overflow".into(),
            1,
            test_presentation(),
        )
        .await
        .expect_err("reject agent beyond agent limit");

    assert_eq!(
        error.to_string(),
        "agent stopped: subagent limit 3 (including root) reached"
    );
}

#[tokio::test]
async fn terminal_update_is_retained_until_its_checkpoint_marker_is_acknowledged() {
    let shared = test_shared();
    let store = Arc::new(FailOnceStore {
        fail_next_save: AtomicBool::new(false),
        saved_state: StdMutex::new(None),
    });
    let checkpoints: Arc<dyn CheckpointStore> = store.clone();
    shared
        .session_start(test_context(checkpoints, Arc::new(|_| Ok(()))))
        .await
        .expect("initialize runtime");
    shared
        .reserve(
            "root",
            "/root/child",
            "/root",
            "child".into(),
            1,
            test_presentation(),
        )
        .await
        .expect("reserve child");
    shared
        .finished(
            "root",
            "/root/child",
            AgentStatus::Completed,
            Some("done".into()),
        )
        .await
        .expect("finish child");

    let pending = shared
        .receive_updates("root", "/root", &BTreeSet::new())
        .await
        .expect("receive updates");
    let id = pending[0].id.clone();
    assert_eq!(
        pending[0].render(&BTreeSet::new()),
        "<subagent_update agent=\"/root/child\" status=\"completed\">\ndone\n</subagent_update>"
    );
    let updates_len = store
        .saved_state
        .lock()
        .expect("saved state")
        .as_ref()
        .and_then(|state| state["updates"].as_array())
        .map(Vec::len);
    assert_eq!(updates_len, Some(1));

    shared
        .receive_updates("root", "/root", &BTreeSet::from([id]))
        .await
        .expect("acknowledge update");

    let updates_len = store
        .saved_state
        .lock()
        .expect("saved state")
        .as_ref()
        .and_then(|state| state["updates"].as_array())
        .map(Vec::len);
    assert_eq!(updates_len, Some(0));
}

#[tokio::test]
async fn terminal_update_does_not_repeat_a_delivered_parent_report() {
    let shared = test_shared();
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(FailOnceStore {
        fail_next_save: AtomicBool::new(false),
        saved_state: StdMutex::new(None),
    });
    shared
        .session_start(test_context(checkpoints, Arc::new(|_| Ok(()))))
        .await
        .expect("initialize runtime");
    shared
        .reserve(
            "root",
            "/root/child",
            "/root",
            "child".into(),
            1,
            test_presentation(),
        )
        .await
        .expect("reserve child");
    shared
        .root("root")
        .await
        .expect("root runtime")
        .state
        .lock()
        .await
        .parent_reports
        .insert(
            "/root/child".into(),
            vec![
                crate::protocol::MessageSubmission {
                    author: crate::protocol::MessageAuthor::Peer {
                        message_id: "report-a".into(),
                        session_id: "child".into(),
                        handle: "child".into(),
                    },
                    text: "done".into(),
                    attachments: Vec::new(),
                    requested_delivery: None,
                    target_turn_id: None,
                },
                crate::protocol::MessageSubmission {
                    author: crate::protocol::MessageAuthor::Peer {
                        message_id: "report-b".into(),
                        session_id: "child".into(),
                        handle: "child".into(),
                    },
                    text: "still checking".into(),
                    attachments: Vec::new(),
                    requested_delivery: None,
                    target_turn_id: None,
                },
            ],
        );
    shared
        .finished(
            "root",
            "/root/child",
            AgentStatus::Completed,
            Some("done".into()),
        )
        .await
        .expect("finish child");

    let pending = shared
        .receive_updates("root", "/root", &BTreeSet::new())
        .await
        .expect("receive updates");

    assert_eq!(
        pending[0].render(&BTreeSet::from(["report-a".into()])),
        "<subagent_update agent=\"/root/child\" status=\"completed\">\n\n</subagent_update>"
    );
}

#[tokio::test]
async fn remove_root_evicts_runtime_state() {
    let shared = test_shared();
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(FailOnceStore {
        fail_next_save: AtomicBool::new(false),
        saved_state: StdMutex::new(None),
    });
    shared
        .session_start(test_context(checkpoints, Arc::new(|_| Ok(()))))
        .await
        .expect("initialize runtime");

    shared.remove_root("root").await;

    assert!(shared.root("root").await.is_err());
}

#[tokio::test]
async fn terminal_persist_failure_is_retried_as_a_durable_error() {
    let shared = test_shared();
    let store = Arc::new(FailOnceStore {
        fail_next_save: AtomicBool::new(false),
        saved_state: StdMutex::new(None),
    });
    let frontend_events = Arc::new(StdMutex::new(Vec::new()));
    let events = Arc::clone(&frontend_events);
    let checkpoints: Arc<dyn CheckpointStore> = store.clone();
    shared
        .session_start(test_context(
            checkpoints,
            Arc::new(move |event| {
                events.lock().expect("frontend events").push(event);
                Ok(())
            }),
        ))
        .await
        .expect("initialize runtime");
    shared
        .reserve(
            "root",
            "/root/child",
            "/root",
            "child".into(),
            1,
            test_presentation(),
        )
        .await
        .expect("reserve child");
    store.fail_next_save.store(true, Ordering::SeqCst);

    shared
        .finished(
            "root",
            "/root/child",
            AgentStatus::Completed,
            Some("done".into()),
        )
        .await
        .expect("finish child");

    let agents = shared.list("root", None).await.expect("list agents");
    let updates = shared
        .wait("root", "/root", Duration::ZERO)
        .await
        .expect("parent update");
    let durable = store
        .saved_state
        .lock()
        .expect("saved state")
        .clone()
        .expect("retried state");
    let rendered_error = frontend_events
        .lock()
        .expect("frontend events")
        .iter()
        .any(|event| {
            matches!(
                event,
                FrontendEvent::Render { block, .. }
                    if block.text.contains("state persistence failed")
            )
        });

    assert_eq!(
        (
            agents[0]["status"].as_str(),
            agents[0]["last_message"]
                .as_str()
                .is_some_and(|message| message.contains("state persistence failed")),
            durable["agents"]["/root/child"]["status"].as_str(),
            rendered_error,
            updates == vec!["/root/child".to_string()],
        ),
        (Some("errored"), true, Some("errored"), true, true)
    );
}

#[tokio::test]
async fn terminal_persist_failure_notifies_after_the_retry_commits() {
    let shared = Arc::new(test_shared());
    let store = Arc::new(BlockingRetryStore {
        saves: AtomicUsize::new(0),
        retry_started: Notify::new(),
        release_retry: Notify::new(),
    });
    let checkpoints: Arc<dyn CheckpointStore> = store.clone();
    shared
        .session_start(test_context(checkpoints, Arc::new(|_| Ok(()))))
        .await
        .expect("initialize runtime");
    shared
        .reserve(
            "root",
            "/root/child",
            "/root",
            "child".into(),
            1,
            test_presentation(),
        )
        .await
        .expect("reserve child");
    let before_commit = shared.changed.notified();
    tokio::pin!(before_commit);
    before_commit.as_mut().enable();
    let finishing = {
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            shared
                .finished(
                    "root",
                    "/root/child",
                    AgentStatus::Completed,
                    Some("done".into()),
                )
                .await
        })
    };
    store.retry_started.notified().await;
    let premature = tokio::time::timeout(Duration::from_millis(10), before_commit.as_mut()).await;
    let agents = shared.list("root", None).await.expect("pre-commit state");
    assert!(premature.is_err() && agents[0]["status"] == "pending_init");

    store.release_retry.notify_one();
    finishing.await.expect("finish task").expect("finish child");

    tokio::time::timeout(Duration::from_millis(100), before_commit)
        .await
        .expect("terminal commit notification");
}

fn test_context(
    checkpoints: Arc<dyn CheckpointStore>,
    frontend: crate::middleware::FrontendEventSink,
) -> RuntimeContext {
    RuntimeContext {
        sender: crate::agent::test_sender(),
        checkpoints,
        session_id: "root".into(),
        model_route: "test".into(),
        model: "model".into(),
        approval_policy: crate::backend::sandbox::ApprovalPolicy::Ask,
        session_context: Default::default(),
        metadata: Default::default(),
        role: crate::agent::AgentRole::Main,
        frontend,
    }
}

fn test_shared() -> Shared {
    Shared::new(2, 2).expect("valid test limits")
}
