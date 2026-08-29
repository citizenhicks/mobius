//! Tool registry, dispatch, and minimal filesystem tools.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use diffy::Patch;
use futures_util::FutureExt;
use futures_util::future::join_all;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::manifest::MiddlewareManifest;
use super::{Middleware, PromptSection};
use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::backend::model::TOOLS_SEARCH_NAME;
use crate::backend::model::ToolCall;
use crate::backend::model::ToolDefinition;
use crate::backend::model::ToolLoad;
#[cfg(test)]
use crate::backend::sandbox::BackgroundCommandPoll;
use crate::backend::sandbox::Sandbox;
use crate::backend::sandbox::SandboxPermissions;
use crate::backend::sandbox::ToolPermissions;
use crate::preview_json;
use crate::protocol::EventMsg;
use crate::protocol::FrontendBlock;
use crate::protocol::FrontendBlockFormat;
use crate::protocol::FrontendBlockRole;
use crate::protocol::FrontendBlockState;
use crate::protocol::FrontendBlockUpdate;
use crate::protocol::FrontendContribution;
use crate::protocol::FrontendTone;
use crate::protocol::ToolLoadEvent;

mod text {
    include!(concat!(env!("OUT_DIR"), "/src_middleware_tools_text.rs"));
}

mod coding;
mod commands;
mod patch;

#[cfg(test)]
use coding::ApplyPatchArgs;
use coding::{ApplyPatch, ReadFile, WriteFile};
#[cfg(test)]
use commands::background_output;
use commands::{Bash, PollCommand, StartCommand, StopCommand};
#[cfg(test)]
use patch::{apply_patch_document, parse_patch_document, validate_patch_complexity};

const MAX_TOOL_OUTPUT_BYTES: usize = 40_000;
const MAX_TOOL_UI_BYTES: usize = 512;
const MAX_TOOL_UI_LINES: usize = 5;
const MAX_TOOL_NAME_BYTES: usize = 256;
const MAX_TOOL_SEARCH_QUERY_BYTES: usize = 512;
const MAX_TOOL_SEARCH_RESULTS: usize = 8;
const MAX_MUTATION_BYTES: usize = 40_000;
const MAX_COMMAND_BYTES: usize = 8_000;
const MAX_PATCH_MATCH_WORK: usize = 32 * 1024 * 1024;

/// Configuration and presentation metadata for workspace tools.
pub const MANIFEST: MiddlewareManifest = MiddlewareManifest {
    id: "tools",
    label: text::MANIFEST_LABEL,
    description: text::MANIFEST_DESCRIPTION,
    required: true,
    default_enabled: true,
    settings: &[],
};

/// Whether a tool can overlap other calls in its model-produced batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Parallel,
    Exclusive,
}

/// Whether a tool requires sandbox mutation approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalRequirement {
    Never,
    Always,
}

/// Whether a tool is initially visible to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExposure {
    /// Included in every model request.
    Direct,
    /// Discoverable through `tools_search` and callable only after materialization.
    Deferred,
    /// Registered for internal ownership but unavailable to the model.
    Hidden,
}

/// Dependencies available only to terminal tool handlers.
pub struct ToolContext {
    pub sandbox: Arc<Sandbox>,
    pub permissions: ToolPermissions,
    pub turn_id: String,
}

/// External identity used when a tool participates in extension hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookIdentity {
    /// Tool name exposed to hook matchers and payloads.
    pub name: &'static str,
    /// Matcher subjects checked in declaration order.
    pub subjects: &'static [&'static str],
}

/// A named tool Adapter registered by middleware.
pub trait Tool: Send + Sync {
    /// Returns the provider-facing tool schema.
    fn definition(&self) -> ToolDefinition;

    /// Declares how the tool is exposed to the model.
    fn exposure(&self) -> ToolExposure {
        ToolExposure::Deferred
    }

    /// Declares whether calls may overlap.
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Exclusive
    }

    /// Declares whether this tool requires sandbox mutation approval.
    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Never
    }

    /// Allows accepted model input to cancel this tool while it is waiting.
    fn cancel_on_input(&self) -> bool {
        false
    }

    /// Declares an external hook alias and matcher subjects.
    fn hook_identity(&self) -> Option<HookIdentity> {
        None
    }

    /// Maps provider-facing arguments into the extension hook payload.
    fn hook_input(&self, arguments: &Value) -> Value {
        arguments.clone()
    }

    /// Maps a hook rewrite back into provider-facing arguments.
    fn rewrite_hook_input(&self, input: Value) -> Result<Value> {
        object_hook_input(input)
    }

    /// Executes one validated provider call.
    fn call<'a>(&'a self, context: ToolContext, arguments: Value) -> BoxFuture<'a, Result<String>>;
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HookTool {
    pub(crate) name: String,
    pub(crate) input: Value,
    pub(crate) subjects: Vec<String>,
}

