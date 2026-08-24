use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::join_all;
use regex::Regex;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::Map;
use serde_json::Value;

use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::backend::sandbox::ApprovalPolicy;
use crate::backend::sandbox::CommandMode;
use crate::backend::sandbox::CommandOutput;
use crate::backend::sandbox::CommandOutputSink;
use crate::backend::sandbox::NetworkAccess;
use crate::backend::sandbox::SandboxBackend;
use crate::backend::sandbox::SandboxMode;
use crate::truncate_utf8;

use super::AuthorizedHooks;

const DEFAULT_HOOK_FILE: &str = "hooks/hooks.json";
const MAX_HOOK_FILES: usize = 16;
const MAX_HOOKS: usize = 64;
const MAX_MATCHING_HOOKS: usize = 256;
const MAX_DOCUMENT_BYTES: u64 = 256_000;
const MAX_COMMAND_BYTES: usize = 8_000;
const MAX_MATCHER_BYTES: usize = 1_024;
const MAX_PATH_BYTES: usize = 4_096;
const MAX_STATUS_BYTES: usize = 256;
const MAX_INPUT_BYTES: usize = 5 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 40_000;
const DEFAULT_CONTEXT_BYTES: usize = 10_000;
const MAX_CONTEXT_BYTES: usize = 40_000;
const MAX_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_SESSION_END_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
pub(crate) enum HookEvent {
    SessionStart,
    SessionEnd,
    SubagentStart,
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    PreCompact,
    PostCompact,
    UserPromptSubmit,
    SubagentStop,
    Stop,
}

impl HookEvent {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::SubagentStart => "SubagentStart",
            Self::PreToolUse => "PreToolUse",
            Self::PermissionRequest => "PermissionRequest",
            Self::PostToolUse => "PostToolUse",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::SubagentStop => "SubagentStop",
            Self::Stop => "Stop",
        }
    }

    const fn uses_matcher(self) -> bool {
        !matches!(self, Self::UserPromptSubmit | Self::Stop)
    }
}

