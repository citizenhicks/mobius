//! Gateway-backed Bot identity and collaboration capabilities.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

use super::manifest::MiddlewareManifest;
use super::tools::{
    ApprovalRequirement, Catalog, ExecutionMode, Tool, ToolContext, render_tool_event,
};
use super::{Middleware, PromptSection, RuntimeContext, ToolExposureContext};
use crate::agent::AgentRole;
use crate::backend::model::ToolDefinition;
use crate::protocol::{EventMsg, FrontendBlock, MessageAuthor};
use crate::{BoxFuture, Result};

mod text {
    include!(concat!(env!("OUT_DIR"), "/src_middleware_bots_text.rs"));
}

const MAX_BOT_NAME_BYTES: usize = 128;
const MAX_BOT_DESCRIPTION_BYTES: usize = 2 * 1024;

/// Configuration and presentation metadata for durable Bots and their collaboration.
pub const MANIFEST: MiddlewareManifest = MiddlewareManifest {
    id: "bots",
    label: text::MANIFEST_LABEL,
    description: text::MANIFEST_DESCRIPTION,
    required: true,
    default_enabled: true,
    settings: &[],
};

/// Gateway operations needed by the framework-owned Bot tools.
pub trait BotsBackend: Send + Sync {
    /// Reports whether the Bot currently belongs to a swarm.
    fn active<'a>(&'a self, bot_id: &'a str) -> BoxFuture<'a, Result<bool>>;

    /// Resolves the stable scratchpad scope for the Bot's current swarm.
    fn scratchpad_scope<'a>(&'a self, bot_id: &'a str) -> BoxFuture<'a, Result<Option<String>>>;

    /// Creates one Bot and joins it to the leader's current Swarm.
    fn spawn_bot<'a>(
        &'a self,
        bot_id: &'a str,
        name: String,
        description: String,
    ) -> BoxFuture<'a, Result<String>>;

    /// Returns the caller's current roster as model-readable text.
    fn roster<'a>(&'a self, bot_id: &'a str) -> BoxFuture<'a, Result<String>>;

    /// Returns the caller's recent shared board as model-readable text.
    fn read<'a>(&'a self, bot_id: &'a str) -> BoxFuture<'a, Result<String>>;

    /// Reports whether an inbound peer message may receive another reply.
    fn can_reply<'a>(&'a self, bot_id: &'a str, message_id: &'a str)
    -> BoxFuture<'a, Result<bool>>;

    /// Durably posts a message and schedules any mentioned peers for delivery.
    fn post<'a>(
        &'a self,
        bot_id: &'a str,
        source_session_id: &'a str,
        text: String,
        in_reply_to_message_id: Option<String>,
    ) -> BoxFuture<'a, Result<String>>;
}

/// Installs Bot identity, discovery, board, and peer-message tools in a session.
pub struct Bots {
    backend: Arc<dyn BotsBackend>,
    bot_id: String,
    reply_to_message_id: Arc<Mutex<Option<String>>>,
}

impl Bots {
    /// Creates Bot middleware backed by its owning gateway.
    #[must_use]
    pub fn new(backend: Arc<dyn BotsBackend>, bot_id: impl Into<String>) -> Self {
        Self {
            backend,
            bot_id: bot_id.into(),
            reply_to_message_id: Arc::new(Mutex::new(None)),
        }
    }
}