#[derive(Clone)]
struct RegisteredTool {
    definition: ToolDefinition,
    exposure: ToolExposure,
    execution_mode: ExecutionMode,
    approval: ApprovalRequirement,
    cancel_on_input: bool,
    handler: RegisteredHandler,
}

#[derive(Clone)]
enum RegisteredHandler {
    Tool(Arc<dyn Tool>),
    Search,
}

/// The validated tool registry built during agent creation.
#[derive(Clone, Default)]
pub struct Catalog {
    tools: BTreeMap<String, RegisteredTool>,
    registered_definitions: Arc<[ToolDefinition]>,
    direct_definitions: Arc<[ToolDefinition]>,
    deferred_definitions: Arc<[ToolDefinition]>,
    revision: String,
    finalized: bool,
}

/// One catalog snapshot resolved for a model boundary.
#[derive(Clone)]
pub(crate) struct PreparedToolSet {
    direct: Vec<ToolDefinition>,
    deferred: Vec<ToolDefinition>,
    available: BTreeSet<String>,
    searchable: BTreeSet<String>,
    materialized: BTreeSet<String>,
    catalog_revision: String,
}

#[derive(Default)]
pub(crate) struct ToolEffects {
    pub(crate) input: Vec<Value>,
    pub(crate) events: Vec<EventMsg>,
}

impl PreparedToolSet {
    pub(crate) fn direct(&self) -> &[ToolDefinition] {
        &self.direct
    }

    pub(crate) fn deferred(&self) -> &[ToolDefinition] {
        &self.deferred
    }

    pub(crate) fn materialized(&self) -> &BTreeSet<String> {
        &self.materialized
    }

    pub(crate) fn accept_materialized(
        &mut self,
        tools: &BTreeSet<String>,
        turn_id: &str,
        load_id: &str,
    ) -> Result<ToolEffects> {
        if !tools.is_subset(&self.searchable) {
            return Err(Error::Provider(
                "provider materialized a tool outside the searchable catalog".into(),
            ));
        }
        self.materialized.extend(tools.iter().cloned());
        materialization_effects(
            &self.catalog_revision,
            tools.iter().cloned(),
            turn_id,
            load_id,
        )
    }
}

