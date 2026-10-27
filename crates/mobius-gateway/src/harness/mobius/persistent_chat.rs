//! Main-conversation instructions and owner-scoped routine management.

use mobius::backend::model::ToolDefinition;
use mobius::middleware::manifest::MiddlewareManifest;
use mobius::middleware::tools::{Catalog, ExecutionMode, Tool, ToolContext};
use mobius::middleware::{Middleware, PreToolUseContext, PromptSection, RuntimeContext};
use mobius::protocol::MessageAuthor;
use mobius::{BoxFuture, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    label: String,
    description: String,
    prompt: String,
    list_routines: String,
    schedule_routine: String,
    routine_command: String,
    emit_hook: String,
    list_subscriptions: String,
    set_subscription: String,
    schedule_at: String,
    schedule_interval: String,
    schedule_ends_at: String,
}
struct Text {
    label: String,
    description: String,
    prompt: String,
    tools: [Arc<ToolDefinition>; 6],
}
static TEXT: std::sync::LazyLock<Text> = std::sync::LazyLock::new(|| {
    let mut definition: Definition = mobius::config::embedded(include_str!("persistent_chat.toml"));
    let tools = ACTIONS.map(|action| Arc::new(action.definition(&mut definition)));
    Text {
        label: definition.label,
        description: definition.description,
        prompt: definition.prompt,
        tools,
    }
});
pub(crate) static MANIFEST: std::sync::LazyLock<MiddlewareManifest> =
    std::sync::LazyLock::new(|| MiddlewareManifest {
        id: "persistent_chat",
        label: &TEXT.label,
        description: &TEXT.description,
        required: true,
        default_enabled: false,
        settings: &[],
        requires: &[],
    });

pub(crate) struct PersistentChat {
    bot_id: Arc<str>,
    host: crate::host::HostAccess,
}
impl PersistentChat {
    pub(crate) fn new(bot_id: Arc<str>, host: crate::host::HostAccess) -> Self {
        Self { bot_id, host }
    }
}
impl Middleware for PersistentChat {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }
    fn main_agent_only(&self) -> bool {
        true
    }
    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        if !crate::bots::is_persistent(&runtime.session_id, &self.bot_id)
            || !matches!(runtime.role, ::mobius::agent::AgentRole::Main)
            || *runtime.session_context.owner_id != *self.bot_id
            || runtime.session_context.workspace_id.is_some()
        {
            return Err(mobius::Error::Config(
                "Persistent Chat can only be installed in its Bot's main conversation".into(),
            ));
        }
        for (action, definition) in ACTIONS.into_iter().zip(&TEXT.tools) {
            catalog.register(Arc::new(RoutineTool {
                action,
                definition: Arc::clone(definition),
                bot_id: Arc::clone(&self.bot_id),
                host: Arc::clone(&self.host),
            }))?;
        }
        Ok(())
    }
    fn render(
        &self,
        event: &mobius::protocol::EventMsg,
        _: &str,
    ) -> Option<mobius::protocol::FrontendBlock> {
        ACTIONS.into_iter().find_map(|action| {
            mobius::middleware::tools::render_named_tool_event(event, action.name())
        })
    }

    fn prompt_section(&self, _: &RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(Some(PromptSection::new(&TEXT.prompt)))
    }
    fn pre_tool_use<'a>(
        &'a self,
        context: &'a mut PreToolUseContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if !matches!(context.turn.author, MessageAuthor::User) && !context.call_is_read_only() {
                context.deny("Source reports can be inspected and summarized; only a user turn can authorize new actions")?;
            }
            Ok(())
        })
    }
}

#[derive(Clone, Copy)]
enum Action {
    List,
    Create,
    Command,
    Reporting,
    Subscribe,
    Emit,
}
impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::List => "list_routines",
            Self::Create => "schedule_routine",
            Self::Command => "routine_command",
            Self::Reporting => "list_subscriptions",
            Self::Subscribe => "set_subscription",
            Self::Emit => "emit_hook",
        }
    }

    fn definition(self, text: &mut Definition) -> ToolDefinition {
        let (description, properties, required) = match self {
            Self::List => (std::mem::take(&mut text.list_routines), json!({}), vec![]),
            Self::Create => (
                std::mem::take(&mut text.schedule_routine),
                json!({"workspace":{"type":"string"},"instructions":{"type":"string"},"bindings":{"type":"array","items":{"$ref":"#/$defs/routine_binding"}}}),
                vec!["instructions", "bindings"],
            ),
            Self::Command => (
                std::mem::take(&mut text.routine_command),
                json!({"command":{"$ref":"#/$defs/routine_command"}}),
                vec!["command"],
            ),
            Self::Reporting => (
                std::mem::take(&mut text.list_subscriptions),
                json!({}),
                vec![],
            ),
            Self::Subscribe => (
                std::mem::take(&mut text.set_subscription),
                json!({"binding":{"$ref":"#/$defs/bot_binding"},"enabled":{"type":"boolean"}}),
                vec!["binding", "enabled"],
            ),
            Self::Emit => (
                std::mem::take(&mut text.emit_hook),
                json!({"name":{"type":"string"},"data":{}}),
                vec!["name", "data"],
            ),
        };
        let mut parameters = json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
        if matches!(self, Self::Create | Self::Command | Self::Subscribe)
            && let Some(parameters) = parameters.as_object_mut()
        {
            parameters.insert("$defs".into(), hook_definitions(text));
        }
        ToolDefinition {
            name: self.name().into(),
            description,
            parameters,
        }
    }
}
const ACTIONS: [Action; 6] = [
    Action::List,
    Action::Create,
    Action::Command,
    Action::Reporting,
    Action::Subscribe,
    Action::Emit,
];
struct RoutineTool {
    action: Action,
    definition: Arc<ToolDefinition>,
    bot_id: Arc<str>,
    host: crate::host::HostAccess,
}