impl Middleware for Bots {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        if !matches!(runtime.role, AgentRole::Main) {
            return Ok(());
        }
        let scope = ToolScope {
            backend: Arc::clone(&self.backend),
            bot_id: self.bot_id.clone(),
            session_id: runtime.session_id.clone(),
            reply_to_message_id: Arc::clone(&self.reply_to_message_id),
        };
        catalog.register(Arc::new(SwarmRoster(scope.clone())))?;
        catalog.register(Arc::new(SwarmRead(scope.clone())))?;
        catalog.register(Arc::new(SwarmSpawnBot(scope.clone())))?;
        catalog.register(Arc::new(SwarmPost(scope)))
    }

    fn prompt_section(&self, runtime: &RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(matches!(runtime.role, AgentRole::Main).then(|| PromptSection::new(text::PROMPT_MAIN)))
    }

    fn tool_exposure<'a>(
        &'a self,
        context: &'a mut ToolExposureContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let peer_message_id =
                context
                    .latest_message()
                    .and_then(|message| match message.author {
                        MessageAuthor::Peer { message_id, .. } => Some(message_id),
                        MessageAuthor::User => None,
                    });
            *self.reply_to_message_id.lock().await = peer_message_id.clone();
            if !self.backend.active(&self.bot_id).await? {
                context.hide(&[
                    "swarm_roster",
                    "swarm_read",
                    "swarm_spawn_bot",
                    "swarm_post",
                ]);
            } else if let Some(message_id) = peer_message_id
                && !self.backend.can_reply(&self.bot_id, &message_id).await?
            {
                context.hide(&["swarm_post"]);
            }
            Ok(())
        })
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        render_tool_event(
            event,
            |name| {
                matches!(
                    name,
                    "swarm_roster" | "swarm_read" | "swarm_spawn_bot" | "swarm_post"
                )
            },
            |name, arguments| super::tools::ToolHeading {
                title: match name {
                    "swarm_roster" => "Swarm roster",
                    "swarm_read" => "Read swarm board",
                    "swarm_spawn_bot" => "Spawn Swarm Bot",
                    "swarm_post" => "Post to swarm",
                    _ => unreachable!("tool predicate excludes other names"),
                }
                .into(),
                detail: arguments
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
            },
        )
    }
}

#[derive(Clone)]
struct ToolScope {
    backend: Arc<dyn BotsBackend>,
    bot_id: String,
    session_id: String,
    reply_to_message_id: Arc<Mutex<Option<String>>>,
}

struct SwarmRoster(ToolScope);

impl Tool for SwarmRoster {
    fn definition(&self) -> ToolDefinition {
        no_arguments_definition("swarm_roster", text::TOOL_ROSTER_DESCRIPTION)
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            require_no_arguments(arguments)?;
            self.0.backend.roster(&self.0.bot_id).await
        })
    }
}

struct SwarmRead(ToolScope);

impl Tool for SwarmRead {
    fn definition(&self) -> ToolDefinition {
        no_arguments_definition("swarm_read", text::TOOL_READ_DESCRIPTION)
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            require_no_arguments(arguments)?;
            self.0.backend.read(&self.0.bot_id).await
        })
    }
}

struct SwarmPost(ToolScope);

struct SwarmSpawnBot(ToolScope);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnBotArgs {
    name: String,
    description: String,
}

impl Tool for SwarmSpawnBot {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "swarm_spawn_bot".into(),
            description: text::TOOL_SPAWN_BOT_DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": text::TOOL_SPAWN_BOT_PARAMETER_NAME_DESCRIPTION,
                        "maxLength": MAX_BOT_NAME_BYTES
                    },
                    "description": {
                        "type": "string",
                        "description": text::TOOL_SPAWN_BOT_PARAMETER_DESCRIPTION_DESCRIPTION,
                        "maxLength": MAX_BOT_DESCRIPTION_BYTES
                    }
                },
                "required": ["name", "description"],
                "additionalProperties": false
            }),
        }
    }

    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Always
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let arguments: SpawnBotArgs = serde_json::from_value(arguments)?;
            let name = bounded_field(arguments.name, "Bot name", MAX_BOT_NAME_BYTES)?;
            let description = bounded_field(
                arguments.description,
                "Bot description",
                MAX_BOT_DESCRIPTION_BYTES,
            )?;
            self.0
                .backend
                .spawn_bot(&self.0.bot_id, name, description)
                .await
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PostArgs {
    text: String,
}

impl Tool for SwarmPost {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "swarm_post".into(),
            description: text::TOOL_POST_DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
                "additionalProperties": false
            }),
        }
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let arguments: PostArgs = serde_json::from_value(arguments)?;
            let in_reply_to_message_id = self.0.reply_to_message_id.lock().await.clone();
            self.0
                .backend
                .post(
                    &self.0.bot_id,
                    &self.0.session_id,
                    arguments.text,
                    in_reply_to_message_id,
                )
                .await
        })
    }
}

fn no_arguments_definition(name: &str, description: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    }
}