impl Catalog {
    /// Registers one tool and rejects invalid definitions or duplicate names.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<()> {
        if self.finalized {
            return Err(Error::Config("tool catalog is already finalized".into()));
        }
        let definition = tool.definition();
        validate_definition(&definition)?;
        if definition.name == TOOLS_SEARCH_NAME {
            return Err(Error::Config(format!(
                "tool name `{TOOLS_SEARCH_NAME}` is reserved"
            )));
        }
        let name = definition.name.clone();
        let entry = RegisteredTool {
            definition,
            exposure: tool.exposure(),
            execution_mode: tool.execution_mode(),
            approval: tool.approval(),
            cancel_on_input: tool.cancel_on_input(),
            handler: RegisteredHandler::Tool(tool),
        };
        self.insert(name, entry)
    }

    /// Freezes the registry and installs `tools_search` when deferred tools exist.
    pub fn finalize(&mut self) -> Result<()> {
        if self.finalized {
            return Err(Error::Config("tool catalog is already finalized".into()));
        }
        if !self.deferred_definitions.is_empty() {
            let definition = tools_search_definition();
            let name = definition.name.clone();
            self.insert(
                name,
                RegisteredTool {
                    definition,
                    exposure: ToolExposure::Direct,
                    execution_mode: ExecutionMode::Exclusive,
                    approval: ApprovalRequirement::Never,
                    cancel_on_input: false,
                    handler: RegisteredHandler::Search,
                },
            )?;
        }
        self.revision = catalog_revision(&self.tools);
        self.finalized = true;
        Ok(())
    }

    fn insert(&mut self, name: String, entry: RegisteredTool) -> Result<()> {
        if self.tools.contains_key(&name) {
            return Err(Error::Duplicate(format!("tool `{name}`")));
        }
        self.tools.insert(name, entry);
        self.registered_definitions = self
            .tools
            .values()
            .map(|tool| tool.definition.clone())
            .collect::<Vec<_>>()
            .into();
        self.direct_definitions = self
            .tools
            .values()
            .filter(|tool| tool.exposure == ToolExposure::Direct)
            .map(|tool| tool.definition.clone())
            .collect::<Vec<_>>()
            .into();
        self.deferred_definitions = self
            .tools
            .values()
            .filter(|tool| tool.exposure == ToolExposure::Deferred)
            .map(|tool| tool.definition.clone())
            .collect::<Vec<_>>()
            .into();
        Ok(())
    }

    /// Returns all registered definitions in stable name order.
    #[must_use]
    pub fn registered_definitions(&self) -> Arc<[ToolDefinition]> {
        Arc::clone(&self.registered_definitions)
    }

    /// Returns definitions included in every model request in stable name order.
    #[must_use]
    pub fn direct_definitions(&self) -> Arc<[ToolDefinition]> {
        Arc::clone(&self.direct_definitions)
    }

    /// Returns discoverable definitions in stable name order.
    #[must_use]
    pub fn deferred_definitions(&self) -> Arc<[ToolDefinition]> {
        Arc::clone(&self.deferred_definitions)
    }

    /// Returns the stable schema and exposure fingerprint of this finalized catalog.
    pub fn revision(&self) -> Result<&str> {
        if self.finalized {
            Ok(&self.revision)
        } else {
            Err(Error::Config(
                "tool catalog must be finalized before reading its revision".into(),
            ))
        }
    }

    pub(crate) fn exposed_names(&self) -> BTreeSet<String> {
        self.direct_definitions
            .iter()
            .chain(self.deferred_definitions.iter())
            .map(|tool| tool.name.clone())
            .collect()
    }

    pub(crate) fn prepare(
        &self,
        input: &[Value],
        mut available: BTreeSet<String>,
    ) -> Result<PreparedToolSet> {
        let deferred = self
            .deferred_definitions
            .iter()
            .filter(|tool| available.contains(&tool.name))
            .cloned()
            .collect::<Vec<_>>();
        let searchable = deferred
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<BTreeSet<_>>();
        if searchable.is_empty() {
            available.remove(TOOLS_SEARCH_NAME);
        }
        let materialized = loaded_tools(input, self.revision()?, &searchable)?;
        let direct = self
            .direct_definitions
            .iter()
            .filter(|tool| available.contains(&tool.name))
            .cloned()
            .collect();
        Ok(PreparedToolSet {
            direct,
            deferred,
            available,
            searchable,
            materialized,
            catalog_revision: self.revision()?.into(),
        })
    }

    pub(crate) fn bind_prepared(
        &self,
        call: ToolCall,
        tools: &PreparedToolSet,
    ) -> Result<BoundToolCall> {
        if !tools.available.contains(&call.name) {
            return Err(Error::Tool(format!(
                "tool `{}` is unavailable for this model step",
                call.name
            )));
        }
        self.bind_call(call, &tools.materialized, &tools.searchable)
    }

    pub(crate) fn bind_live_batch(
        &self,
        calls: &[ToolCall],
        tools: &PreparedToolSet,
    ) -> (Vec<BoundToolCall>, Vec<ToolResult>) {
        let mut bound = Vec::with_capacity(calls.len());
        let mut rejected = Vec::new();
        for call in calls {
            match self.bind_prepared(call.clone(), tools) {
                Ok(call) => bound.push(call),
                Err(error) => rejected.push(ToolResult::error(call, error.to_string())),
            }
        }
        (bound, rejected)
    }

    /// Searches currently deferred tools using deterministic name-first ranking.
    pub fn search_deferred(
        &self,
        query: &str,
        searchable: &BTreeSet<String>,
    ) -> Result<Vec<ToolDefinition>> {
        let query = query.trim();
        if query.is_empty() {
            return Err(Error::Tool("tools_search query cannot be empty".into()));
        }
        if query.len() > MAX_TOOL_SEARCH_QUERY_BYTES {
            return Err(Error::Tool(format!(
                "tools_search query exceeds {MAX_TOOL_SEARCH_QUERY_BYTES} bytes"
            )));
        }
        let query_lower = query.to_lowercase();
        let mut matches = self
            .tools
            .values()
            .filter(|tool| {
                tool.exposure == ToolExposure::Deferred
                    && searchable.contains(&tool.definition.name)
            })
            .filter_map(|tool| {
                let name = tool.definition.name.to_lowercase();
                let rank = if name == query_lower {
                    0
                } else if name.contains(&query_lower) {
                    1
                } else if tool
                    .definition
                    .description
                    .to_lowercase()
                    .contains(&query_lower)
                {
                    2
                } else {
                    return None;
                };
                Some((rank, &tool.definition))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|(left_rank, left), (right_rank, right)| {
            left_rank
                .cmp(right_rank)
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(matches
            .into_iter()
            .take(MAX_TOOL_SEARCH_RESULTS)
            .map(|(_, definition)| definition.clone())
            .collect())
    }

    /// Validates a model-returned call against this catalog and its step materialization.
    pub fn bind_call(
        &self,
        call: ToolCall,
        materialized: &BTreeSet<String>,
        searchable: &BTreeSet<String>,
    ) -> Result<BoundToolCall> {
        if !self.finalized {
            return Err(Error::Config(
                "tool catalog must be finalized before binding calls".into(),
            ));
        }
        let Some(tool) = self.get(&call.name) else {
            return Err(Error::Tool(format!("unknown tool `{}`", call.name)));
        };
        let materialized = match tool.exposure {
            ToolExposure::Direct => false,
            ToolExposure::Deferred if !searchable.contains(&call.name) => {
                return Err(Error::Tool(format!(
                    "tool `{}` is not available for this model step",
                    call.name
                )));
            }
            ToolExposure::Deferred if materialized.contains(&call.name) => true,
            ToolExposure::Deferred => {
                return Err(Error::Tool(format!(
                    "tool `{}` was not materialized for this model step",
                    call.name
                )));
            }
            ToolExposure::Hidden => {
                return Err(Error::Tool(format!(
                    "tool `{}` is hidden from the model",
                    call.name
                )));
            }
        };
        let search_scope =
            matches!(&tool.handler, RegisteredHandler::Search).then(|| searchable.clone());
        Ok(BoundToolCall {
            call,
            materialized,
            search_scope,
        })
    }

    /// Returns whether the named tool requires approval.
    #[must_use]
    pub fn requires_approval(&self, name: &str) -> bool {
        self.tools
            .get(name)
            .is_some_and(|tool| tool.approval == ApprovalRequirement::Always)
    }

    pub(crate) fn cancels_on_input(&self, calls: &[ToolCall]) -> bool {
        !calls.is_empty()
            && calls.iter().all(|call| {
                self.tools
                    .get(&call.name)
                    .is_some_and(|tool| tool.cancel_on_input)
            })
    }

    pub(crate) fn hook_tool(&self, call: &ToolCall, description: Option<&str>) -> HookTool {
        let registered = self.get(&call.name);
        let handler = registered.and_then(|tool| match &tool.handler {
            RegisteredHandler::Tool(handler) => Some(handler),
            RegisteredHandler::Search => None,
        });
        let identity = handler.and_then(|handler| handler.hook_identity());
        let name = identity.map_or_else(|| call.name.clone(), |identity| identity.name.into());
        let subjects = identity.map_or_else(
            || vec![call.name.clone()],
            |identity| {
                identity
                    .subjects
                    .iter()
                    .map(|subject| (*subject).into())
                    .collect()
            },
        );
        let mut input = handler.map_or_else(
            || call.arguments.clone(),
            |handler| handler.hook_input(&call.arguments),
        );
        if let Some(description) = description
            && let Some(input) = input.as_object_mut()
        {
            input
                .entry("description")
                .or_insert_with(|| Value::String(description.into()));
        }
        HookTool {
            name,
            input,
            subjects,
        }
    }

    pub(crate) fn rewrite_hook_input(&self, name: &str, input: Value) -> Result<Value> {
        match self.get(name).map(|tool| &tool.handler) {
            Some(RegisteredHandler::Tool(handler)) => handler.rewrite_hook_input(input),
            Some(RegisteredHandler::Search) | None => object_hook_input(input),
        }
    }

    fn get(&self, name: &str) -> Option<&RegisteredTool> {
        self.tools.get(name)
    }
}

fn catalog_revision(tools: &BTreeMap<String, RegisteredTool>) -> String {
    let mut hasher = Sha256::new();
    for tool in tools.values() {
        hasher.update([match tool.exposure {
            ToolExposure::Direct => 0,
            ToolExposure::Deferred => 1,
            ToolExposure::Hidden => 2,
        }]);
        hash_field(&mut hasher, tool.definition.name.as_bytes());
        hash_field(&mut hasher, tool.definition.description.as_bytes());
        hash_field(
            &mut hasher,
            tool.definition.parameters.to_string().as_bytes(),
        );
    }
    format!("{:x}", hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn loaded_tools(
    input: &[Value],
    catalog_revision: &str,
    searchable: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let mut loaded = BTreeSet::new();
    for item in input {
        let Some(selection) = ToolLoad::from_input(item)? else {
            continue;
        };
        if selection.catalog_revision == catalog_revision {
            loaded.extend(
                selection
                    .tools
                    .into_iter()
                    .filter(|name| searchable.contains(name)),
            );
        }
    }
    Ok(loaded)
}

fn materialization_effects(
    catalog_revision: &str,
    tools: impl IntoIterator<Item = String>,
    turn_id: &str,
    load_id: &str,
) -> Result<ToolEffects> {
    let tools = tools.into_iter().collect::<Vec<_>>();
    if tools.is_empty() {
        return Ok(ToolEffects::default());
    }
    let load = ToolLoad {
        catalog_revision: catalog_revision.into(),
        tools,
    };
    Ok(ToolEffects {
        input: vec![load.clone().into_input()],
        events: vec![EventMsg::ToolLoad(ToolLoadEvent {
            turn_id: turn_id.into(),
            load_id: load_id.into(),
            catalog_revision: load.catalog_revision,
            tools: load.tools,
        })],
    })
}

/// A tool call proven callable for one model step.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundToolCall {
    call: ToolCall,
    materialized: bool,
    search_scope: Option<BTreeSet<String>>,
}

impl BoundToolCall {
    /// Returns the original provider call for hooks, approvals, and events.
    #[must_use]
    pub fn as_call(&self) -> &ToolCall {
        &self.call
    }

    /// Returns the validated provider call.
    #[must_use]
    pub fn into_call(self) -> ToolCall {
        self.call
    }
}

/// Returns the core discovery tool schema.
#[must_use]
pub fn tools_search_definition() -> ToolDefinition {
    ToolDefinition {
        name: TOOLS_SEARCH_NAME.into(),
        description: "Find currently available tools by name or description and load matching tools for this session.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_TOOL_SEARCH_QUERY_BYTES
                }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
    }
}

fn validate_definition(definition: &ToolDefinition) -> Result<()> {
    if definition.name.trim().is_empty() {
        return Err(Error::Config("tool name cannot be empty".into()));
    }
    if definition.name.len() > MAX_TOOL_NAME_BYTES {
        return Err(Error::Config(format!(
            "tool name exceeds {MAX_TOOL_NAME_BYTES} bytes"
        )));
    }
    if !definition.parameters.is_object() {
        return Err(Error::Config(format!(
            "tool `{}` parameters must be a JSON object",
            definition.name
        )));
    }
    Ok(())
}

fn object_hook_input(input: Value) -> Result<Value> {
    if input.is_object() {
        Ok(input)
    } else {
        Err(Error::Config(
            "hook tool rewrite must be a JSON object".into(),
        ))
    }
}

/// The result returned to the model for one tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    pub output: String,
    pub is_error: bool,
    pub(crate) handler_executed: bool,
    pub(crate) additional_input: Vec<Value>,
    pub(crate) events: Vec<EventMsg>,
}

impl ToolResult {
    pub(crate) fn error(call: &ToolCall, output: impl AsRef<str>) -> Self {
        Self {
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            output: capped(output.as_ref(), MAX_TOOL_OUTPUT_BYTES),
            is_error: true,
            handler_executed: false,
            additional_input: Vec::new(),
            events: Vec::new(),
        }
    }

    pub(crate) fn replace(&mut self, output: impl AsRef<str>) {
        self.output = capped(output.as_ref(), MAX_TOOL_OUTPUT_BYTES);
    }
}

/// Executes maximal runs of parallel-safe calls concurrently.
/// Exclusive calls form barriers and execute alone.
pub(crate) async fn execute_batch(
    catalog: &Catalog,
    calls: &[BoundToolCall],
    sandbox: Arc<Sandbox>,
    permissions: &SandboxPermissions,
    turn_id: &str,
) -> Vec<ToolResult> {
    let mut results = Vec::with_capacity(calls.len());
    let mut index = 0;
    while index < calls.len() {
        if is_parallel(catalog, &calls[index]) {
            let end = calls[index..]
                .iter()
                .position(|call| !is_parallel(catalog, call))
                .map_or(calls.len(), |offset| index + offset);
            // ModelOutput validation bounds every batch to 128 calls.
            results.extend(
                join_all(
                    calls[index..end]
                        .iter()
                        .cloned()
                        .map(|call| execute_call(catalog, call, &sandbox, permissions, turn_id)),
                )
                .await,
            );
            index = end;
        } else {
            results.push(
                execute_call(
                    catalog,
                    calls[index].clone(),
                    &sandbox,
                    permissions,
                    turn_id,
                )
                .await,
            );
            index += 1;
        }
    }
    results
}

fn is_parallel(catalog: &Catalog, call: &BoundToolCall) -> bool {
    catalog
        .get(&call.as_call().name)
        .is_some_and(|tool| tool.execution_mode == ExecutionMode::Parallel)
}

async fn execute_call(
    catalog: &Catalog,
    call: BoundToolCall,
    sandbox: &Arc<Sandbox>,
    permissions: &SandboxPermissions,
    turn_id: &str,
) -> ToolResult {
    let BoundToolCall {
        call,
        materialized,
        search_scope,
    } = call;
    let context = ToolContext {
        sandbox: Arc::clone(sandbox),
        permissions: permissions.for_call(&call.call_id),
        turn_id: turn_id.into(),
    };
    let Some(tool) = catalog.get(&call.name).cloned() else {
        return ToolResult::error(&call, format!("unknown tool `{}`", call.name));
    };
    let catalog_revision = match catalog.revision() {
        Ok(revision) => revision.to_owned(),
        Err(error) => return ToolResult::error(&call, error.to_string()),
    };
    match tool.exposure {
        ToolExposure::Direct => {}
        ToolExposure::Deferred if materialized => {}
        ToolExposure::Deferred => {
            return ToolResult::error(
                &call,
                format!(
                    "tool `{}` was not materialized for this model step",
                    call.name
                ),
            );
        }
        ToolExposure::Hidden => {
            return ToolResult::error(
                &call,
                format!("tool `{}` is hidden from the model", call.name),
            );
        }
    }
    if tool.approval == ApprovalRequirement::Always && !context.permissions.allows_mutation() {
        return ToolResult::error(&call, "tool call is not authorized to mutate state");
    }
    let ToolCall {
        call_id,
        name,
        arguments,
    } = call;
    let search_catalog = catalog.clone();
    let result = AssertUnwindSafe(async move {
        match tool.handler {
            RegisteredHandler::Tool(handler) => handler
                .call(context, arguments)
                .await
                .map(ToolOutput::content),
            RegisteredHandler::Search => {
                let Some(search_scope) = search_scope else {
                    return Err(Error::Tool("tools_search scope is unavailable".into()));
                };
                tools_search(&search_catalog, arguments, &search_scope)
            }
        }
    })
    .catch_unwind()
    .await;
    match result {
        Ok(Ok(output)) => {
            match materialization_effects(&catalog_revision, output.loaded_tools, turn_id, &call_id)
            {
                Ok(effects) => ToolResult {
                    call_id,
                    name,
                    output: capped(&output.content, MAX_TOOL_OUTPUT_BYTES),
                    is_error: false,
                    handler_executed: true,
                    additional_input: effects.input,
                    events: effects.events,
                },
                Err(error) => ToolResult {
                    call_id,
                    name,
                    output: capped(&error.to_string(), MAX_TOOL_OUTPUT_BYTES),
                    is_error: true,
                    handler_executed: true,
                    additional_input: Vec::new(),
                    events: Vec::new(),
                },
            }
        }
        Ok(Err(error)) => ToolResult {
            call_id,
            name,
            output: capped(&error.to_string(), MAX_TOOL_OUTPUT_BYTES),
            is_error: true,
            handler_executed: true,
            additional_input: Vec::new(),
            events: Vec::new(),
        },
        Err(_) => ToolResult {
            call_id,
            name,
            output: "tool panicked".into(),
            is_error: true,
            handler_executed: true,
            additional_input: Vec::new(),
            events: Vec::new(),
        },
    }
}

struct ToolOutput {
    content: String,
    loaded_tools: Vec<String>,
}

impl ToolOutput {
    fn content(content: String) -> Self {
        Self {
            content,
            loaded_tools: Vec::new(),
        }
    }
}

fn tools_search(
    catalog: &Catalog,
    arguments: Value,
    searchable: &BTreeSet<String>,
) -> Result<ToolOutput> {
    let Some(arguments) = arguments.as_object() else {
        return Err(Error::Tool(
            "tools_search arguments must be an object".into(),
        ));
    };
    if arguments.len() != 1 {
        return Err(Error::Tool(
            "tools_search accepts only the `query` argument".into(),
        ));
    }
    let query = arguments
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Tool("tools_search requires a string `query`".into()))?;
    let loaded_tools = catalog
        .search_deferred(query, searchable)?
        .into_iter()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    let content = serde_json::to_string(&serde_json::json!({
        "loaded_tools": &loaded_tools
    }))?;
    Ok(ToolOutput {
        content,
        loaded_tools,
    })
}

