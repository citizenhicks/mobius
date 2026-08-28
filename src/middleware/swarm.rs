//! Gateway-backed collaboration between ordinary agent sessions.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::manifest::MiddlewareManifest;
use super::tools::{Catalog, ExecutionMode, Tool, ToolContext, render_tool_event};
use super::{Middleware, PromptSection, RuntimeContext, ToolExposureContext};
use crate::agent::AgentRole;
use crate::backend::model::ToolDefinition;
use crate::protocol::{EventMsg, FrontendBlock};
use crate::{BoxFuture, Result};

mod text {
    include!(concat!(env!("OUT_DIR"), "/src_middleware_swarm_text.rs"));
}

/// Configuration and presentation metadata for peer-session collaboration.
pub const MANIFEST: MiddlewareManifest = MiddlewareManifest {
    id: "swarm",
    label: text::MANIFEST_LABEL,
    description: text::MANIFEST_DESCRIPTION,
    required: true,
    default_enabled: true,
    settings: &[],
};

/// Gateway operations needed by the framework-owned swarm tools.
pub trait SwarmBackend: Send + Sync {
    /// Reports whether the session currently belongs to a swarm.
    fn active<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<bool>>;

    /// Returns the caller's current roster as model-readable text.
    fn roster<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<String>>;

    /// Returns the caller's recent shared board as model-readable text.
    fn read<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<String>>;

    /// Durably posts a message and schedules any mentioned peers for delivery.
    fn post<'a>(&'a self, session_id: &'a str, message: String) -> BoxFuture<'a, Result<String>>;
}

/// Installs swarm discovery, board, and peer-message tools in an ordinary session.
pub struct Swarm {
    backend: Arc<dyn SwarmBackend>,
}

impl Swarm {
    /// Creates swarm middleware backed by its owning gateway.
    #[must_use]
    pub fn new(backend: Arc<dyn SwarmBackend>) -> Self {
        Self { backend }
    }
}

impl Middleware for Swarm {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        if !matches!(runtime.role, AgentRole::Main) {
            return Ok(());
        }
        let scope = ToolScope {
            backend: Arc::clone(&self.backend),
            session_id: runtime.session_id.clone(),
        };
        catalog.register(Arc::new(SwarmRoster(scope.clone())))?;
        catalog.register(Arc::new(SwarmRead(scope.clone())))?;
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
            if !self.backend.active(context.session_id).await? {
                context.hide(&["swarm_roster", "swarm_read", "swarm_post"]);
            } else if context.peer_input() {
                context.hide(&["swarm_post"]);
            }
            Ok(())
        })
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        render_tool_event(
            event,
            |name| matches!(name, "swarm_roster" | "swarm_read" | "swarm_post"),
            |name, arguments| super::tools::ToolHeading {
                title: match name {
                    "swarm_roster" => "Swarm roster",
                    "swarm_read" => "Read swarm board",
                    "swarm_post" => "Post to swarm",
                    _ => unreachable!("tool predicate excludes other names"),
                }
                .into(),
                detail: arguments
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
            },
        )
    }
}

#[derive(Clone)]
struct ToolScope {
    backend: Arc<dyn SwarmBackend>,
    session_id: String,
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
            self.0.backend.roster(&self.0.session_id).await
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
            self.0.backend.read(&self.0.session_id).await
        })
    }
}

struct SwarmPost(ToolScope);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PostArgs {
    message: String,
}

impl Tool for SwarmPost {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "swarm_post".into(),
            description: text::TOOL_POST_DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"message": {"type": "string"}},
                "required": ["message"],
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
            self.0
                .backend
                .post(&self.0.session_id, arguments.message)
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    struct Membership(bool);

    impl SwarmBackend for Membership {
        fn active<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async move { Ok(self.0) })
        }

        fn roster<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { unreachable!() })
        }

        fn read<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { unreachable!() })
        }

        fn post<'a>(
            &'a self,
            _session_id: &'a str,
            _message: String,
        ) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { unreachable!() })
        }
    }

    #[tokio::test]
    async fn swarm_tools_exist_only_for_members() {
        let names = || {
            BTreeSet::from([
                "swarm_post".to_string(),
                "swarm_read".to_string(),
                "swarm_roster".to_string(),
            ])
        };
        let hidden = Swarm::new(Arc::new(Membership(false)));
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

        let active = Swarm::new(Arc::new(Membership(true)));
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

        let peer = crate::backend::model::peer_message("message", "peer", "worker", "done");
        let mut peer_available = names();
        active
            .tool_exposure(&mut ToolExposureContext {
                session_id: "chat",
                input: &[peer],
                available: &mut peer_available,
            })
            .await
            .expect("peer turn");
        assert_eq!(
            peer_available,
            BTreeSet::from(["swarm_read".to_string(), "swarm_roster".to_string()])
        );
    }
}