fn require_no_arguments(arguments: Value) -> Result<()> {
    serde_json::from_value::<NoArguments>(arguments)?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

fn bounded_field(value: String, label: &str, max_bytes: usize) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > max_bytes {
        return Err(crate::Error::Tool(format!(
            "{label} must be 1–{max_bytes} UTF-8 bytes"
        )));
    }
    Ok(value.into())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Mutex as StdMutex;

    use super::*;

    struct Membership {
        active: bool,
        can_reply: bool,
    }

    impl BotsBackend for Membership {
        fn active<'a>(&'a self, _bot_id: &'a str) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async move { Ok(self.active) })
        }

        fn scratchpad_scope<'a>(
            &'a self,
            _bot_id: &'a str,
        ) -> BoxFuture<'a, Result<Option<String>>> {
            Box::pin(async move { Ok(self.active.then(|| "swarm".into())) })
        }

        fn spawn_bot<'a>(
            &'a self,
            _bot_id: &'a str,
            _name: String,
            _description: String,
        ) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { unreachable!() })
        }

        fn roster<'a>(&'a self, _bot_id: &'a str) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { unreachable!() })
        }

        fn read<'a>(&'a self, _bot_id: &'a str) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { unreachable!() })
        }

        fn can_reply<'a>(
            &'a self,
            _bot_id: &'a str,
            _message_id: &'a str,
        ) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async move { Ok(self.can_reply) })
        }

        fn post<'a>(
            &'a self,
            _bot_id: &'a str,
            _source_session_id: &'a str,
            _text: String,
            _in_reply_to_message_id: Option<String>,
        ) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { unreachable!() })
        }
    }

    struct SpawnBackend {
        calls: StdMutex<Vec<(String, String, String)>>,
        error: bool,
    }

    impl BotsBackend for SpawnBackend {
        fn active<'a>(&'a self, _bot_id: &'a str) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async { Ok(true) })
        }

        fn scratchpad_scope<'a>(
            &'a self,
            _bot_id: &'a str,
        ) -> BoxFuture<'a, Result<Option<String>>> {
            Box::pin(async { unreachable!() })
        }

        fn spawn_bot<'a>(
            &'a self,
            bot_id: &'a str,
            name: String,
            description: String,
        ) -> BoxFuture<'a, Result<String>> {
            self.calls
                .lock()
                .expect("spawn calls")
                .push((bot_id.into(), name, description));
            let error = self.error;
            Box::pin(async move {
                if error {
                    Err(crate::Error::Tool("spawn rejected".into()))
                } else {
                    Ok("spawned-bot".into())
                }
            })
        }

        fn roster<'a>(&'a self, _bot_id: &'a str) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { unreachable!() })
        }

        fn read<'a>(&'a self, _bot_id: &'a str) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { unreachable!() })
        }

        fn can_reply<'a>(
            &'a self,
            _bot_id: &'a str,
            _message_id: &'a str,
        ) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async { unreachable!() })
        }

        fn post<'a>(
            &'a self,
            _bot_id: &'a str,
            _source_session_id: &'a str,
            _text: String,
            _in_reply_to_message_id: Option<String>,
        ) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { unreachable!() })
        }
    }

    fn spawn_tool(backend: Arc<dyn BotsBackend>) -> SwarmSpawnBot {
        SwarmSpawnBot(ToolScope {
            backend,
            bot_id: "leader-bot".into(),
            session_id: "chat".into(),
            reply_to_message_id: Arc::new(Mutex::new(None)),
        })
    }

    fn tool_context() -> ToolContext {
        use crate::backend::sandbox::{
            ApprovalPolicy, NetworkAccess, Sandbox, SandboxMode, SandboxPermissions,
        };

        ToolContext {
            sandbox: Arc::new(Sandbox::new(
                Arc::new(
                    crate::backend::sandbox::local::LocalSandbox::new(".").expect("local sandbox"),
                ),
                ApprovalPolicy::Ask,
            )),
            permissions: SandboxPermissions::restore(
                "chat",
                SandboxMode::WorkspaceWrite,
                NetworkAccess::Denied,
                ["call".into()],
            )
            .for_call("call"),
            turn_id: "turn".into(),
        }
    }

    #[test]
    fn post_tool_uses_text_as_its_payload_name() {
        let tool = SwarmPost(ToolScope {
            backend: Arc::new(Membership {
                active: true,
                can_reply: true,
            }),
            bot_id: "reviewer".into(),
            session_id: "chat".into(),
            reply_to_message_id: Arc::new(Mutex::new(None)),
        });

        assert_eq!(
            tool.definition().parameters,
            serde_json::json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn spawn_tool_has_a_bounded_approval_required_schema() {
        let tool = spawn_tool(Arc::new(SpawnBackend {
            calls: StdMutex::new(Vec::new()),
            error: false,
        }));
        let definition = tool.definition();

        assert_eq!(tool.approval(), ApprovalRequirement::Always);
        assert_eq!(definition.name, "swarm_spawn_bot");
        assert_eq!(
            definition.parameters["required"],
            serde_json::json!(["name", "description"])
        );
        assert_eq!(
            definition.parameters["properties"]["name"]["maxLength"],
            MAX_BOT_NAME_BYTES
        );
        assert_eq!(
            definition.parameters["properties"]["description"]["maxLength"],
            MAX_BOT_DESCRIPTION_BYTES
        );
        assert_eq!(definition.parameters["additionalProperties"], false);
        assert!(definition.parameters["properties"].get("handle").is_none());
        assert!(definition.parameters["properties"].get("config").is_none());
        assert!(definition.parameters["properties"].get("tint").is_none());
    }

    #[tokio::test]
    async fn spawn_tool_forwards_canonical_fields_and_backend_errors() {
        let backend = Arc::new(SpawnBackend {
            calls: StdMutex::new(Vec::new()),
            error: false,
        });
        let tool = spawn_tool(backend.clone());

        assert_eq!(
            tool.call(
                tool_context(),
                serde_json::json!({
                    "name": "  Release specialist  ",
                    "description": "  Coordinates release validation.  "
                }),
            )
            .await
            .expect("spawn Bot"),
            "spawned-bot"
        );
        assert_eq!(
            *backend.calls.lock().expect("spawn calls"),
            [(
                "leader-bot".into(),
                "Release specialist".into(),
                "Coordinates release validation.".into(),
            )]
        );

        let failing = spawn_tool(Arc::new(SpawnBackend {
            calls: StdMutex::new(Vec::new()),
            error: true,
        }));
        assert_eq!(
            failing
                .call(
                    tool_context(),
                    serde_json::json!({"name": "Worker", "description": "Review changes"}),
                )
                .await
                .expect_err("backend failure")
                .to_string(),
            "tool error: spawn rejected"
        );
        assert!(
            tool.call(
                tool_context(),
                serde_json::json!({"name": " ", "description": "Review changes"}),
            )
            .await
            .expect_err("blank name")
            .to_string()
            .contains("Bot name must be")
        );
    }

    #[tokio::test]
    async fn swarm_tools_exist_only_for_members() {
        let names = || {
            BTreeSet::from([
                "swarm_post".to_string(),
                "swarm_read".to_string(),
                "swarm_roster".to_string(),
                "swarm_spawn_bot".to_string(),
            ])
        };
        let hidden = Bots::new(
            Arc::new(Membership {
                active: false,
                can_reply: false,
            }),
            "reviewer",
        );
        let mut unavailable = names();
        hidden
            .tool_exposure(&mut ToolExposureContext {
                session_id: "chat",
                input: &[],
                available: &mut unavailable,
            })
            .await
            .expect("inactive membership");
        assert!(unavailable.is_empty());

        let active = Bots::new(
            Arc::new(Membership {
                active: true,
                can_reply: true,
            }),
            "reviewer",
        );
        let mut available = names();
        active
            .tool_exposure(&mut ToolExposureContext {
                session_id: "chat",
                input: &[],
                available: &mut available,
            })
            .await
            .expect("active membership");
        assert_eq!(available, names());

        let peer = crate::backend::model::message_input(&crate::protocol::MessageEvent {
            author: MessageAuthor::Peer {
                message_id: "message".into(),
                session_id: "peer".into(),
                handle: "worker".into(),
            },
            delivery: crate::protocol::MessageDelivery::Turn,
            text: "done".into(),
            attachments: Vec::new(),
            message_target: None,
        })
        .expect("peer message");
        let mut peer_available = names();
        active
            .tool_exposure(&mut ToolExposureContext {
                session_id: "chat",
                input: std::slice::from_ref(&peer),
                available: &mut peer_available,
            })
            .await
            .expect("peer turn");
        assert_eq!(peer_available, names());

        let bounded = Bots::new(
            Arc::new(Membership {
                active: true,
                can_reply: false,
            }),
            "reviewer",
        );
        let mut bounded_available = names();
        bounded
            .tool_exposure(&mut ToolExposureContext {
                session_id: "chat",
                input: std::slice::from_ref(&peer),
                available: &mut bounded_available,
            })
            .await
            .expect("bounded peer turn");
        assert_eq!(
            bounded_available,
            BTreeSet::from([
                "swarm_read".to_string(),
                "swarm_roster".to_string(),
                "swarm_spawn_bot".to_string(),
            ])
        );
    }
}