fn capped(output: &str, limit: usize) -> String {
    if output.len() <= limit {
        return output.to_string();
    }

    let left_budget = limit / 2;
    let right_budget = limit - left_budget;
    let left = crate::truncate_utf8(output, left_budget);
    let mut right_start = output.len() - right_budget;
    while !output.is_char_boundary(right_start) {
        right_start += 1;
    }
    let removed = output[left.len()..right_start].chars().count();
    format!(
        "{}…{removed} chars truncated…{}",
        left,
        &output[right_start..]
    )
}

fn compact_output(output: &str) -> String {
    let total_lines = output.lines().count();
    if output.len() <= MAX_TOOL_UI_BYTES && total_lines <= MAX_TOOL_UI_LINES {
        return output.to_string();
    }

    let kept_lines = if total_lines > MAX_TOOL_UI_LINES {
        MAX_TOOL_UI_LINES - 1
    } else {
        total_lines
    };
    let line_budget = MAX_TOOL_UI_BYTES / kept_lines.max(1);
    let mut preview = String::new();
    let mut first = true;
    let mut append = |line: &str| {
        if !first {
            preview.push('\n');
        }
        first = false;
        preview.push_str(&capped(line, line_budget));
    };

    if total_lines <= MAX_TOOL_UI_LINES {
        output.lines().for_each(&mut append);
        return preview;
    }

    let head_lines = (MAX_TOOL_UI_LINES - 1) / 2;
    output.lines().take(head_lines).for_each(&mut append);
    append(&format!(
        "… +{} lines",
        total_lines - (MAX_TOOL_UI_LINES - 1)
    ));
    let mut tail = output
        .lines()
        .rev()
        .take(MAX_TOOL_UI_LINES - 1 - head_lines)
        .collect::<Vec<_>>();
    tail.reverse();
    tail.into_iter().for_each(append);
    preview
}

