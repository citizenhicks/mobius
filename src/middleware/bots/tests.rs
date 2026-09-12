use super::*;
use std::borrow::Cow;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
use crate::backend::model::{
    Model, ModelEventSink, ModelOutput, ModelRequest, ModelRouter, user_message,
};
use crate::backend::sandbox::{
    ApprovalPolicy, NetworkAccess, Sandbox, SandboxMode, SandboxPermissions, local::LocalSandbox,
};

#[derive(Default)]
struct Backend {
    routines: AtomicUsize,
    chats: AtomicUsize,
    grouped: bool,
}

impl BotsBackend for Backend {
    fn create_routine<'a>(
        &'a self,
        bot_id: &'a str,
        workspace: &'a Path,
        instructions: String,
        schedule: Value,
        ends_at: Option<i64>,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            assert_eq!(bot_id, "self-bot");
            assert_eq!(workspace, Path::new("workspace"));
            assert_eq!(instructions, "Review the build");
            assert_eq!(
                schedule,
                serde_json::json!({"kind":"interval", "every_seconds":60})
            );
            assert_eq!(ends_at, Some(1000));
            self.routines.fetch_add(1, Ordering::Relaxed);
            Ok("routine created".into())
        })
    }

    fn chat_context<'a>(
        &'a self,
        bot_id: &'a str,
        session_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>>> {
        Box::pin(async move {
            assert_eq!(bot_id, "self-bot");
            assert_eq!(session_id, "session");
            self.chats.fetch_add(1, Ordering::Relaxed);
            Ok(self.grouped.then(|| "@peer: shared history".into()))
        })
    }
}

fn child_role() -> AgentRole {
    AgentRole::Subagent {
        parent_session_id: "parent".into(),
        parent_turn_id: "turn".into(),
    }
}

#[test]
fn routine_tool_and_prompt_require_enabled_main_agent() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let mut runtime = RuntimeContext {
        sender: crate::agent::test_sender(),
        checkpoints: Arc::new(
            SqliteCheckpoint::new(temporary.path().join("state.sqlite3")).expect("checkpoints"),
        ),
        session_id: "session".into(),
        model_route: "model".into(),
        model: "model".into(),
        approval_policy: ApprovalPolicy::Ask,
        session_context: Default::default(),
        metadata: Default::default(),
        role: AgentRole::Main,
        frontend: Arc::new(|_| Ok(())),
    };
    for (role, enabled, expected) in [
        (AgentRole::Main, false, false),
        (AgentRole::Main, true, true),
        (child_role(), false, false),
        (child_role(), true, false),
    ] {
        runtime.role = role;
        let mut middleware = Bots::new(Arc::new(Backend::default()), "self-bot");
        if enabled {
            middleware = middleware.with_routine_creation("workspace");
        }
        let mut catalog = Catalog::default();
        middleware
            .register(&mut catalog, &runtime)
            .expect("register");
        let definitions = catalog.registered_definitions();
        assert_eq!(definitions.len(), usize::from(expected));
        assert_eq!(
            middleware
                .prompt_section(&runtime)
                .expect("prompt")
                .is_some(),
            expected
        );
        if expected {
            assert_eq!(definitions[0].name, "create_routine");
            assert!(catalog.requires_approval("create_routine"));
        }
    }
}

#[tokio::test]
async fn routine_creation_always_requires_approval_and_cannot_target_another_bot() {
    let backend = Arc::new(Backend::default());
    let tool = CreateRoutine(RoutineScope {
        backend: backend.clone(),
        bot_id: "self-bot".into(),
        workspace: "workspace".into(),
    });
    assert_eq!(tool.approval(), ApprovalRequirement::Always);
    let sandbox = Arc::new(Sandbox::new(
        Arc::new(LocalSandbox::new(".").expect("sandbox")),
        ApprovalPolicy::Ask,
    ));
    let context = || {
        ToolContext::new(
            sandbox.clone(),
            SandboxPermissions::restore(
                "session",
                SandboxMode::WorkspaceWrite,
                NetworkAccess::Denied,
                ["call".into()],
            )
            .for_call("call"),
            "turn",
        )
    };
    let arguments = serde_json::json!({
        "instructions": "Review the build",
        "schedule": {"kind":"interval", "every_seconds":60},
        "ends_at": 1000,
    });
    let mut targeted = arguments.clone();
    targeted["bot_handle"] = "other-bot".into();
    assert!(tool.call(context(), targeted).await.is_err());
    assert_eq!(backend.routines.load(Ordering::Relaxed), 0);
    tool.call(context(), arguments)
        .await
        .expect("create own routine");
    assert_eq!(backend.routines.load(Ordering::Relaxed), 1);
}

struct NoModel;

impl Model for NoModel {
    fn respond<'a>(
        &'a self,
        _request: ModelRequest<'a>,
        _events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(async { unreachable!("context decoration does not call the model") })
    }
}

#[tokio::test]
async fn group_context_is_request_only_and_available_to_main_agents() {
    let router = ModelRouter::new("model", Arc::new(NoModel));
    let original = vec![user_message("hello")];
    for (role, grouped, expected) in [
        (AgentRole::Main, true, true),
        (AgentRole::Main, false, false),
        (child_role(), true, false),
    ] {
        let backend = Arc::new(Backend {
            grouped,
            ..Backend::default()
        });
        let middleware = Bots::new(backend.clone(), "self-bot");
        let mut context = ModelRequestContext {
            role: &role,
            model: &router,
            provider: "model",
            session_id: "session",
            turn_id: "turn",
            model_step: 0,
            input: Cow::Borrowed(&original),
        };
        middleware
            .model_request(&mut context)
            .await
            .expect("chat context");
        assert_eq!(
            backend.chats.load(Ordering::Relaxed),
            usize::from(role == AgentRole::Main)
        );
        assert_eq!(context.input().len(), 1 + usize::from(expected));
        assert_eq!(context.input()[0], original[0]);
        if expected {
            let chat = &context.input()[1];
            assert_eq!(
                crate::protocol::internal_message_kind(chat),
                Some("group_chat")
            );
            assert!(
                chat["content"][0]["text"]
                    .as_str()
                    .expect("text")
                    .contains("@peer: shared history")
            );
        } else {
            assert!(matches!(context.input, Cow::Borrowed(_)));
        }
    }
    assert_eq!(original, [user_message("hello")]);
}
