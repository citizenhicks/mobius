use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

use super::*;
use crate::BoxFuture;

struct CancellationGuard(Arc<AtomicBool>);

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct PendingBackend {
    started: Arc<Notify>,
    cancelled: Arc<AtomicBool>,
    finish: Option<Arc<Notify>>,
}

impl SandboxBackend for PendingBackend {
    crate::backend::sandbox::unreachable_backend_methods!();

    fn execute<'a>(
        &'a self,
        _command: &'a str,
        _sandbox_mode: SandboxMode,
        _network_access: NetworkAccess,
        _mode: CommandMode,
        _output: CommandOutputSink,
    ) -> BoxFuture<'a, Result<CommandOutput>> {
        Box::pin(async move {
            let _guard = CancellationGuard(Arc::clone(&self.cancelled));
            self.started.notify_one();
            match &self.finish {
                Some(finish) => finish.notified().await,
                None => std::future::pending().await,
            }
            Ok(command_output())
        })
    }
}

fn command_output() -> CommandOutput {
    CommandOutput {
        exit_code: 3,
        stdout: "finished".into(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
    }
}

#[tokio::test]
async fn completed_command_is_not_evicted_before_initial_delivery() {
    let finish = Arc::new(Notify::new());
    let backend: Arc<dyn SandboxBackend> = Arc::new(PendingBackend {
        started: Arc::default(),
        cancelled: Arc::default(),
        finish: Some(Arc::clone(&finish)),
    });
    let commands = BackgroundCommands::new(1);
    let session: SessionId = "session".parse().expect("session ID");
    let (notices, mut received) = tokio::sync::mpsc::unbounded_channel();
    commands
        .listen(
            &session,
            Arc::new(move |_, notice| {
                notices.send(notice).expect("notice receiver");
            }),
            Arc::new(|_| Ok(())),
        )
        .expect("listener");
    let start = || {
        commands.start(
            &session,
            Arc::clone(&backend),
            "printf first".into(),
            SandboxMode::DangerFullAccess,
            NetworkAccess::Denied,
            CommandOutputSink::default(),
        )
    };
    let first = start().expect("first command");
    finish.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !commands.entries.lock().expect("entries")[&first.id]
            .task
            .is_finished()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first command finished");

    assert!(start().is_err());
    let output = first
        .wait(&session, std::time::Duration::ZERO)
        .await
        .expect("first result");
    assert_eq!(output.status, BackgroundCommandStatus::Exited);
    assert_eq!(output.stdout, "finished");
    assert!(
        received.try_recv().is_err(),
        "collected command sent a notice"
    );
    start().expect("collected result releases capacity");
}

#[test]
fn completed_rows_are_bounded_without_evicting_active_commands() {
    let (sent, received) = std::sync::mpsc::channel();
    let listener = SessionListener {
        notice: Arc::new(|_, _| ()),
        frontend: Arc::new(move |event| {
            sent.send(event).expect("widget receiver");
            Ok(())
        }),
        rows: Mutex::default(),
    };
    let state = AtomicU8::new(0);
    listener
        .started(&Arc::from("active"), "build".into(), &state)
        .expect("active command");
    for n in 0..=MAX_RECENT_RESULTS {
        let id: Arc<str> = format!("done-{n}").into();
        listener
            .started(&id, id.to_string(), &state)
            .expect("start");
        listener
            .finished(&id, FrontendListItemState::Completed)
            .expect("finish");
    }
    let FrontendEvent::Widget { item, .. } = received.try_iter().last().expect("last update")
    else {
        panic!("command list")
    };
    assert_eq!(item.count, Some(1));
    let Some(FrontendWidgetContent::ActionList { items, .. }) = &item.content else {
        panic!("command rows")
    };
    assert_eq!(items.len(), 1 + MAX_RECENT_RESULTS);
    assert_eq!(items[0].id.as_ref(), "active");
    assert!(items[1..].iter().all(|row| row.id.as_ref() != "done-0"
        && row.state == FrontendListItemState::Completed
        && row.actions.is_empty()));
}

#[tokio::test]
async fn session_listeners_receive_only_their_held_commands() {
    let commands = BackgroundCommands::default();
    let (notices, mut received) = tokio::sync::mpsc::unbounded_channel();
    let (updates, mut widgets) = tokio::sync::mpsc::unbounded_channel();
    for name in ["session-a", "session-b"] {
        let session: SessionId = name.parse().expect("session ID");
        let notices = notices.clone();
        let updates = updates.clone();
        commands
            .listen(
                &session,
                Arc::new(move |id, notice| {
                    notices
                        .send((name, id.to_owned(), notice))
                        .expect("notice receiver");
                }),
                Arc::new(move |event| {
                    updates.send((name, event)).expect("widget receiver");
                    Ok(())
                }),
            )
            .expect("listener");
    }
    let mut pending = Vec::new();
    for (name, count) in [("session-a", 1), ("session-b", 1), ("session-a", 2)] {
        let session: SessionId = name.parse().expect("session ID");
        let finish = Arc::new(Notify::new());
        let started = Arc::new(Notify::new());
        let command = commands
            .start(
                &session,
                Arc::new(PendingBackend {
                    started: Arc::clone(&started),
                    cancelled: Arc::default(),
                    finish: Some(Arc::clone(&finish)),
                }),
                name.into(),
                SandboxMode::DangerFullAccess,
                NetworkAccess::Denied,
                CommandOutputSink::default(),
            )
            .expect("start");
        started.notified().await;
        let held = command
            .wait(&session, std::time::Duration::ZERO)
            .await
            .expect("held");
        assert_eq!(held.status, BackgroundCommandStatus::Running);
        let (owner, event) = widgets.try_recv().expect("start widget");
        assert_eq!(owner, name);
        let FrontendEvent::Widget { item, .. } = event else {
            panic!("running widget")
        };
        assert_eq!(item.count, Some(count));
        let Some(FrontendWidgetContent::ActionList { items, .. }) = &item.content else {
            panic!("command list")
        };
        assert!(
            items.iter().all(|item| item.text == name),
            "another session's command leaked"
        );
        pending.push((session, held.command_id.expect("command ID"), finish));
    }
    let (session, stopped, _finish) = pending.pop().expect("second command in session-a");
    assert!(
        commands
            .request_user_stop(&"session-b".parse().expect("session ID"), &stopped)
            .is_err()
    );
    commands
        .stop(&session, &stopped)
        .await
        .expect("stop command");
    let (owner, event) = widgets.try_recv().expect("remaining command");
    assert_eq!(owner, session.as_str());
    let FrontendEvent::Widget { item, .. } = event else {
        panic!("one command remains")
    };
    assert_eq!(item.count, Some(1));
    for (session, id, finish) in pending {
        finish.notify_one();
        let (owner, noticed_id, notice) =
            tokio::time::timeout(std::time::Duration::from_secs(5), received.recv())
                .await
                .expect("completion notice")
                .expect("notice sender");
        assert_eq!(owner, session.as_str());
        assert_eq!(noticed_id, id);
        assert!(notice.contains("exited with code 3"));
        commands.shutdown(&session).await.expect("shutdown");
        let (owner, event) = widgets.try_recv().expect("last command removed");
        assert_eq!(owner, session.as_str());
        assert!(
            matches!(event, FrontendEvent::Widget { item, .. } if item.count == Some(0)),
            "finished commands stay in open details but clear the pill count"
        );
        assert!(
            !commands
                .listeners
                .lock()
                .expect("listeners")
                .contains_key(&session)
        );
    }
    assert!(received.try_recv().is_err());
}

#[tokio::test]
async fn poll_collects_a_finished_result_before_the_task_returns() {
    let commands = BackgroundCommands::default();
    let session: SessionId = "session".parse().expect("session ID");
    let finish = Arc::new(Notify::new());
    let task_finish = Arc::clone(&finish);
    commands.entries.lock().expect("entries").insert(
        "command".into(),
        Entry {
            owner: session.share(),
            started: std::time::Instant::now(),
            state: Arc::new(AtomicU8::new(FINISHED)),
            output: Arc::default(),
            task: tokio::spawn(async move {
                task_finish.notified().await;
                Ok(command_output())
            }),
        },
    );
    let mut polling = Box::pin(commands.poll(&session, "command"));
    tokio::select! {
        biased;
        _ = &mut polling => panic!("must await the finished result"),
        () = tokio::task::yield_now() => {}
    }
    finish.notify_one();
    let output = polling.await.expect("finished result");
    assert_eq!(output.status, BackgroundCommandStatus::Exited);
    assert_eq!(output.stdout, "finished");
}

#[tokio::test]
async fn session_shutdown_cancels_its_commands() {
    let started = Arc::new(Notify::new());
    let cancelled = Arc::new(AtomicBool::new(false));
    let commands = BackgroundCommands::default();
    let _command = commands
        .start(
            &"session-a".parse().expect("session ID"),
            Arc::new(PendingBackend {
                started: Arc::clone(&started),
                cancelled: Arc::clone(&cancelled),
                finish: None,
            }),
            "command".into(),
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
            CommandOutputSink::default(),
        )
        .expect("start");
    started.notified().await;

    commands
        .shutdown(&"session-a".parse().expect("session ID"))
        .await
        .expect("shutdown");

    assert!(cancelled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn cancelling_the_initial_wait_stops_the_undelivered_command() {
    let started = Arc::new(Notify::new());
    let cancelled = Arc::new(AtomicBool::new(false));
    let commands = BackgroundCommands::default();
    let session: SessionId = "session".parse().expect("session ID");
    let command = commands
        .start(
            &session,
            Arc::new(PendingBackend {
                started: Arc::clone(&started),
                cancelled: Arc::clone(&cancelled),
                finish: None,
            }),
            "command".into(),
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
            CommandOutputSink::default(),
        )
        .expect("start");
    let mut waiting = Box::pin(command.wait(&session, std::time::Duration::from_secs(60)));
    tokio::select! {
        _ = &mut waiting => panic!("command must still be running"),
        () = started.notified() => {}
    }
    drop(waiting);
    assert!(!commands.has_owner(&session).expect("no orphan command"));
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !cancelled.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("command cancelled");
}