/// Middleware that contributes an explicit list of tools.
pub struct Tools {
    tools: Vec<Arc<dyn Tool>>,
    names: BTreeSet<String>,
}

impl Tools {
    /// Creates a tool middleware from explicit handlers.
    #[must_use]
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        let mut names = tools
            .iter()
            .map(|tool| tool.definition().name)
            .collect::<BTreeSet<_>>();
        names.insert(TOOLS_SEARCH_NAME.into());
        Self { tools, names }
    }

    /// Creates the default file, foreground command, and background command tools.
    #[must_use]
    pub fn coding() -> Self {
        Self::new(vec![
            Arc::new(ReadFile),
            Arc::new(WriteFile),
            Arc::new(ApplyPatch),
            Arc::new(Bash),
            Arc::new(StartCommand),
            Arc::new(PollCommand),
            Arc::new(StopCommand),
        ])
    }

    fn section(&self) -> PromptSection {
        PromptSection::new(text::PROMPT_MAIN)
    }
}

impl Middleware for Tools {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn register(&self, catalog: &mut Catalog, _runtime: &super::RuntimeContext) -> Result<()> {
        for tool in &self.tools {
            catalog.register(Arc::clone(tool))?;
        }
        Ok(())
    }

    fn prompt_section(&self, _runtime: &super::RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(Some(self.section()))
    }

