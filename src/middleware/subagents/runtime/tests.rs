use crate::SessionId;
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
    fail_at: Option<usize>,
    block_at: Option<usize>,
    retry_started: Notify,
    release_retry: Notify,
}

fn test_presentation() -> AgentPresentation {
    AgentPresentation {
        model: "test".into(),
        spawn_context: String::new(),
    }
}

impl CheckpointStore for BlockingRetryStore {
    fn message_accepted<'a>(
        &'a self,
        _session_id: &'a SessionId,
        _submission_id: &'a str,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn load<'a>(&'a self, _session_id: &'a SessionId) -> BoxFuture<'a, Result<Option<Checkpoint>>> {
        Box::pin(async { Ok(None) })
    }

    fn delete_sessions<'a>(&'a self, _session_ids: &'a [SessionId]) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn save_with_events(
        &self,
        checkpoint: Arc<Checkpoint>,
        _transcript_delta: Vec<Arc<Value>>,
        _execution: Option<ExecutionRecord>,
        events: Vec<TimestampedEvent>,
    ) -> BoxFuture<'_, Result<Vec<JournalEvent>>> {
        Box::pin(async move {
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
        _session_id: &'a SessionId,
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
        _session_id: &'a SessionId,
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
        let fail = self.fail_at == Some(save);
        let block = self.block_at == Some(save);
        Box::pin(async move {
            if fail {
                return Err(Error::Checkpoint("forced state save failure".into()));
            }
            if block {
                self.retry_started.notify_one();
                self.release_retry.notified().await;
            }
            Ok(())
        })
    }
}