pub(super) const fn permission_mode(policy: ApprovalPolicy) -> &'static str {
    match policy {
        ApprovalPolicy::Ask | ApprovalPolicy::AutoApprove => "default",
        ApprovalPolicy::Allow | ApprovalPolicy::AllowNetwork => "dontAsk",
        ApprovalPolicy::FullAccess => "bypassPermissions",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum HookDecision {
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PermissionDecision {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct HookOutcome {
    pub(super) failure: Option<String>,
    pub(super) additional_context: Option<String>,
    pub(super) system_message: Option<String>,
    pub(super) decision: Option<HookDecision>,
    pub(super) reason: Option<String>,
    pub(super) updated_input: Option<Value>,
    pub(super) permission_decision: Option<PermissionDecision>,
    pub(super) continue_session: Option<bool>,
    pub(super) stop_reason: Option<String>,
}

pub(crate) struct HookSet {
    plugin_root: PathBuf,
    data_dir: PathBuf,
    hooks: BTreeMap<HookEvent, Vec<CommandHook>>,
}

pub(super) struct HookDefinitions {
    hooks: BTreeMap<HookEvent, Vec<CommandHook>>,
}

impl HookDefinitions {
    pub(super) fn load(plugin_root: &Path, manifest_hooks: Option<&Value>) -> Result<Self> {
        Ok(Self {
            hooks: load_hooks(plugin_root, manifest_hooks)?,
        })
    }

    pub(super) fn inspect(&self) -> Vec<super::ExtensionHook> {
        self.hooks
            .iter()
            .flat_map(|(event, hooks)| {
                hooks.iter().map(move |hook| super::ExtensionHook {
                    event: event.as_str().into(),
                    matcher: hook.matcher_text.clone(),
                    command: hook.command.clone(),
                    timeout_seconds: hook.timeout.as_secs(),
                })
            })
            .collect()
    }
}

impl HookSet {
    pub(super) fn new(
        plugin_root: PathBuf,
        data_dir: PathBuf,
        definitions: HookDefinitions,
    ) -> Result<Self> {
        let data_dir = canonical_directory(data_dir, "plugin data directory")?;
        Ok(Self {
            plugin_root,
            data_dir,
            hooks: definitions.hooks,
        })
    }

    #[cfg(test)]
    pub(crate) fn load(
        plugin_root: PathBuf,
        data_dir: PathBuf,
        manifest_hooks: Option<&Value>,
    ) -> Result<Self> {
        let definitions = HookDefinitions::load(&plugin_root, manifest_hooks)?;
        Self::new(plugin_root, data_dir, definitions)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.hooks.values().all(Vec::is_empty)
    }
}

fn load_hooks(
    plugin_root: &Path,
    manifest_hooks: Option<&Value>,
) -> Result<BTreeMap<HookEvent, Vec<CommandHook>>> {
    let mut hooks = BTreeMap::new();
    let mut count = 0;

    match manifest_hooks {
        Some(value) => load_manifest_sources(value, plugin_root, &mut hooks, &mut count)?,
        None => match plugin_root.join(DEFAULT_HOOK_FILE).symlink_metadata() {
            Ok(_) => {
                let path = super::confined_path(plugin_root, DEFAULT_HOOK_FILE, false)?;
                append_document(read_document(&path)?, &mut hooks, &mut count)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        },
    }

    Ok(hooks)
}

pub(crate) struct HookRuntime {
    backend: Arc<dyn SandboxBackend>,
    workspace: PathBuf,
}

impl HookRuntime {
    pub(crate) fn new(backend: Arc<dyn SandboxBackend>, workspace: PathBuf) -> Result<Self> {
        Ok(Self {
            backend,
            workspace: canonical_directory(workspace, "hook workspace")?,
        })
    }

    pub(crate) fn run_all<'a>(
        &'a self,
        sets: &'a [AuthorizedHooks],
        event: HookEvent,
        input: Value,
        matcher_subjects: &'a [&'a str],
    ) -> BoxFuture<'a, Result<Vec<HookOutcome>>> {
        Box::pin(async move {
            for subject in matcher_subjects {
                if subject.len() > MAX_MATCHER_BYTES {
                    return Err(Error::Config("hook matcher subject is too long".into()));
                }
            }

            let input = hook_input(input, event, &self.workspace)?;
            let mut matching = Vec::new();
            for authorized in sets {
                let Some(hooks) = authorized.set.hooks.get(&event) else {
                    continue;
                };
                for hook in hooks {
                    if hook.matches(event, matcher_subjects) {
                        if matching.len() == MAX_MATCHING_HOOKS {
                            return Err(Error::Config(format!(
                                "matching hook count exceeds {MAX_MATCHING_HOOKS}"
                            )));
                        }
                        matching.push((authorized, hook));
                    }
                }
            }

            Ok(join_all(
                matching
                    .into_iter()
                    .map(|(authorized, hook)| self.run_hook(authorized, hook, event, &input)),
            )
            .await)
        })
    }

    async fn run_hook(
        &self,
        authorized: &AuthorizedHooks,
        hook: &CommandHook,
        event: HookEvent,
        input: &str,
    ) -> HookOutcome {
        let set = &authorized.set;
        let mut input_file = match tempfile::NamedTempFile::new_in(&set.data_dir) {
            Ok(file) => file,
            Err(error) => return HookOutcome::failed(error.to_string()),
        };
        if let Err(error) = input_file.write_all(input.as_bytes()) {
            return HookOutcome::failed(error.to_string());
        }
        let command = match hook_command(
            &self.workspace,
            &set.plugin_root,
            &set.data_dir,
            &hook.command,
            input_file.path(),
        ) {
            Ok(command) => command,
            Err(error) => return HookOutcome::failed(error.to_string()),
        };
        let execution = self.backend.execute_authorized(
            &command,
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
            CommandMode::Foreground,
            CommandOutputSink::default(),
            &authorized.authorization,
        );
        match tokio::time::timeout(hook.timeout, execution).await {
            Err(_) => HookOutcome::failed(format!(
                "{} hook exceeded {} seconds",
                event.as_str(),
                hook.timeout.as_secs()
            )),
            Ok(Err(error)) => HookOutcome::failed(error.to_string()),
            Ok(Ok(None)) => HookOutcome::default(),
            Ok(Ok(Some(output))) => match parse_output(event, hook.context_bytes, output) {
                Ok(outcome) => outcome,
                Err(error) => HookOutcome::failed(error.to_string()),
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookDocument {
    #[serde(default)]
    description: Option<String>,
    hooks: BTreeMap<HookEvent, Vec<RawHookGroup>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHookGroup {
    matcher: Option<String>,
    hooks: Vec<RawHookHandler>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum RawHookHandler {
    Command {
        command: String,
        #[serde(rename = "commandWindows")]
        command_windows: Option<String>,
        timeout: Option<u64>,
        #[serde(rename = "statusMessage")]
        status_message: Option<String>,
        #[serde(rename = "additionalContextLimit")]
        additional_context_limit: Option<usize>,
        #[serde(rename = "async")]
        asynchronous: Option<bool>,
    },
    Prompt,
    Agent,
}

struct CommandHook {
    matcher: Option<Regex>,
    matcher_text: Option<String>,
    command: String,
    timeout: Duration,
    context_bytes: usize,
}

impl CommandHook {
    fn parse(event: HookEvent, matcher: Option<&str>, raw: RawHookHandler) -> Result<Option<Self>> {
        let RawHookHandler::Command {
            command,
            command_windows,
            timeout,
            status_message,
            additional_context_limit,
            asynchronous,
        } = raw
        else {
            return Ok(None);
        };
        if asynchronous == Some(true) && event != HookEvent::SessionEnd {
            return Err(Error::Config("asynchronous hooks are not supported".into()));
        }
        validate_text(&command, 1, MAX_COMMAND_BYTES, "hook command")?;
        if let Some(command_windows) = &command_windows {
            validate_text(
                command_windows,
                1,
                MAX_COMMAND_BYTES,
                "Windows hook command",
            )?;
        }
        if let Some(status) = &status_message {
            validate_text(status, 1, MAX_STATUS_BYTES, "hook status message")?;
        }

        let timeout = Duration::from_secs(timeout.unwrap_or_else(|| {
            if event == HookEvent::SessionEnd {
                1
            } else {
                MAX_TIMEOUT.as_secs()
            }
        }));
        let max_timeout = if event == HookEvent::SessionEnd {
            MAX_SESSION_END_TIMEOUT
        } else {
            MAX_TIMEOUT
        };
        if timeout.is_zero() || timeout > max_timeout {
            return Err(Error::Config(format!(
                "{} hook timeout must be between 1 and {} seconds",
                event.as_str(),
                max_timeout.as_secs()
            )));
        }

        let matcher_text = match matcher.map(str::trim) {
            None | Some("") | Some("*") => None,
            Some(matcher) => Some(matcher.to_owned()),
        };
        let matcher = match matcher_text.as_deref() {
            None => None,
            Some(matcher) => {
                validate_text(matcher, 1, MAX_MATCHER_BYTES, "hook matcher")?;
                Some(
                    RegexBuilder::new(matcher)
                        .size_limit(1 << 20)
                        .build()
                        .map_err(|error| {
                            Error::Config(format!("invalid hook matcher `{matcher}`: {error}"))
                        })?,
                )
            }
        };

        Ok(Some(Self {
            matcher,
            matcher_text,
            command,
            timeout,
            context_bytes: context_bytes(additional_context_limit)?,
        }))
    }

    fn matches(&self, event: HookEvent, subjects: &[&str]) -> bool {
        !event.uses_matcher()
            || self
                .matcher
                .as_ref()
                .is_none_or(|matcher| subjects.iter().any(|subject| matcher.is_match(subject)))
    }
}

fn load_manifest_sources(
    value: &Value,
    plugin_root: &Path,
    hooks: &mut BTreeMap<HookEvent, Vec<CommandHook>>,
    count: &mut usize,
) -> Result<()> {
    let sources = value
        .as_array()
        .map_or_else(|| vec![value], |sources| sources.iter().collect::<Vec<_>>());
    if sources.len() > MAX_HOOK_FILES {
        return Err(Error::Config(format!(
            "plugin hook source count exceeds {MAX_HOOK_FILES}"
        )));
    }
    for source in sources {
        let document = match source {
            Value::String(path) => read_document(&plugin_hook_path(plugin_root, path)?)?,
            Value::Object(object) => inline_document(object)?,
            _ => {
                return Err(Error::Config(
                    "plugin hooks must be a path, hooks object, or array of those".into(),
                ));
            }
        };
        append_document(document, hooks, count)?;
    }
    Ok(())
}

fn inline_document(object: &Map<String, Value>) -> Result<HookDocument> {
    let value = if object.contains_key("hooks") {
        Value::Object(object.clone())
    } else {
        let mut document = Map::new();
        document.insert("hooks".into(), Value::Object(object.clone()));
        Value::Object(document)
    };
    serde_json::from_value(value)
        .map_err(|error| Error::Config(format!("invalid inline plugin hooks: {error}")))
}

fn read_document(path: &Path) -> Result<HookDocument> {
    serde_json::from_slice(&super::read_bounded_file(
        path,
        MAX_DOCUMENT_BYTES,
        "plugin hook document",
    )?)
    .map_err(|error| {
        Error::Config(format!(
            "invalid plugin hook document {}: {error}",
            path.display()
        ))
    })
}

fn append_document(
    document: HookDocument,
    hooks: &mut BTreeMap<HookEvent, Vec<CommandHook>>,
    count: &mut usize,
) -> Result<()> {
    if let Some(description) = document.description {
        validate_text(&description, 1, MAX_STATUS_BYTES, "hook description")?;
    }
    for (event, groups) in document.hooks {
        for group in groups {
            for raw in group.hooks {
                if *count == MAX_HOOKS {
                    return Err(Error::Config(format!(
                        "plugin hook count exceeds {MAX_HOOKS}"
                    )));
                }
                *count += 1;
                if let Some(hook) = CommandHook::parse(event, group.matcher.as_deref(), raw)? {
                    hooks.entry(event).or_default().push(hook);
                }
            }
        }
    }
    Ok(())
}

fn plugin_hook_path(plugin_root: &Path, path: &str) -> Result<PathBuf> {
    if path.len() > MAX_PATH_BYTES || !path.starts_with("./") {
        return Err(Error::Config(
            "plugin hook paths must be ./-prefixed and bounded".into(),
        ));
    }
    super::confined_path(plugin_root, path, true)
}

fn canonical_directory(path: PathBuf, label: &str) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .map_err(|error| Error::Config(format!("invalid {label}: {error}")))?;
    if !path.is_dir() {
        return Err(Error::Config(format!("{label} is not a directory")));
    }
    if path.as_os_str().len() > MAX_PATH_BYTES {
        return Err(Error::Config(format!("{label} path is too long")));
    }
    Ok(path)
}

fn context_bytes(limit: Option<usize>) -> Result<usize> {
    match limit {
        None => Ok(DEFAULT_CONTEXT_BYTES),
        Some(0) => Ok(MAX_CONTEXT_BYTES),
        Some(limit) => limit
            .checked_mul(4)
            .filter(|bytes| *bytes <= MAX_CONTEXT_BYTES)
            .ok_or_else(|| {
                Error::Config(format!(
                    "hook additional context limit exceeds {} tokens",
                    MAX_CONTEXT_BYTES / 4
                ))
            }),
    }
}

fn hook_input(input: Value, event: HookEvent, workspace: &Path) -> Result<String> {
    let Value::Object(mut input) = input else {
        return Err(Error::Config("hook input must be a JSON object".into()));
    };
    input.insert(
        "cwd".into(),
        Value::String(path_text(workspace, "hook workspace")?.into()),
    );
    input.insert(
        "hook_event_name".into(),
        Value::String(event.as_str().into()),
    );
    let input = serde_json::to_string(&input)?;
    if input.len() > MAX_INPUT_BYTES {
        return Err(Error::Config(format!(
            "hook input exceeds {MAX_INPUT_BYTES} bytes"
        )));
    }
    Ok(input)
}

fn hook_command(
    workspace: &Path,
    plugin_root: &Path,
    data_dir: &Path,
    command: &str,
    input: &Path,
) -> Result<String> {
    let workspace = shell_quote(path_text(workspace, "hook workspace")?);
    let plugin_root = shell_quote(path_text(plugin_root, "plugin root")?);
    let data_dir = shell_quote(path_text(data_dir, "plugin data directory")?);
    let command = shell_quote(command);
    let input = shell_quote(path_text(input, "hook input")?);
    Ok(format!(
        "cd {workspace} && env PLUGIN_ROOT={plugin_root} PLUGIN_DATA={data_dir} CLAUDE_PLUGIN_ROOT={plugin_root} CLAUDE_PLUGIN_DATA={data_dir} /bin/sh -c {command} < {input}"
    ))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn path_text<'a>(path: &'a Path, label: &str) -> Result<&'a str> {
    path.to_str()
        .ok_or_else(|| Error::Config(format!("{label} must be valid UTF-8")))
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawHookOutput {
    #[serde(rename = "continue")]
    continue_session: Option<bool>,
    stop_reason: Option<String>,
    system_message: Option<String>,
    suppress_output: Option<bool>,
    decision: Option<HookDecision>,
    reason: Option<String>,
    hook_specific_output: Option<RawHookSpecificOutput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawHookSpecificOutput {
    hook_event_name: HookEvent,
    additional_context: Option<String>,
    permission_decision: Option<RawToolPermissionDecision>,
    permission_decision_reason: Option<String>,
    updated_input: Option<Value>,
    decision: Option<RawPermissionRequestDecision>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawToolPermissionDecision {
    Allow,
    Deny,
    Ask,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPermissionRequestDecision {
    behavior: PermissionDecision,
    message: Option<String>,
}

fn parse_output(
    event: HookEvent,
    context_bytes: usize,
    output: CommandOutput,
) -> Result<HookOutcome> {
    if output.stdout.len() > MAX_OUTPUT_BYTES || output.stderr.len() > MAX_OUTPUT_BYTES {
        return Err(Error::Config("hook output is too large".into()));
    }
    if output.stdout_truncated || output.stderr_truncated {
        return Err(Error::Config("hook output was truncated".into()));
    }
    let stderr = output.stderr;
    if output.exit_code != 0 {
        return nonzero_outcome(event, output.exit_code, stderr);
    }

    let stdout = output.stdout.trim();
    if stdout.is_empty() {
        return Ok(HookOutcome::default());
    }
    if !stdout.starts_with('{') {
        return match event {
            HookEvent::SessionStart | HookEvent::SubagentStart | HookEvent::UserPromptSubmit => {
                validate_text(stdout, 0, context_bytes, "hook additional context")?;
                Ok(HookOutcome {
                    additional_context: Some(stdout.into()),
                    ..HookOutcome::default()
                })
            }
            HookEvent::SubagentStop | HookEvent::Stop => Err(Error::Config(format!(
                "{} hook output must be JSON",
                event.as_str()
            ))),
            _ => Ok(HookOutcome::default()),
        };
    }

    let raw: RawHookOutput = serde_json::from_str(stdout).map_err(|error| {
        Error::Config(format!("invalid {} hook output: {error}", event.as_str()))
    })?;
    normalize_output(event, context_bytes, raw)
}

fn nonzero_outcome(event: HookEvent, exit_code: i32, stderr: String) -> Result<HookOutcome> {
    if exit_code == 2
        && matches!(
            event,
            HookEvent::PreToolUse
                | HookEvent::PostToolUse
                | HookEvent::UserPromptSubmit
                | HookEvent::SubagentStop
                | HookEvent::Stop
        )
    {
        validate_text(&stderr, 1, MAX_OUTPUT_BYTES, "hook blocking reason")?;
        let (permission_decision, decision) = if event == HookEvent::PreToolUse {
            (Some(PermissionDecision::Deny), None)
        } else {
            (None, Some(HookDecision::Block))
        };
        return Ok(HookOutcome {
            reason: Some(stderr),
            permission_decision,
            decision,
            ..Default::default()
        });
    }

    let detail = if stderr.trim().is_empty() {
        format!("{} hook exited with status {exit_code}", event.as_str())
    } else {
        format!(
            "{} hook exited with status {exit_code}: {}",
            event.as_str(),
            stderr.trim()
        )
    };
    Ok(HookOutcome {
        failure: Some(bounded(detail)),
        ..Default::default()
    })
}

fn normalize_output(
    event: HookEvent,
    context_bytes: usize,
    raw: RawHookOutput,
) -> Result<HookOutcome> {
    let RawHookOutput {
        mut continue_session,
        mut stop_reason,
        system_message,
        suppress_output,
        decision,
        reason,
        hook_specific_output,
    } = raw;
    if event == HookEvent::SubagentStart {
        continue_session = None;
        stop_reason = None;
    }
    validate_optional_text(&stop_reason, MAX_OUTPUT_BYTES, "hook stop reason")?;
    validate_optional_text(&system_message, MAX_OUTPUT_BYTES, "hook system message")?;
    validate_optional_text(&reason, MAX_OUTPUT_BYTES, "hook reason")?;

    let (additional_context, tool_permission, permission_request, updated_input, specific_reason) =
        match hook_specific_output {
            None => (None, None, None, None, None),
            Some(specific) => {
                if specific.hook_event_name != event {
                    return Err(Error::Config(format!(
                        "{} hook returned output for {}",
                        event.as_str(),
                        specific.hook_event_name.as_str()
                    )));
                }
                validate_optional_text(
                    &specific.additional_context,
                    context_bytes,
                    "hook additional context",
                )?;
                validate_optional_text(
                    &specific.permission_decision_reason,
                    MAX_OUTPUT_BYTES,
                    "hook permission reason",
                )?;
                (
                    specific.additional_context,
                    specific.permission_decision,
                    specific.decision,
                    specific.updated_input,
                    specific.permission_decision_reason,
                )
            }
        };

    let fields = output_fields([
        continue_session.is_some(),
        stop_reason.is_some(),
        suppress_output.is_some(),
        decision.is_some(),
        reason.is_some(),
        additional_context.is_some(),
        tool_permission.is_some(),
        permission_request.is_some(),
        updated_input.is_some(),
    ]);
    validate_output_shape(event, fields, tool_permission)?;

    let permission_decision = match event {
        HookEvent::PreToolUse => match tool_permission {
            Some(RawToolPermissionDecision::Allow) => Some(PermissionDecision::Allow),
            Some(RawToolPermissionDecision::Deny) => Some(PermissionDecision::Deny),
            Some(RawToolPermissionDecision::Ask) => {
                return Err(Error::Config(
                    "PreToolUse permissionDecision `ask` is not supported".into(),
                ));
            }
            None => None,
        },
        HookEvent::PermissionRequest => permission_request
            .as_ref()
            .map(|decision| decision.behavior),
        _ => None,
    };
    let permission_reason = permission_request
        .and_then(|decision| decision.message)
        .or(specific_reason)
        .or(reason);

    Ok(HookOutcome {
        failure: None,
        additional_context,
        system_message,
        decision,
        reason: permission_reason,
        updated_input,
        permission_decision,
        continue_session,
        stop_reason,
    })
}

const CONTINUE: u16 = 1 << 0;
const STOP_REASON: u16 = 1 << 1;
const SUPPRESS_OUTPUT: u16 = 1 << 2;
const DECISION: u16 = 1 << 3;
const REASON: u16 = 1 << 4;
const ADDITIONAL_CONTEXT: u16 = 1 << 5;
const TOOL_PERMISSION: u16 = 1 << 6;
const PERMISSION_REQUEST: u16 = 1 << 7;
const UPDATED_INPUT: u16 = 1 << 8;

fn output_fields(present: [bool; 9]) -> u16 {
    [
        CONTINUE,
        STOP_REASON,
        SUPPRESS_OUTPUT,
        DECISION,
        REASON,
        ADDITIONAL_CONTEXT,
        TOOL_PERMISSION,
        PERMISSION_REQUEST,
        UPDATED_INPUT,
    ]
    .into_iter()
    .zip(present)
    .filter_map(|(field, present)| present.then_some(field))
    .fold(0, |fields, field| fields | field)
}

fn validate_output_shape(
    event: HookEvent,
    fields: u16,
    tool_permission: Option<RawToolPermissionDecision>,
) -> Result<()> {
    let has = |field| fields & field != 0;
    if has(DECISION) != has(REASON) {
        return Err(Error::Config(
            "hook decision `block` requires a reason".into(),
        ));
    }
    if has(UPDATED_INPUT) && !matches!(tool_permission, Some(RawToolPermissionDecision::Allow)) {
        return Err(Error::Config(
            "updatedInput requires permissionDecision `allow`".into(),
        ));
    }

    let allowed = match event {
        HookEvent::SessionStart => CONTINUE | STOP_REASON | SUPPRESS_OUTPUT | ADDITIONAL_CONTEXT,
        HookEvent::SessionEnd => 0,
        HookEvent::SubagentStart => ADDITIONAL_CONTEXT,
        HookEvent::PreToolUse => {
            DECISION | REASON | ADDITIONAL_CONTEXT | TOOL_PERMISSION | UPDATED_INPUT
        }
        HookEvent::PermissionRequest => PERMISSION_REQUEST,
        HookEvent::PostToolUse => CONTINUE | STOP_REASON | DECISION | REASON | ADDITIONAL_CONTEXT,
        HookEvent::PreCompact | HookEvent::PostCompact => CONTINUE | STOP_REASON | SUPPRESS_OUTPUT,
        HookEvent::UserPromptSubmit => {
            CONTINUE | STOP_REASON | SUPPRESS_OUTPUT | DECISION | REASON | ADDITIONAL_CONTEXT
        }
        HookEvent::SubagentStop | HookEvent::Stop => {
            CONTINUE | STOP_REASON | SUPPRESS_OUTPUT | DECISION | REASON
        }
    };
    if fields & !allowed != 0 {
        return Err(Error::Config(format!(
            "{} hook returned unsupported output fields",
            event.as_str()
        )));
    }
    Ok(())
}

impl HookOutcome {
    pub(super) fn failed(message: String) -> Self {
        Self {
            failure: Some(bounded(message)),
            ..Self::default()
        }
    }
}

fn validate_optional_text(value: &Option<String>, max: usize, label: &str) -> Result<()> {
    if let Some(value) = value {
        validate_text(value, 0, max, label)?;
    }
    Ok(())
}

fn validate_text(value: &str, min: usize, max: usize, label: &str) -> Result<()> {
    if value.len() < min || value.len() > max || value.contains('\0') {
        return Err(Error::Config(format!(
            "{label} must be {min}–{max} bytes and contain no nulls"
        )));
    }
    Ok(())
}

fn bounded(value: String) -> String {
    truncate_utf8(&value, MAX_OUTPUT_BYTES).into()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use serde_json::json;
    use tokio::sync::Notify;

    use super::*;
    use crate::backend::sandbox::CommandAuthorization;

    #[test]
    fn approval_policies_map_to_plugin_permission_modes() {
        assert_eq!(
            [
                ApprovalPolicy::Ask,
                ApprovalPolicy::Allow,
                ApprovalPolicy::AllowNetwork,
                ApprovalPolicy::AutoApprove,
                ApprovalPolicy::FullAccess,
            ]
            .map(permission_mode),
            [
                "default",
                "dontAsk",
                "dontAsk",
                "default",
                "bypassPermissions",
            ]
        );
    }

    struct FakeSandbox {
        commands: Mutex<Vec<String>>,
        started: AtomicUsize,
        release: Notify,
    }

    impl FakeSandbox {
        fn new() -> Self {
            Self {
                commands: Mutex::new(Vec::new()),
                started: AtomicUsize::new(0),
                release: Notify::new(),
            }
        }

        fn launch(&self, command: &str) -> bool {
            self.commands.lock().expect("commands").push(command.into());
            let second = self.started.fetch_add(1, Ordering::SeqCst) + 1 == 2;
            if second {
                self.release.notify_waiters();
            }
            second
        }

        fn output(command: &str) -> CommandOutput {
            let stdout = if command.contains("/bin/sh -c 'first' <") {
                r#"{"systemMessage":"first"}"#
            } else {
                r#"{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"second"}}"#
            };
            CommandOutput {
                exit_code: 0,
                stdout: stdout.into(),
                stdout_truncated: false,
                stderr: String::new(),
                stderr_truncated: false,
            }
        }
    }

    impl SandboxBackend for FakeSandbox {
        fn read<'a>(&'a self, _path: &'a str) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { Err(Error::Sandbox("unexpected read".into())) })
        }

        fn read_bytes<'a>(
            &'a self,
            _path: &'a str,
            _max_bytes: usize,
        ) -> BoxFuture<'a, Result<Vec<u8>>> {
            Box::pin(async { Err(Error::Sandbox("unexpected read".into())) })
        }

        fn write<'a>(&'a self, _path: &'a str, _content: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Err(Error::Sandbox("unexpected write".into())) })
        }

        fn execute<'a>(
            &'a self,
            command: &'a str,
            sandbox_mode: SandboxMode,
            network_access: NetworkAccess,
            mode: CommandMode,
            _output: CommandOutputSink,
        ) -> BoxFuture<'a, Result<CommandOutput>> {
            Box::pin(async move {
                assert_eq!(sandbox_mode, SandboxMode::WorkspaceWrite);
                assert_eq!(network_access, NetworkAccess::Denied);
                assert_eq!(mode, CommandMode::Foreground);
                let released = self.release.notified();
                if !self.launch(command) {
                    released.await;
                }
                Ok(Self::output(command))
            })
        }

        fn execute_authorized<'a>(
            &'a self,
            command: &'a str,
            sandbox_mode: SandboxMode,
            network_access: NetworkAccess,
            mode: CommandMode,
            _output: CommandOutputSink,
            authorization: &'a CommandAuthorization,
        ) -> BoxFuture<'a, Result<Option<CommandOutput>>> {
            Box::pin(async move {
                assert_eq!(sandbox_mode, SandboxMode::WorkspaceWrite);
                assert_eq!(network_access, NetworkAccess::Denied);
                assert_eq!(mode, CommandMode::Foreground);
                let released = self.release.notified();
                let mut second = None;
                authorization(&mut || {
                    second = Some(self.launch(command));
                    Ok(())
                })?;
                let Some(second) = second else {
                    return Ok(None);
                };
                if !second {
                    released.await;
                }
                Ok(Some(Self::output(command)))
            })
        }
    }

    #[test]
    fn parser_accepts_every_event_and_skips_inert_handlers() {
        let root = tempfile::tempdir().expect("plugin root");
        let data = tempfile::tempdir().expect("plugin data");
        let events = [
            "SessionStart",
            "SessionEnd",
            "SubagentStart",
            "PreToolUse",
            "PermissionRequest",
            "PostToolUse",
            "PreCompact",
            "PostCompact",
            "UserPromptSubmit",
            "SubagentStop",
            "Stop",
        ];
        let hooks = events
            .into_iter()
            .map(|event| {
                (
                    event.into(),
                    json!([{"matcher":"^Bash$","hooks":[{"type":"command","command":"true","timeout":1,"async":false}]}]),
                )
            })
            .collect::<Map<_, _>>();
        let set = HookSet::load(
            root.path().into(),
            data.path().into(),
            Some(&Value::Object(hooks)),
        )
        .expect("valid hooks");
        assert_eq!(set.hooks.values().map(Vec::len).sum::<usize>(), 11);

        let inert = json!({"Stop":[{"hooks":[
            {"type":"prompt"},
            {"type":"prompt","command":"true"},
            {"type":"agent"}
        ]}]});
        assert!(
            HookSet::load(root.path().into(), data.path().into(), Some(&inert))
                .expect("inert handlers parse")
                .is_empty()
        );
        HookSet::load(
            root.path().into(),
            data.path().into(),
            Some(&json!({"SessionEnd":[{"hooks":[{"type":"command","command":"true","async":true}]}]})),
        )
        .expect("SessionEnd remains synchronous");

        for invalid in [
            json!({"Stop":[{"hooks":[{"type":"command","command":"true","async":true}]}]}),
            json!({"Stop":[{"hooks":[{"type":"command","command":"true","unknown":true}]}]}),
        ] {
            assert!(HookSet::load(root.path().into(), data.path().into(), Some(&invalid)).is_err());
        }
    }

    #[test]
    fn legacy_pre_tool_block_is_a_denial() {
        let outcome = parse_output(
            HookEvent::PreToolUse,
            DEFAULT_CONTEXT_BYTES,
            CommandOutput {
                exit_code: 0,
                stdout: r#"{"decision":"block","reason":"unsafe"}"#.into(),
                stdout_truncated: false,
                stderr: String::new(),
                stderr_truncated: false,
            },
        )
        .expect("legacy block");

        assert_eq!(outcome.decision, Some(HookDecision::Block));
        assert_eq!(outcome.reason.as_deref(), Some("unsafe"));
    }

    #[tokio::test]
    async fn runner_starts_matches_concurrently_and_preserves_declaration_order() {
        let workspace = tempfile::tempdir().expect("workspace");
        let root = tempfile::tempdir().expect("plugin root");
        let data = tempfile::tempdir().expect("plugin data");
        let hooks = json!({
            "SessionStart": [{
                "matcher": "startup|resume",
                "hooks": [
                    {"type":"command","command":"first","timeout":1},
                    {"type":"command","command":"second","timeout":1}
                ]
            }]
        });
        let set =
            HookSet::load(root.path().into(), data.path().into(), Some(&hooks)).expect("hook set");
        let permitted = Arc::new(AtomicBool::new(true));
        let gate = Arc::clone(&permitted);
        let set = AuthorizedHooks {
            set,
            authorization: Arc::new(move |launch| {
                if gate.load(Ordering::SeqCst) {
                    launch()?;
                }
                Ok(())
            }),
        };
        let sets = [set];
        let backend = Arc::new(FakeSandbox::new());
        let runtime = HookRuntime::new(backend.clone(), workspace.path().into()).expect("runtime");
        let outcomes = runtime
            .run_all(
                &sets,
                HookEvent::SessionStart,
                json!({"session_id":"thread","source":"startup","prompt":"it's safe"}),
                &["startup"],
            )
            .await
            .expect("outcomes");

        assert_eq!(backend.started.load(Ordering::SeqCst), 2);
        assert_eq!(outcomes[0].system_message.as_deref(), Some("first"));
        assert_eq!(outcomes[1].additional_context.as_deref(), Some("second"));
        assert!(
            backend
                .commands
                .lock()
                .expect("commands")
                .iter()
                .all(|command| command.contains(" < '") && command.contains("PLUGIN_ROOT='"))
        );

        permitted.store(false, Ordering::SeqCst);
        runtime
            .run_all(
                &sets,
                HookEvent::SessionStart,
                json!({"session_id":"thread","source":"startup"}),
                &["startup"],
            )
            .await
            .expect("revoked outcomes");
        assert_eq!(backend.started.load(Ordering::SeqCst), 2);
    }
}