impl Tool for RoutineTool {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &self.definition
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Exclusive
    }
    fn read_only(&self) -> bool {
        matches!(self.action, Action::List | Action::Reporting)
    }
    fn call(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<mobius::protocol::ToolResponse>> {
        Box::pin(async move {
            if !self.read_only() && !matches!(*context.author, MessageAuthor::User) {
                return Err(mobius::Error::Tool(
                    "Bot management requires a user turn or an exact saved hook action".into(),
                ));
            }
            let host = (self.host)().map_err(tool_error)?;
            let result = match self.action {
                Action::List => {
                    let _: Empty = serde_json::from_value(arguments)?;
                    host.bot_routine_snapshot(&self.bot_id)
                        .await
                        .map_err(rejected)?
                }
                Action::Reporting => {
                    let _: Empty = serde_json::from_value(arguments)?;
                    host.bot_reporting_snapshot(&self.bot_id)
                        .await
                        .map_err(rejected)?
                }
                Action::Create => {
                    let args: CreateArgs = serde_json::from_value(arguments)?;
                    let definition = crate::wire::RoutineDefinition {
                        workspace: host
                            .routine_workspace(&self.bot_id, args.workspace.as_deref())
                            .await
                            .map_err(rejected)?,
                        instructions: args.instructions,
                        bindings: args.bindings,
                    };
                    serde_json::to_value(
                        host.create_routine(&self.bot_id, &definition, None)
                            .await
                            .map_err(rejected)?,
                    )?
                }
                Action::Command => {
                    let args: CommandArgs = serde_json::from_value(arguments)?;
                    host.execute_routine_command(
                        &args.command,
                        Some(&self.bot_id),
                        None,
                        &context.call_id,
                    )
                    .await
                    .map_err(rejected)?;
                    json!({"accepted":context.call_id})
                }
                Action::Subscribe => {
                    let args: SubscriptionArgs = serde_json::from_value(arguments)?;
                    host.set_bot_subscription(crate::wire::BotSubscription {
                        bot_id: Arc::clone(&self.bot_id),
                        binding: args.binding,
                        enabled: args.enabled,
                    })
                    .await
                    .map_err(rejected)?;
                    json!({"saved":true})
                }
                Action::Emit => {
                    let args: EmitArgs = serde_json::from_value(arguments)?;
                    serde_json::to_value(
                        host.emit_bot_hook(&self.bot_id, &args.name, args.data, &context.call_id)
                            .await
                            .map_err(rejected)?,
                    )?
                }
            };
            Ok(serde_json::to_string(&result)?.into())
        })
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateArgs {
    workspace: Option<std::path::PathBuf>,
    instructions: String,
    bindings: Vec<crate::wire::RoutineBinding>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandArgs {
    command: crate::wire::RoutineCommand,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubscriptionArgs {
    binding: crate::wire::HookBinding<crate::wire::BotAction>,
    enabled: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmitArgs {
    name: String,
    data: Value,
}
fn rejected(crate::host::Rejection { message, .. }: crate::host::Rejection) -> mobius::Error {
    mobius::Error::Tool(message.to_string())
}
fn tool_error(error: impl std::fmt::Display) -> mobius::Error {
    mobius::Error::Tool(error.to_string())
}

fn hook_definitions(text: &Definition) -> Value {
    json!({
        "schedule":{"oneOf":[
            {"type":"object","properties":{"kind":{"const":"once"},"at":{"type":"integer","minimum":1,"maximum":crate::bots::MAX_SCHEDULE_TIMESTAMP,"description":text.schedule_at}},"required":["kind","at"],"additionalProperties":false},
            {"type":"object","properties":{"kind":{"const":"interval"},"every_seconds":{"type":"integer","minimum":60,"description":text.schedule_interval}},"required":["kind","every_seconds"],"additionalProperties":false},
            {"type":"object","properties":{"kind":{"const":"cron"},"expression":{"type":"string"},"time_zone":{"type":"string"}},"required":["kind","expression","time_zone"],"additionalProperties":false}
        ]},
        "source":{"oneOf":[
            source_schema("routine","routine_id"),source_schema("session","session_id"),source_schema("bot","bot_id"),source_schema("client","client_id"),
            {"type":"object","properties":{"type":{"const":"gateway"}},"required":["type"],"additionalProperties":false}
        ]},
        "selector":{"oneOf":[
            {"type":"object","properties":{"type":{"const":"schedule"},"schedule":{"$ref":"#/$defs/schedule"},"ends_at":{"type":["integer","null"],"minimum":1,"maximum":crate::bots::MAX_SCHEDULE_TIMESTAMP,"description":text.schedule_ends_at}},"required":["type","schedule"],"additionalProperties":false},
            {"type":"object","properties":{"type":{"const":"event"},"source":{"$ref":"#/$defs/source"},"kind":{"enum":["routine_created","routine_updated","routine_paused","routine_resumed","routine_deleted","run_started","run_finished","run_skipped","session_created","session_turn_started","session_turn_finished","session_approval","session_attention","session_deleted","session_owner_changed","client_connected","client_disconnected","custom_received"]},"routine_outcome":{"enum":["succeeded","failed","cancelled",null]},"session_outcome":{"enum":["completed","aborted","failed",null]},"custom_name":{"type":["string","null"]}},"required":["type","source","kind"],"additionalProperties":false}
        ]},
        "definition":{"type":"object","properties":{"workspace":{"type":"string"},"instructions":{"type":"string"},"bindings":{"type":"array","items":{"$ref":"#/$defs/routine_binding"}}},"required":["workspace","instructions","bindings"],"additionalProperties":false},
        "routine_action":{"oneOf":[
            unit_action("start"),unit_action("pause"),unit_action("resume"),unit_action("delete"),
            {"type":"object","properties":{"type":{"const":"stop"},"run_id":{"type":"string"}},"required":["type","run_id"],"additionalProperties":false},
            {"type":"object","properties":{"type":{"const":"update"},"definition":{"$ref":"#/$defs/definition"}},"required":["type","definition"],"additionalProperties":false}
        ]},
        "routine_command":{"type":"object","properties":{"routine_id":{"type":"string"},"action":{"$ref":"#/$defs/routine_action"}},"required":["routine_id","action"],"additionalProperties":false},
        "routine_binding":binding_schema(&json!({"$ref":"#/$defs/routine_action"})),
        "bot_binding":binding_schema(&json!({"oneOf":[
            {"type":"object","properties":{"type":{"const":"report"},"instruction":{"type":"string"}},"required":["type","instruction"],"additionalProperties":false},
            {"type":"object","properties":{"type":{"const":"routine"},"command":{"$ref":"#/$defs/routine_command"}},"required":["type","command"],"additionalProperties":false},
            {"type":"object","properties":{"type":{"const":"session"},"session_id":{"type":"string"},"op":{"$ref":"#/$defs/session_op"}},"required":["type","session_id","op"],"additionalProperties":false}
        ]})),
        "session_op":{"oneOf":[
            {"type":"object","properties":{"type":{"const":"interrupt"},"turn_id":{"type":"string"}},"required":["type","turn_id"],"additionalProperties":false},
            {"type":"object","properties":{"type":{"const":"message"},"message":{"type":"object","properties":{"author":{"type":"object","properties":{"type":{"const":"user"}},"required":["type"],"additionalProperties":false},"text":{"type":"string"},"attachments":{"type":"array","maxItems":0},"reply":{"type":"null"},"requested_delivery":{"enum":["queue","steer",null]},"target_turn_id":{"type":["string","null"]}},"required":["author","text","attachments","reply","requested_delivery","target_turn_id"],"additionalProperties":false}},"required":["type","message"],"additionalProperties":false}
        ]}
    })
}
fn source_schema(kind: &str, field: &str) -> Value {
    json!({"type":"object","properties":{"type":{"const":kind},field:{"type":"string"}},"required":["type",field],"additionalProperties":false})
}
fn unit_action(kind: &str) -> Value {
    json!({"type":"object","properties":{"type":{"const":kind}},"required":["type"],"additionalProperties":false})
}
fn binding_schema(action: &Value) -> Value {
    json!({"type":"object","properties":{"id":{"type":"string"},"on":{"$ref":"#/$defs/selector"},"action":action},"required":["id","on","action"],"additionalProperties":false})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_snapshot_actions_are_read_only() {
        let host: crate::host::HostAccess = Arc::new(|| Err(crate::Error::Config("unused".into())));
        let read_only = ACTIONS
            .into_iter()
            .zip(&TEXT.tools)
            .filter(|&(action, definition)| {
                RoutineTool {
                    action,
                    definition: Arc::clone(definition),
                    bot_id: "bot".into(),
                    host: Arc::clone(&host),
                }
                .read_only()
            })
            .map(|(action, _)| action.name())
            .collect::<Vec<_>>();
        assert_eq!(read_only, ["list_routines", "list_subscriptions"]);
    }
}