    fn frontend(&self) -> FrontendContribution {
        FrontendContribution {
            capability: self.name().into(),
            ..FrontendContribution::default()
        }
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        if let EventMsg::ToolLoad(load) = event {
            return Some(FrontendBlock {
                id: Some(format!("{}/{}/load", load.turn_id, load.load_id)),
                group: None,
                update: FrontendBlockUpdate::Replace,
                state: FrontendBlockState::Complete,
                role: FrontendBlockRole::Tool,
                title: text::RENDER_LOAD.into(),
                text: load.tools.join("\n"),
                symbol: None,
                files: Vec::new(),
                format: FrontendBlockFormat::PlainText,
                tone: FrontendTone::Success,
            });
        }
        let mut block = render_tool_event(event, |name| self.names.contains(name), tool_heading)?;
        match event {
            EventMsg::ToolCallBegin(call) if call.name == "read_file" => {
                block.group = Some(format!("read:{}", call.turn_id));
            }
            EventMsg::ToolCallEnd(result) if result.name == "read_file" => {
                block.group = Some(format!("read:{}", result.turn_id));
            }
            EventMsg::ToolCallEnd(result)
                if !result.is_error
                    && result.name == "apply_patch"
                    && Patch::from_str(&result.output).is_ok() =>
            {
                block.update = FrontendBlockUpdate::Replace;
                block.title = tool_heading(&result.name, &Value::Null).title;
                block.text = result.output.clone();
                block.format = FrontendBlockFormat::UnifiedDiff;
            }
            _ => {}
        }
        Some(block)
    }
}