impl CheckpointStore for FailOnceStore {
    fn message_accepted<'a>(
        &'a self,
        _session_id: &'a SessionId,
        _submission_id: &'a str,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn load<'a>(&'a self, _session_id: &'a SessionId) -> BoxFuture<'a, Result<Option<Checkpoint>>> {
        Box::pin(async { Ok(None) })
    }

    fn delete_sessions<'a>(&'a self, _session_ids: &'a [SessionId]) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn save_with_events(
        &self,
        checkpoint: Arc<Checkpoint>,
        _transcript_delta: Vec<Arc<Value>>,
        _execution: Option<ExecutionRecord>,
        events: Vec<TimestampedEvent>,
    ) -> BoxFuture<'_, Result<Vec<JournalEvent>>> {
        Box::pin(async move {
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
        _session_id: &'a SessionId,
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
        _session_id: &'a SessionId,
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
        .session_start(&test_context(checkpoints, Arc::new(|_| Ok(()))))
        .await
        .expect("initialize runtime");

    let failed = shared
        .reserve(
            &"root".parse().expect("session ID"),
            "/root/child",
            "/root",
            "child".parse().expect("session ID"),
            1,
            test_presentation(),
        )
        .await
        .is_err();
    let after_failure = shared
        .list(&"root".parse().expect("session ID"), None)
        .await
        .expect("list agents")
        .len();
    let retried = shared
        .reserve(
            &"root".parse().expect("session ID"),
            "/root/child",
            "/root",
            "child".parse().expect("session ID"),
            1,
            test_presentation(),
        )
        .await
        .is_ok();
    let after_retry = shared
        .list(&"root".parse().expect("session ID"), None)
        .await
        .expect("list agents")
        .len();

    assert_eq!(
        (failed, after_failure, retried, after_retry),
        (true, 0, true, 1)
    );
}

#[tokio::test]
async fn reserve_enforces_configured_concurrency_including_root() {
    let shared = Shared::new(3, 4);
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(FailOnceStore {
        fail_next_save: AtomicBool::new(false),
        saved_state: StdMutex::new(None),
    });
    shared
        .session_start(&test_context(checkpoints, Arc::new(|_| Ok(()))))
        .await
        .expect("initialize runtime");
    for index in 0..2 {
        shared
            .reserve(
                &"root".parse().expect("session ID"),
                &format!("/root/child_{index}"),
                "/root",
                format!("child-{index}").parse().expect("session ID"),
                1,
                test_presentation(),
            )
            .await
            .expect("reserve within concurrency limit");
    }

    let error = shared
        .reserve(
            &"root".parse().expect("session ID"),
            "/root/overflow",
            "/root",
            "overflow".parse().expect("session ID"),
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
async fn terminal_update_is_retained_until_its_checkpoint_marker_is_acknowledged() {
    let shared = test_shared();
    let store = Arc::new(FailOnceStore {
        fail_next_save: AtomicBool::new(false),
        saved_state: StdMutex::new(None),
    });
    let checkpoints: Arc<dyn CheckpointStore> = Arc::<FailOnceStore>::clone(&store);
    shared
        .session_start(&test_context(checkpoints, Arc::new(|_| Ok(()))))
        .await
        .expect("initialize runtime");
    shared
        .reserve(
            &"root".parse().expect("session ID"),
            "/root/child",
            "/root",
            "child".parse().expect("session ID"),
            1,
            test_presentation(),
        )
        .await
        .expect("reserve child");
    shared
        .finished(
            &"root".parse().expect("session ID"),
            "/root/child",
            AgentStatus::Completed,
            Some("done".into()),
        )
        .await
        .expect("finish child");

    let pending = shared
        .receive_updates(
            &"root".parse().expect("session ID"),
            "/root",
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .await
        .expect("receive updates");
    let (kind, text) = &pending[0];
    let id = kind
        .strip_prefix("subagent_update:")
        .expect("update kind")
        .to_owned();
    assert_eq!(
        text,
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
        .receive_updates(
            &"root".parse().expect("session ID"),
            "/root",
            &BTreeSet::from([id.as_str()]),
            &BTreeSet::new(),
        )
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
async fn stale_root_teardown_cannot_evict_a_reconnected_root() {
    let shared = Arc::new(test_shared());
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(FailOnceStore {
        fail_next_save: AtomicBool::new(false),
        saved_state: StdMutex::new(None),
    });
    shared
        .session_start(&test_context(
            Arc::clone(&checkpoints),
            Arc::new(|_| Ok(())),
        ))
        .await
        .expect("initialize runtime");
    shared
        .reserve(
            &"root".parse().expect("session ID"),
            "/root/child",
            "/root",
            "child".parse().expect("session ID"),
            1,
            test_presentation(),
        )
        .await
        .expect("reserve child");

    let old_root = shared
        .root(&"root".parse().expect("session ID"))
        .await
        .expect("old root");
    let writer = old_root.writer.lock().await;
    let mut removing = tokio::spawn({
        let shared = Arc::clone(&shared);
        async move {
            shared
                .remove_root(&"root".parse().expect("session ID"))
                .await
        }
    });
    tokio::task::yield_now().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut removing)
            .await
            .is_err(),
        "teardown should wait for the old root writer"
    );

    assert!(shared.roots.lock().await.remove("root").is_some());
    shared
        .session_start(&test_context(
            Arc::clone(&checkpoints),
            Arc::new(|_| Ok(())),
        ))
        .await
        .expect("reinitialize runtime");
    drop(writer);

    removing
        .await
        .expect("teardown task")
        .expect("remove old root");
    assert!(
        shared
            .root(&"root".parse().expect("session ID"))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn detached_root_mutation_cannot_persist_after_root_eviction() {
    let shared = Arc::new(test_shared());
    let store = Arc::new(BlockingRetryStore {
        saves: AtomicUsize::new(0),
        fail_at: None,
        block_at: Some(2),
        retry_started: Notify::new(),
        release_retry: Notify::new(),
    });
    let checkpoints: Arc<dyn CheckpointStore> = Arc::<BlockingRetryStore>::clone(&store);
    shared
        .session_start(&test_context(
            Arc::clone(&checkpoints),
            Arc::new(|_| Ok(())),
        ))
        .await
        .expect("initialize runtime");
    shared
        .reserve(
            &"root".parse().expect("session ID"),
            "/root/child",
            "/root",
            "child".parse().expect("session ID"),
            1,
            test_presentation(),
        )
        .await
        .expect("reserve child");

    let old_root = shared
        .root(&"root".parse().expect("session ID"))
        .await
        .expect("old root");
    let writer = old_root.writer.lock().await;
    let removing = {
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            shared
                .remove_root(&"root".parse().expect("session ID"))
                .await
        })
    };
    tokio::task::yield_now().await;
    let stale_mutation = {
        let shared = Arc::clone(&shared);
        let old_root = Arc::clone(&old_root);
        tokio::spawn(async move {
            let _writer = old_root.writer.lock().await;
            shared
                .commit_locked_root(
                    &"root".parse().expect("session ID"),
                    &old_root,
                    |root| {
                        root.tree.agents.insert(
                            "/root/racing".into(),
                            AgentRecord {
                                parent: "/root".into(),
                                session_id: "racing".parse().expect("session ID"),
                                depth: 1,
                                model: "test".into(),
                                spawn_context: String::new(),
                                active_turn_id: None,
                                status: AgentStatus::PendingInit,
                                last_message: None,
                            },
                        );
                        Ok(Stage::Changed(()))
                    },
                    OnPersistFailure::Abort,
                )
                .await
        })
    };
    tokio::task::yield_now().await;
    drop(writer);

    removing.await.expect("teardown task").expect("remove root");
    shared
        .session_start(&test_context(
            Arc::clone(&checkpoints),
            Arc::new(|_| Ok(())),
        ))
        .await
        .expect("reinitialize runtime");

    let racing = tokio::time::timeout(Duration::from_millis(100), stale_mutation).await;
    assert!(
        racing.is_ok(),
        "detached mutation should not block on persistence"
    );
    assert!(matches!(
        racing.expect("reserve timeout").expect("reserve task"),
        Err(Error::Unknown(_))
    ));
    assert!(
        shared
            .root(&"root".parse().expect("session ID"))
            .await
            .is_ok()
    );
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
    let checkpoints: Arc<dyn CheckpointStore> = Arc::<FailOnceStore>::clone(&store);
    shared
        .session_start(&test_context(
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
            &"root".parse().expect("session ID"),
            "/root/child",
            "/root",
            "child".parse().expect("session ID"),
            1,
            test_presentation(),
        )
        .await
        .expect("reserve child");
    store.fail_next_save.store(true, Ordering::SeqCst);

    shared
        .finished(
            &"root".parse().expect("session ID"),
            "/root/child",
            AgentStatus::Completed,
            Some("done".into()),
        )
        .await
        .expect("finish child");

    let agents = shared
        .list(&"root".parse().expect("session ID"), None)
        .await
        .expect("list agents");
    let updates = shared
        .wait(
            &"root".parse().expect("session ID"),
            "/root",
            Duration::ZERO,
        )
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
            updates == vec![Arc::<str>::from("/root/child")],
        ),
        (Some("errored"), true, Some("errored"), true, true)
    );
}

fn test_context(
    checkpoints: Arc<dyn CheckpointStore>,
    frontend: crate::middleware::FrontendEventSink,
) -> RuntimeContext {
    RuntimeContext {
        children: crate::agent::ChildAgents::default(),
        sender: crate::agent::test_sender(),
        checkpoints,
        session_id: "root".parse().expect("session ID"),
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
    Shared::new(2, 2)
}

#[test]
fn status_widget_counts_agents_and_marks_interrupted_ones() {
    let agent = |status| AgentRecord {
        parent: "/root".into(),
        session_id: "child".parse().expect("session ID"),
        depth: 1,
        model: "test".into(),
        spawn_context: String::new(),
        active_turn_id: None,
        status,
        last_message: None,
    };
    let mut tree = Tree::default();
    tree.agents
        .insert("/root/a".into(), agent(AgentStatus::Interrupted));
    tree.agents
        .insert("/root/b".into(), agent(AgentStatus::Completed));
    let widget = status_widget(&tree);
    assert_eq!(widget.count, Some(2));
    assert_eq!(widget.text, "2 subagents");
    let Some(FrontendWidgetContent::ActionList { items, .. }) = &widget.content else {
        panic!("agent list");
    };
    assert!(
        items
            .iter()
            .any(|item| item.state == FrontendListItemState::Interrupted)
    );
}
