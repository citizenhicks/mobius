use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::Notify;
use tokio::sync::mpsc;

use super::*;

const SCHEDULER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
enum SchedulerEvent {
    Started(String),
    Finished(String),
}

#[derive(Deserialize)]
struct ScheduledCall {
    id: String,
    #[serde(default)]
    after: Vec<String>,
}

#[derive(Clone)]
struct SchedulerHarness {
    releases: Arc<BTreeMap<String, Arc<Notify>>>,
    completed: Arc<Mutex<BTreeSet<String>>>,
    events: mpsc::UnboundedSender<SchedulerEvent>,
}

impl SchedulerHarness {
    fn new(call_ids: &[&str]) -> (Self, mpsc::UnboundedReceiver<SchedulerEvent>) {
        let (events, receiver) = mpsc::unbounded_channel();
        let releases = call_ids
            .iter()
            .map(|call_id| ((*call_id).into(), Arc::new(Notify::new())))
            .collect();
        (
            Self {
                releases: Arc::new(releases),
                completed: Arc::new(Mutex::new(BTreeSet::new())),
                events,
            },
            receiver,
        )
    }

    fn tool(&self, name: &'static str, execution_mode: ExecutionMode) -> Arc<dyn Tool> {
        Arc::new(SchedulerTool {
            name,
            execution_mode,
            releases: Arc::clone(&self.releases),
            completed: Arc::clone(&self.completed),
            events: self.events.clone(),
        })
    }

    fn release(&self, call_id: &str) {
        self.releases
            .get(call_id)
            .expect("scheduled call release")
            .notify_one();
    }
}

struct SchedulerTool {
    name: &'static str,
    execution_mode: ExecutionMode,
    releases: Arc<BTreeMap<String, Arc<Notify>>>,
    completed: Arc<Mutex<BTreeSet<String>>>,
    events: mpsc::UnboundedSender<SchedulerEvent>,
}

impl Tool for SchedulerTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.execution_mode
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<String>> {
        let arguments: ScheduledCall =
            serde_json::from_value(arguments).expect("scheduled call arguments");
        let release = Arc::clone(
            self.releases
                .get(&arguments.id)
                .expect("scheduled call release"),
        );
        let completed = Arc::clone(&self.completed);
        let events = self.events.clone();
        Box::pin(async move {
            let missing = {
                let completed = completed.lock().expect("scheduler completion state");
                arguments
                    .after
                    .iter()
                    .find(|call_id| !completed.contains(*call_id))
                    .cloned()
            };
            if let Some(call_id) = missing {
                return Err(Error::Tool(format!(
                    "scheduled call `{}` started before `{call_id}` completed",
                    arguments.id
                )));
            }
            events
                .send(SchedulerEvent::Started(arguments.id.clone()))
                .expect("record scheduled call start");
            release.notified().await;
            completed
                .lock()
                .expect("scheduler completion state")
                .insert(arguments.id.clone());
            events
                .send(SchedulerEvent::Finished(arguments.id.clone()))
                .expect("record scheduled call completion");
            Ok(arguments.id)
        })
    }
}

fn scheduled_call(call_id: &str, name: &str, after: &[&str]) -> ToolCall {
    ToolCall {
        call_id: call_id.into(),
        name: name.into(),
        arguments: serde_json::json!({"id": call_id, "after": after}),
    }
}

fn spawn_test_batch(
    mut catalog: Catalog,
    calls: Vec<ToolCall>,
    permissions: SandboxPermissions,
) -> tokio::task::JoinHandle<Vec<ToolResult>> {
    let sandbox = test_sandbox();
    let calls = finalize_and_bind(&mut catalog, &calls);
    tokio::spawn(
        async move { execute_batch(&catalog, &calls, sandbox, &permissions, "turn").await },
    )
}

async fn next_scheduler_event(
    events: &mut mpsc::UnboundedReceiver<SchedulerEvent>,
) -> SchedulerEvent {
    tokio::time::timeout(SCHEDULER_TIMEOUT, events.recv())
        .await
        .expect("scheduler event timeout")
        .expect("scheduler event channel")
}

async fn started_call(events: &mut mpsc::UnboundedReceiver<SchedulerEvent>) -> String {
    match next_scheduler_event(events).await {
        SchedulerEvent::Started(call_id) => call_id,
        event => panic!("expected scheduled call start, got {event:?}"),
    }
}

async fn finished_call(events: &mut mpsc::UnboundedReceiver<SchedulerEvent>) -> String {
    match next_scheduler_event(events).await {
        SchedulerEvent::Finished(call_id) => call_id,
        event => panic!("expected scheduled call completion, got {event:?}"),
    }
}

async fn batch_results(execution: tokio::task::JoinHandle<Vec<ToolResult>>) -> Vec<ToolResult> {
    tokio::time::timeout(SCHEDULER_TIMEOUT, execution)
        .await
        .expect("tool batch timeout")
        .expect("tool batch task")
}