pub(crate) fn render_tool_event(
    event: &EventMsg,
    owns: impl Fn(&str) -> bool,
    heading: impl Fn(&str, &Value) -> ToolHeading,
) -> Option<FrontendBlock> {
    match event {
        EventMsg::ToolCallBegin(call) if owns(&call.name) => {
            let heading = heading(&call.name, &call.arguments);
            Some(FrontendBlock {
                id: Some(format!("{}/{}", call.turn_id, call.call_id)),
                group: None,
                update: FrontendBlockUpdate::Replace,
                state: FrontendBlockState::Pending,
                role: FrontendBlockRole::Tool,
                title: heading.title,
                text: heading.detail,
                symbol: None,
                files: Vec::new(),
                format: FrontendBlockFormat::PlainText,
                tone: FrontendTone::Neutral,
            })
        }
        EventMsg::ToolCallEnd(result) if owns(&result.name) => {
            let output = compact_output(&result.output);
            Some(FrontendBlock {
                id: Some(format!("{}/{}", result.turn_id, result.call_id)),
                group: None,
                update: FrontendBlockUpdate::Append,
                state: FrontendBlockState::Complete,
                role: FrontendBlockRole::Tool,
                title: tool_heading(&result.name, &Value::Null).title,
                text: output,
                symbol: None,
                files: Vec::new(),
                format: FrontendBlockFormat::PlainText,
                tone: if result.is_error {
                    FrontendTone::Error
                } else {
                    FrontendTone::Success
                },
            })
        }
        _ => None,
    }
}