#[tokio::test]
async fn consecutive_parallel_calls_overlap() {
    let (harness, mut events) = SchedulerHarness::new(&["p1", "p2"]);
    let mut catalog = Catalog::default();
    catalog
        .register(harness.tool("parallel", ExecutionMode::Parallel))
        .expect("register parallel tool");
    let execution = spawn_test_batch(
        catalog,
        vec![
            scheduled_call("p1", "parallel", &[]),
            scheduled_call("p2", "parallel", &[]),
        ],
        test_permissions(&[]),
    );

    let started = BTreeSet::from([
        started_call(&mut events).await,
        started_call(&mut events).await,
    ]);
    assert_eq!(started, BTreeSet::from(["p1".into(), "p2".into()]));
    harness.release("p1");
    harness.release("p2");

    let results = batch_results(execution).await;
    assert!(results.iter().all(|result| !result.is_error), "{results:?}");
}

#[tokio::test]
async fn exclusive_calls_separate_parallel_segments() {
    let (harness, mut events) = SchedulerHarness::new(&["p1", "p2", "e", "p3", "p4"]);
    let mut catalog = Catalog::default();
    catalog
        .register(harness.tool("parallel", ExecutionMode::Parallel))
        .expect("register parallel tool");
    catalog
        .register(harness.tool("exclusive", ExecutionMode::Exclusive))
        .expect("register exclusive tool");
    let execution = spawn_test_batch(
        catalog,
        vec![
            scheduled_call("p1", "parallel", &[]),
            scheduled_call("p2", "parallel", &[]),
            scheduled_call("e", "exclusive", &["p1", "p2"]),
            scheduled_call("p3", "parallel", &["e"]),
            scheduled_call("p4", "parallel", &["e"]),
        ],
        test_permissions(&[]),
    );

    assert_eq!(
        BTreeSet::from([
            started_call(&mut events).await,
            started_call(&mut events).await,
        ]),
        BTreeSet::from(["p1".into(), "p2".into()])
    );
    harness.release("p1");
    harness.release("p2");
    assert_eq!(
        BTreeSet::from([
            finished_call(&mut events).await,
            finished_call(&mut events).await,
        ]),
        BTreeSet::from(["p1".into(), "p2".into()])
    );
    assert_eq!(started_call(&mut events).await, "e");
    harness.release("e");
    assert_eq!(finished_call(&mut events).await, "e");
    assert_eq!(
        BTreeSet::from([
            started_call(&mut events).await,
            started_call(&mut events).await,
        ]),
        BTreeSet::from(["p3".into(), "p4".into()])
    );
    harness.release("p3");
    harness.release("p4");

    let results = batch_results(execution).await;
    assert!(results.iter().all(|result| !result.is_error), "{results:?}");
}

#[tokio::test]
async fn parallel_results_remain_in_model_call_order() {
    let (harness, mut events) = SchedulerHarness::new(&["p1", "p2"]);
    let mut catalog = Catalog::default();
    catalog
        .register(harness.tool("parallel", ExecutionMode::Parallel))
        .expect("register parallel tool");
    let execution = spawn_test_batch(
        catalog,
        vec![
            scheduled_call("p1", "parallel", &[]),
            scheduled_call("p2", "parallel", &[]),
        ],
        test_permissions(&[]),
    );

    let _ = started_call(&mut events).await;
    let _ = started_call(&mut events).await;
    harness.release("p2");
    assert_eq!(finished_call(&mut events).await, "p2");
    harness.release("p1");
    assert_eq!(finished_call(&mut events).await, "p1");

    assert_eq!(
        batch_results(execution)
            .await
            .into_iter()
            .map(|result| (result.call_id, result.output))
            .collect::<Vec<_>>(),
        vec![("p1".into(), "p1".into()), ("p2".into(), "p2".into())]
    );
}

#[tokio::test]
async fn consecutive_exclusive_calls_remain_sequential() {
    let (harness, mut events) = SchedulerHarness::new(&["e1", "e2"]);
    let mut catalog = Catalog::default();
    catalog
        .register(harness.tool("exclusive", ExecutionMode::Exclusive))
        .expect("register exclusive tool");
    let execution = spawn_test_batch(
        catalog,
        vec![
            scheduled_call("e1", "exclusive", &[]),
            scheduled_call("e2", "exclusive", &["e1"]),
        ],
        test_permissions(&[]),
    );

    assert_eq!(started_call(&mut events).await, "e1");
    harness.release("e1");
    assert_eq!(finished_call(&mut events).await, "e1");
    assert_eq!(started_call(&mut events).await, "e2");
    harness.release("e2");

    let results = batch_results(execution).await;
    assert!(results.iter().all(|result| !result.is_error), "{results:?}");
}

#[test]
fn unknown_tools_are_rejected_before_scheduling() {
    let (harness, _events) = SchedulerHarness::new(&["p1"]);
    let mut catalog = Catalog::default();
    catalog
        .register(harness.tool("parallel", ExecutionMode::Parallel))
        .expect("register parallel tool");
    catalog.finalize().expect("finalize catalog");

    assert_eq!(
        catalog
            .bind_call(
                scheduled_call("missing-call", "missing", &[]),
                &BTreeSet::new(),
                &BTreeSet::new(),
            )
            .expect_err("unknown tool must not bind")
            .to_string(),
        "tool error: unknown tool `missing`"
    );
}