pub(crate) struct ToolHeading {
    pub(crate) title: String,
    pub(crate) detail: String,
}

impl From<&str> for ToolHeading {
    fn from(title: &str) -> Self {
        Self {
            title: title.into(),
            detail: String::new(),
        }
    }
}

impl From<String> for ToolHeading {
    fn from(title: String) -> Self {
        Self {
            title,
            detail: String::new(),
        }
    }
}

fn tool_heading(name: &str, arguments: &Value) -> ToolHeading {
    if name == "apply_patch" {
        let detail = arguments
            .get("patch")
            .and_then(Value::as_str)
            .and_then(|patch| {
                patch
                    .lines()
                    .find_map(|line| line.strip_prefix("*** Update File: "))
            })
            .unwrap_or_default()
            .into();
        return ToolHeading {
            title: text::RENDER_APPLY_PATCH.into(),
            detail,
        };
    }
    let (label, detail) = match name {
        "read_file" => (text::RENDER_READ_FILE, "path"),
        "write_file" => (text::RENDER_WRITE_FILE, "path"),
        "bash" => (text::RENDER_BASH, "command"),
        "start_command" => (text::RENDER_START_COMMAND, "command"),
        "poll_command" => (text::RENDER_POLL_COMMAND, "command_id"),
        "stop_command" => (text::RENDER_STOP_COMMAND, "command_id"),
        _ => {
            return ToolHeading {
                title: name.into(),
                detail: preview_json(arguments),
            };
        }
    };
    labeled_tool_heading(label, detail, arguments)
}

pub(crate) fn labeled_tool_heading(label: &str, detail: &str, arguments: &Value) -> ToolHeading {
    ToolHeading {
        title: label.into(),
        detail: arguments
            .get(detail)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or_default()
            .into(),
    }
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
