//! Standalone skills and activated Agent Plugin packages.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use cap_std::ambient_authority;
use cap_std::fs::Dir;
use serde::Deserialize;
use serde_json::Value;

use super::CompactContext;
use super::Middleware;
use super::PermissionRequestContext;
use super::PostToolUseContext;
use super::PreToolUseContext;
use super::PromptSection;
use super::RuntimeContext;
use super::SessionStartContext;
use super::SessionStartSource;
use super::StopContext;
use super::UserPromptSubmitContext;
use super::manifest::MiddlewareManifest;
use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::agent::AgentRole;
use crate::backend::model::internal_user_message;
use crate::backend::sandbox::ApprovalPolicy;
use crate::backend::sandbox::CommandAuthorization;
use crate::backend::sandbox::SandboxBackend;
use crate::protocol::EventMsg;
use crate::protocol::FrontendContribution;
use crate::protocol::FrontendReference;
use crate::protocol::FrontendSlot;
use crate::protocol::FrontendTone;
use crate::protocol::FrontendWidget;
use crate::protocol::WarningEvent;
use crate::protocol::internal_message_kind;
use crate::truncate_utf8;

mod hooks;

mod text {
    include!(concat!(
        env!("OUT_DIR"),
        "/src_middleware_extensions_text.rs"
    ));
}

const MAX_SKILLS: usize = 64;
const MAX_SKILL_BYTES: u64 = 40_000;
const MAX_PLUGINS: usize = 32;
const MAX_PLUGIN_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_PLUGIN_ID_BYTES: usize = 128;
const MAX_PLUGIN_VERSION_BYTES: usize = 128;
const MAX_HOOK_CONTEXT_BYTES: usize = 40_000;
const MAX_HOOK_NOTICES: usize = 32;
const SKILL_FILE: &str = "SKILL.md";
const LEGACY_PLUGIN_MANIFEST: &str = ".codex-plugin/plugin.json";
const AGENT_PLUGIN_MANIFEST: &str = "plugin.json";
const MCP_COMPONENTS: [&str; 2] = [".mcp.json", "mcp.json"];
const AGENT_PLUGIN_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";
const SESSION_HOOK_CONTEXT_KIND: &str = "extension_session_hook";

/// Fail-closed authorization checked immediately before each plugin hook command starts.
pub type HookAuthorization = CommandAuthorization;

/// Configuration and presentation metadata for installed extensions.
pub const MANIFEST: MiddlewareManifest = MiddlewareManifest {
    id: "extensions",
    label: text::MANIFEST_LABEL,
    description: text::MANIFEST_DESCRIPTION,
    required: false,
    default_enabled: true,
    settings: &[],
};

/// One validated package format understood by the extensions middleware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionPackageKind {
    Skill,
    Plugin,
}

/// Frontend-safe metadata read from one extension package root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionPackage {
    pub kind: ExtensionPackageKind,
    pub name: String,
    pub version: Option<String>,
    pub description: String,
    pub skills: Vec<String>,
    pub hooks: Vec<ExtensionHook>,
}

/// One executable hook shown to an owner before trust is granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionHook {
    pub event: String,
    pub matcher: Option<String>,
    pub command: String,
    pub timeout_seconds: u64,
}

/// Validates and inspects one standalone Agent Skill or Agent Plugin package.
pub fn inspect_package(root: impl AsRef<Path>) -> Result<ExtensionPackage> {
    let root = canonical_plugin_root(root.as_ref())?;
    match agent_plugin_schema(&root)? {
        Some(schema) if schema == AGENT_PLUGIN_SCHEMA => inspect_agent_plugin(&root),
        Some(schema) => Err(Error::Config(format!(
            "unsupported Agent Plugin schema `{schema}`"
        ))),
        None if root.join(LEGACY_PLUGIN_MANIFEST).exists() => inspect_legacy_plugin(&root),
        None => inspect_skill(&root),
    }
}

fn inspect_legacy_plugin(root: &Path) -> Result<ExtensionPackage> {
    let manifest = load_legacy_plugin_manifest(root)?;
    reject_mcp_components(root, &manifest.name)?;
    let mut skills = BTreeMap::new();
    discover_plugin_skills(root, &manifest, &mut skills)?;
    let hooks = hooks::inspect(root, manifest.hooks.as_ref())?;
    if skills.is_empty() && hooks.is_empty() {
        return Err(Error::Config(format!(
            "plugin `{}` has no supported contributions",
            manifest.name
        )));
    }
    Ok(ExtensionPackage {
        kind: ExtensionPackageKind::Plugin,
        name: manifest.name,
        version: manifest.version,
        description: manifest.description.unwrap_or_default(),
        skills: skills.into_keys().collect(),
        hooks,
    })
}

fn inspect_agent_plugin(root: &Path) -> Result<ExtensionPackage> {
    let manifest = load_agent_plugin_manifest(root)?;
    reject_mcp_components(root, &manifest.name)?;
    let mut skills = BTreeMap::new();
    discover_agent_plugin_skills(root, &manifest.name, &mut skills)?;
    if skills.is_empty() {
        return Err(Error::Config(format!(
            "plugin `{}` has no supported contributions",
            manifest.name
        )));
    }
    Ok(ExtensionPackage {
        kind: ExtensionPackageKind::Plugin,
        name: manifest.name,
        version: manifest.version,
        description: manifest.description.unwrap_or_default(),
        skills: skills.into_keys().collect(),
        hooks: Vec::new(),
    })
}

fn inspect_skill(root: &Path) -> Result<ExtensionPackage> {
    let path = root.join(SKILL_FILE);
    let content = String::from_utf8(read_bounded_file(&path, MAX_SKILL_BYTES, "skill manifest")?)
        .map_err(|_| Error::Config("skill manifest is not valid UTF-8".into()))?;
    let (name, description) = skill_metadata(&path, &content);
    if !valid_legacy_package_name(&name) {
        return Err(Error::Config(format!(
            "skill name `{name}` must be kebab-case"
        )));
    }
    Ok(ExtensionPackage {
        kind: ExtensionPackageKind::Skill,
        skills: vec![name.clone()],
        name,
        version: None,
        description,
        hooks: Vec::new(),
    })
}

#[derive(Clone)]
struct Skill {
    name: String,
    description: String,
    location: PathBuf,
}

struct AuthorizedHooks {
    set: hooks::HookSet,
    authorization: HookAuthorization,
}

/// Discovers bounded skill extensions and advertises their resource locations.
pub struct Extensions {
    skills: BTreeMap<String, Skill>,
    plugins: BTreeSet<String>,
    hooks: Vec<AuthorizedHooks>,
    hook_runtime: Option<hooks::HookRuntime>,
    prompt: String,
}

impl Extensions {
    /// Discovers direct child `SKILL.md` files under each root.
    pub fn discover(roots: impl IntoIterator<Item = PathBuf>) -> Result<Self> {
        let mut skills = BTreeMap::new();
        discover_roots(roots, &mut skills, None, false)?;
        Ok(Self {
            skills,
            plugins: BTreeSet::new(),
            hooks: Vec::new(),
            hook_runtime: None,
            prompt: text::PROMPT_DEFAULT.into(),
        })
    }

    /// Adds user-installed skills after the explicit roots.
    pub fn discover_installed(roots: impl IntoIterator<Item = PathBuf>) -> Result<Self> {
        let mut discovered = Self::discover(roots)?;
        discover_roots(installed_skill_roots(), &mut discovered.skills, None, true)?;
        Ok(discovered)
    }

    /// Activates plugin snapshots and their declared contributions.
    ///
    /// The optional predicate authorizes command hooks for that snapshot and is checked
    /// immediately before every launch. Bundled skills remain available without it. Callers
    /// must pass immutable snapshots rather than paths discovered from a workspace.
    pub fn activate_plugins(
        mut self,
        roots: impl IntoIterator<Item = (PathBuf, Option<HookAuthorization>)>,
        workspace: impl AsRef<Path>,
        backend: Arc<dyn SandboxBackend>,
    ) -> Result<Self> {
        if !self.plugins.is_empty() {
            return Err(Error::Config("plugins were activated twice".into()));
        }
        let roots = roots.into_iter().collect::<Vec<_>>();
        if roots.is_empty() {
            return Ok(self);
        }
        let workspace = canonical_directory(workspace.as_ref(), "plugin workspace")?;
        let workspace_dir = Dir::open_ambient_dir(&workspace, ambient_authority())?;
        let data_root = workspace.join(".mobius/extensions");
        workspace_dir.create_dir_all(".mobius/extensions")?;
        let data_root_dir = workspace_dir.open_dir(".mobius/extensions")?;
        let mut ignore = cap_std::fs::OpenOptions::new();
        ignore.write(true).create_new(true);
        match data_root_dir.open_with(".gitignore", &ignore) {
            Ok(mut file) => file.write_all(b"*\n")?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let data_root = canonical_directory(&data_root, "plugin data root")?;
        if !data_root.starts_with(&workspace) {
            return Err(Error::Config(
                "plugin data root escapes the workspace".into(),
            ));
        }
        for (root, authorization) in roots {
            if self.plugins.len() == MAX_PLUGINS {
                return Err(Error::Config(format!("plugin count exceeds {MAX_PLUGINS}")));
            }
            let root = canonical_plugin_root(&root)?;
            if root.starts_with(&workspace) || workspace.starts_with(&root) {
                return Err(Error::Config(
                    "plugin snapshot and writable workspace must not overlap".into(),
                ));
            }
            let package = inspect_package(&root)?;
            if package.kind != ExtensionPackageKind::Plugin {
                return Err(Error::Config("activated extension is not a plugin".into()));
            }
            if !self.plugins.insert(package.name.clone()) {
                return Err(Error::Duplicate(format!("plugin `{}`", package.name)));
            }
            let skill_count = self.skills.len();
            let data = data_root.join(&package.name);
            data_root_dir.create_dir_all(&package.name)?;
            let data = canonical_directory(&data, "plugin data directory")?;
            if !data.starts_with(&data_root) {
                return Err(Error::Config(format!(
                    "plugin `{}` data directory escapes its root",
                    package.name
                )));
            }
            let hooks = if agent_plugin_schema(&root)?.as_deref() == Some(AGENT_PLUGIN_SCHEMA) {
                discover_agent_plugin_skills(&root, &package.name, &mut self.skills)?;
                None
            } else {
                let manifest = load_legacy_plugin_manifest(&root)?;
                discover_plugin_skills(&root, &manifest, &mut self.skills)?;
                Some(hooks::HookSet::load(root, data, manifest.hooks.as_ref())?)
            };
            if self.skills.len() == skill_count
                && hooks.as_ref().is_none_or(hooks::HookSet::is_empty)
            {
                return Err(Error::Config(format!(
                    "plugin `{}` has no supported contributions",
                    package.name
                )));
            }
            if let (Some(authorization), Some(hooks)) = (authorization, hooks)
                && !hooks.is_empty()
            {
                self.hooks.push(AuthorizedHooks {
                    set: hooks,
                    authorization,
                });
            }
        }
        if !self.hooks.is_empty() {
            self.hook_runtime = Some(hooks::HookRuntime::new(backend, workspace)?);
        }
        Ok(self)
    }

    /// Overrides the instruction placed before discovered skill metadata.
    pub fn prompt(mut self, prompt: impl Into<String>) -> Result<Self> {
        let prompt = prompt.into();
        if prompt.trim().is_empty() {
            return Err(Error::Config("extensions prompt cannot be empty".into()));
        }
        self.prompt = prompt;
        Ok(self)
    }

    /// Returns the canonical skill directories that generic read tools may access.
    #[must_use]
    pub fn resource_roots(&self) -> Vec<PathBuf> {
        self.skills
            .values()
            .filter_map(|skill| skill.location.parent().map(Path::to_path_buf))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn section(&self) -> Option<PromptSection> {
        if self.skills.is_empty() {
            return None;
        }
        let skills = self
            .skills
            .values()
            .map(|skill| {
                format!(
                    "- name: {}\n  description: {}\n  location: {}",
                    prompt_value(&skill.name),
                    prompt_value(&skill.description),
                    prompt_value(&skill.location.display().to_string())
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        Some(PromptSection::new(format!(
            "{}\n\n{skills}",
            self.prompt.trim()
        )))
    }

    async fn run_hooks(
        &self,
        event: hooks::HookEvent,
        input: Value,
        matcher_subjects: &[&str],
    ) -> Vec<hooks::HookOutcome> {
        match &self.hook_runtime {
            Some(runtime) => match runtime
                .run_all(&self.hooks, event, input, matcher_subjects)
                .await
            {
                Ok(outcomes) => outcomes,
                Err(error) => vec![hooks::HookOutcome::failed(error.to_string())],
            },
            None => Vec::new(),
        }
    }

    async fn run_compact_hook(
        &self,
        event: hooks::HookEvent,
        context: &mut CompactContext<'_>,
    ) -> Result<()> {
        let mut input = hook_input(
            context.session_id,
            context.model,
            None,
            Some(context.turn_id),
        );
        input.insert("trigger".into(), Value::String("auto".into()));
        let outcomes = self.run_hooks(event, Value::Object(input), &["auto"]).await;
        push_hook_notices(context.events, &outcomes);
        if let Some(outcome) = outcomes
            .iter()
            .find(|outcome| outcome.continue_session == Some(false))
        {
            context.stop(hook_stop_reason(outcome))?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyPluginManifest {
    name: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    skills: Option<String>,
    #[serde(default)]
    hooks: Option<Value>,
    #[serde(default)]
    mcp_servers: Option<Value>,
    #[serde(default)]
    apps: Option<Value>,
    #[serde(flatten)]
    metadata: BTreeMap<String, Value>,
}

impl LegacyPluginManifest {
    fn validate(&self) -> Result<()> {
        if !valid_legacy_package_name(&self.name) {
            return Err(Error::Config(format!(
                "plugin name `{}` must be kebab-case",
                self.name
            )));
        }
        if self.version.as_ref().is_some_and(|version| {
            version.is_empty()
                || version.len() > MAX_PLUGIN_VERSION_BYTES
                || !version.bytes().all(|byte| byte.is_ascii_graphic())
        }) {
            return Err(Error::Config(format!(
                "plugin `{}` has an invalid version",
                self.name
            )));
        }
        if self
            .description
            .as_ref()
            .is_some_and(|description| description.is_empty() || description.len() > 4_096)
        {
            return Err(Error::Config(format!(
                "plugin `{}` has an invalid description",
                self.name
            )));
        }
        const METADATA: [&str; 8] = [
            "author",
            "homepage",
            "repository",
            "license",
            "keywords",
            "interface",
            "bundledContentVariant",
            "$schema",
        ];
        if let Some(field) = self
            .metadata
            .keys()
            .find(|field| !METADATA.contains(&field.as_str()))
        {
            return Err(Error::Config(format!(
                "plugin `{}` has unknown manifest field `{field}`",
                self.name
            )));
        }
        Ok(())
    }
}

fn load_legacy_plugin_manifest(root: &Path) -> Result<LegacyPluginManifest> {
    let manifest_path = confined_path(root, LEGACY_PLUGIN_MANIFEST, false)?;
    let manifest: LegacyPluginManifest = serde_json::from_slice(&read_bounded_file(
        &manifest_path,
        MAX_PLUGIN_MANIFEST_BYTES,
        "plugin manifest",
    )?)?;
    manifest.validate()?;
    if manifest.mcp_servers.is_some() || manifest.apps.is_some() {
        return Err(Error::Config(format!(
            "legacy plugin `{}` declares an unsupported remote contribution",
            manifest.name
        )));
    }
    Ok(manifest)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentPluginManifest {
    #[serde(rename = "$schema")]
    schema: String,
    name: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    author: Option<AgentPluginAuthor>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    keywords: Option<Vec<String>>,
    #[serde(default)]
    extensions: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentPluginAuthor {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

fn agent_plugin_schema(root: &Path) -> Result<Option<String>> {
    let path = root.join(AGENT_PLUGIN_MANIFEST);
    match path.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return Ok(Some(String::new())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let value = match serde_json::from_slice::<Value>(&read_bounded_file(
        &path,
        MAX_PLUGIN_MANIFEST_BYTES,
        "Agent Plugin manifest",
    )?) {
        Ok(value) => value,
        Err(_) => return Ok(Some(String::new())),
    };
    Ok(Some(
        value
            .get("$schema")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    ))
}

fn load_agent_plugin_manifest(root: &Path) -> Result<AgentPluginManifest> {
    let path = confined_path(root, AGENT_PLUGIN_MANIFEST, false)?;
    let manifest: AgentPluginManifest = serde_json::from_slice(&read_bounded_file(
        &path,
        MAX_PLUGIN_MANIFEST_BYTES,
        "Agent Plugin manifest",
    )?)?;
    if manifest.schema != AGENT_PLUGIN_SCHEMA
        || manifest.name.len() > 64
        || !valid_package_name(&manifest.name)
        || manifest
            .version
            .as_ref()
            .is_some_and(|value| value.len() > MAX_PLUGIN_VERSION_BYTES)
        || manifest
            .description
            .as_ref()
            .is_some_and(|value| value.len() > 4_096)
        || manifest.extensions.values().any(|value| !value.is_object())
        || manifest
            .author
            .as_ref()
            .is_some_and(AgentPluginAuthor::is_too_large)
        || manifest
            .homepage
            .as_ref()
            .into_iter()
            .chain(manifest.repository.as_ref())
            .chain(manifest.license.as_ref())
            .any(|value| value.len() > 4_096)
        || manifest
            .keywords
            .as_ref()
            .is_some_and(|values| values.len() > 64 || values.iter().any(|value| value.len() > 256))
    {
        return Err(Error::Config(format!(
            "plugin `{}` has an invalid Agent Plugins v1 manifest",
            manifest.name
        )));
    }
    Ok(manifest)
}

impl AgentPluginAuthor {
    fn is_too_large(&self) -> bool {
        self.name
            .as_ref()
            .into_iter()
            .chain(self.email.as_ref())
            .chain(self.url.as_ref())
            .any(|value| value.len() > 4_096)
    }
}

fn reject_mcp_components(root: &Path, plugin_name: &str) -> Result<()> {
    for component in MCP_COMPONENTS {
        match root.join(component).symlink_metadata() {
            Ok(_) => {
                return Err(Error::Config(format!(
                    "plugin `{plugin_name}` declares an unsupported MCP contribution"
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn discover_plugin_skills(
    root: &Path,
    manifest: &LegacyPluginManifest,
    skills: &mut BTreeMap<String, Skill>,
) -> Result<()> {
    let Some(path) = manifest.skills.as_deref() else {
        return Ok(());
    };
    let root = confined_path(root, path, true)?;
    if !root.is_dir() {
        return Err(Error::Config(format!(
            "plugin `{}` skills path is not a directory",
            manifest.name
        )));
    }
    discover_roots([root], skills, Some(&manifest.name), false)
}

fn discover_agent_plugin_skills(
    root: &Path,
    plugin_name: &str,
    skills: &mut BTreeMap<String, Skill>,
) -> Result<()> {
    let root = root.join("skills");
    match root.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_dir() => {
            discover_roots([root], skills, Some(plugin_name), false)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Returns whether a package name is canonical for the supported extension formats.
#[must_use]
pub fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PLUGIN_ID_BYTES
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
        })
        && name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && !name.contains("--")
        && !name.contains("..")
}

fn valid_legacy_package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PLUGIN_ID_BYTES
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

fn canonical_plugin_root(path: &Path) -> Result<PathBuf> {
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(Error::Config(format!(
            "plugin root cannot be a symlink: {}",
            path.display()
        )));
    }
    canonical_directory(path, "plugin root")
}

fn canonical_directory(path: &Path, name: &str) -> Result<PathBuf> {
    let path = std::fs::canonicalize(path)?;
    if !path.is_dir() {
        return Err(Error::Config(format!(
            "{name} is not a directory: {}",
            path.display()
        )));
    }
    Ok(path)
}

fn confined_path(root: &Path, value: &str, require_dot_prefix: bool) -> Result<PathBuf> {
    if value.is_empty() || (require_dot_prefix && !value.starts_with("./")) {
        return Err(Error::Config(format!(
            "plugin path `{value}` must start with `./`"
        )));
    }
    let relative = Path::new(value);
    if relative.is_absolute()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(Error::Config(format!(
            "plugin path `{value}` is not confined to its package"
        )));
    }
    let mut current = root.to_path_buf();
    for part in relative.components() {
        let Component::Normal(part) = part else {
            continue;
        };
        current.push(part);
        if std::fs::symlink_metadata(&current)?
            .file_type()
            .is_symlink()
        {
            return Err(Error::Config(format!(
                "plugin path `{value}` contains a symlink"
            )));
        }
    }
    let path = std::fs::canonicalize(current)?;
    if !path.starts_with(root) {
        return Err(Error::Config(format!(
            "plugin path `{value}` escapes its package"
        )));
    }
    Ok(path)
}

fn read_bounded_file(path: &Path, max_bytes: u64, name: &str) -> Result<Vec<u8>> {
    if !std::fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(Error::Config(format!(
            "{name} is not a regular file: {}",
            path.display()
        )));
    }
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::Config(format!(
            "{name} is not a regular file: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(Error::Config(format!("{name} exceeds {max_bytes} bytes")));
    }
    Ok(bytes)
}

fn discover_roots(
    roots: impl IntoIterator<Item = PathBuf>,
    skills: &mut BTreeMap<String, Skill>,
    namespace: Option<&str>,
    keep_existing: bool,
) -> Result<()> {
    for root in roots {
        let root_path = match std::fs::canonicalize(&root) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let root = match Dir::open_ambient_dir(&root_path, ambient_authority()) {
            Ok(root) => root,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let mut directories = root
            .entries()?
            .map(|entry| entry.map(|entry| PathBuf::from(entry.file_name())))
            .collect::<std::io::Result<Vec<_>>>()?;
        directories.sort();
        for directory_path in directories {
            let directory = match root.open_dir(&directory_path) {
                Ok(directory) => directory,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let metadata = match directory.metadata(SKILL_FILE) {
                Ok(metadata) => metadata,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            if !metadata.is_file() {
                continue;
            }
            let content = read_skill_resource(&directory, Path::new(SKILL_FILE))?;
            let skill_path = root_path.join(&directory_path).join(SKILL_FILE);
            let (name, description) = skill_metadata(&skill_path, &content);
            let name = namespace.map_or(name.clone(), |namespace| format!("{namespace}:{name}"));
            if skills.contains_key(&name) {
                if keep_existing {
                    continue;
                }
                return Err(Error::Duplicate(format!("skill `{name}`")));
            }
            if skills.len() == MAX_SKILLS {
                return Err(Error::Config(format!("skill count exceeds {MAX_SKILLS}")));
            }
            skills.insert(
                name.clone(),
                Skill {
                    name,
                    description,
                    location: skill_path,
                },
            );
        }
    }
    Ok(())
}

fn prompt_value(value: &str) -> String {
    Value::String(value.into()).to_string()
}

fn installed_skill_roots() -> Vec<PathBuf> {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let codex_home = env::var_os("CODEX_HOME").map(PathBuf::from);
    installed_skill_roots_from(home, codex_home)
}

fn installed_skill_roots_from(home: Option<PathBuf>, codex_home: Option<PathBuf>) -> Vec<PathBuf> {
    let codex_home = codex_home.or_else(|| home.as_ref().map(|path| path.join(".codex")));
    let mut roots = Vec::new();
    if let Some(home) = home {
        roots.push(home.join(".agents/skills"));
    }
    if let Some(codex_home) = codex_home {
        roots.push(codex_home.join("skills"));
        roots.push(codex_home.join("skills/.system"));
    }
    #[cfg(unix)]
    roots.push(PathBuf::from("/etc/codex/skills"));
    roots
}

fn hook_input(
    session_id: &str,
    model: &str,
    approval_policy: Option<ApprovalPolicy>,
    turn_id: Option<&str>,
) -> serde_json::Map<String, Value> {
    let mut input = serde_json::Map::from_iter([
        ("session_id".into(), Value::String(session_id.into())),
        ("transcript_path".into(), Value::Null),
        ("model".into(), Value::String(model.into())),
    ]);
    if let Some(approval_policy) = approval_policy {
        input.insert(
            "permission_mode".into(),
            Value::String(hooks::permission_mode(approval_policy).into()),
        );
    }
    if let Some(turn_id) = turn_id {
        input.insert("turn_id".into(), Value::String(turn_id.into()));
    }
    input
}

fn hook_notices(outcomes: &[hooks::HookOutcome]) -> Vec<String> {
    let mut messages = Vec::new();
    for outcome in outcomes {
        for message in [&outcome.failure, &outcome.system_message]
            .into_iter()
            .flatten()
        {
            if messages.len() == MAX_HOOK_NOTICES {
                messages.push("additional extension hook notices were omitted".into());
                return messages;
            }
            messages.push(message.clone());
        }
    }
    messages
}

fn push_hook_notices(events: &mut Vec<EventMsg>, outcomes: &[hooks::HookOutcome]) {
    events.extend(
        hook_notices(outcomes)
            .into_iter()
            .map(|message| EventMsg::Warning(WarningEvent { message })),
    );
}

fn publish_hook_notices(runtime: &RuntimeContext, outcomes: &[hooks::HookOutcome]) -> Result<()> {
    for message in hook_notices(outcomes) {
        for event in
            super::MiddlewareCommandOutput::render(MANIFEST.id, message, FrontendTone::Warning)
                .events
        {
            (runtime.frontend)(event)?;
        }
    }
    Ok(())
}

fn hook_context(outcomes: &[hooks::HookOutcome]) -> Option<String> {
    let context = outcomes
        .iter()
        .filter_map(|outcome| outcome.additional_context.as_deref())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!context.is_empty()).then(|| truncate_utf8(&context, MAX_HOOK_CONTEXT_BYTES).into())
}

fn hook_stop_reason(outcome: &hooks::HookOutcome) -> String {
    [
        outcome.reason.as_deref(),
        outcome.stop_reason.as_deref(),
        outcome.additional_context.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find(|value| !value.trim().is_empty())
    .unwrap_or("stopped by extension hook")
    .into()
}

impl Middleware for Extensions {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn prompt_section(&self, _runtime: &super::RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(self.section())
    }

    fn session_start<'a>(
        &'a self,
        context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let source = match context.source() {
                SessionStartSource::Startup => "startup",
                SessionStartSource::Resume => "resume",
                SessionStartSource::Compact => "compact",
            };
            let (event, input, subjects) = match &context.runtime.role {
                AgentRole::Main => {
                    let mut input = hook_input(
                        &context.runtime.session_id,
                        &context.runtime.model,
                        Some(context.runtime.approval_policy),
                        None,
                    );
                    input.insert("source".into(), Value::String(source.into()));
                    (hooks::HookEvent::SessionStart, input, vec![source])
                }
                AgentRole::Subagent { .. } if context.source() == SessionStartSource::Compact => {
                    return Ok(());
                }
                AgentRole::Subagent {
                    parent_session_id,
                    parent_turn_id,
                } => {
                    let mut input = hook_input(
                        parent_session_id,
                        &context.runtime.model,
                        Some(context.runtime.approval_policy),
                        Some(parent_turn_id),
                    );
                    input.extend([
                        (
                            "agent_id".into(),
                            Value::String(context.runtime.session_id.clone()),
                        ),
                        ("agent_type".into(), Value::String("subagent".into())),
                    ]);
                    (hooks::HookEvent::SubagentStart, input, vec!["subagent"])
                }
            };
            let outcomes = self.run_hooks(event, Value::Object(input), &subjects).await;
            publish_hook_notices(context.runtime, &outcomes)?;
            context.retain_input(|item| {
                internal_message_kind(item) != Some(SESSION_HOOK_CONTEXT_KIND)
            });
            if let Some(additional) = hook_context(&outcomes) {
                context.push_input(internal_user_message(
                    SESSION_HOOK_CONTEXT_KIND,
                    &additional,
                ));
            }
            if let Some(outcome) = outcomes
                .iter()
                .find(|outcome| outcome.continue_session == Some(false))
            {
                context.stop(hook_stop_reason(outcome))?;
            }
            Ok(())
        })
    }

    fn user_prompt_submit<'a>(
        &'a self,
        context: &'a mut UserPromptSubmitContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut input = hook_input(
                context.turn.session_id,
                context.turn.model,
                Some(context.turn.approval_policy),
                Some(context.turn.turn_id),
            );
            input.extend([("prompt".into(), Value::String(context.message.into()))]);
            let outcomes = self
                .run_hooks(
                    hooks::HookEvent::UserPromptSubmit,
                    Value::Object(input),
                    &[],
                )
                .await;
            push_hook_notices(context.events, &outcomes);
            if let Some(outcome) = outcomes.iter().find(|outcome| {
                outcome.decision == Some(hooks::HookDecision::Block)
                    || outcome.continue_session == Some(false)
            }) {
                context.reject(hook_stop_reason(outcome))?;
            } else if let Some(additional) = hook_context(&outcomes) {
                context.push_input(internal_user_message("extension_prompt_hook", &additional));
            }
            Ok(())
        })
    }

    fn pre_tool_use<'a>(
        &'a self,
        context: &'a mut PreToolUseContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let original_name = context.call().name.clone();
            let tool = context.tools.hook_tool(context.call(), None);
            let mut input = hook_input(
                context.turn.session_id,
                context.turn.model,
                Some(context.turn.approval_policy),
                Some(context.turn.turn_id),
            );
            input.extend([
                ("tool_name".into(), Value::String(tool.name)),
                (
                    "tool_use_id".into(),
                    Value::String(context.call().call_id.clone()),
                ),
                ("tool_input".into(), tool.input),
            ]);
            let subjects = tool.subjects.iter().map(String::as_str).collect::<Vec<_>>();
            let outcomes = self
                .run_hooks(
                    hooks::HookEvent::PreToolUse,
                    Value::Object(input),
                    &subjects,
                )
                .await;
            push_hook_notices(context.events, &outcomes);
            if let Some(additional) = hook_context(&outcomes) {
                context.push_input(internal_user_message("extension_tool_hook", &additional));
            }
            if let Some(outcome) = outcomes.iter().find(|outcome| {
                outcome.permission_decision == Some(hooks::PermissionDecision::Deny)
                    || outcome.decision == Some(hooks::HookDecision::Block)
            }) {
                return context.deny(hook_stop_reason(outcome));
            }
            let mut rewrites = outcomes
                .iter()
                .filter_map(|outcome| outcome.updated_input.clone());
            let Some(rewrite) = rewrites.next() else {
                return Ok(());
            };
            if rewrites.any(|candidate| candidate != rewrite) {
                return context.deny("conflicting extension hook tool rewrites");
            }
            match context
                .tools
                .rewrite_hook_input(&original_name, rewrite)
                .and_then(|arguments| context.replace(original_name, arguments))
            {
                Ok(()) => Ok(()),
                Err(error) => {
                    context.events.push(EventMsg::Warning(WarningEvent {
                        message: error.to_string(),
                    }));
                    Ok(())
                }
            }
        })
    }

    fn permission_request<'a>(
        &'a self,
        context: &'a mut PermissionRequestContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut all_allowed = !context.requested_call_ids.is_empty();
            for call_id in context.requested_call_ids {
                let call = context
                    .calls
                    .iter()
                    .find(|call| &call.call_id == call_id)
                    .ok_or_else(|| Error::Config("approval hook call is missing".into()))?;
                let tool = context.tools.hook_tool(call, Some(context.reason));
                let mut input = hook_input(
                    context.turn.session_id,
                    context.turn.model,
                    Some(context.turn.approval_policy),
                    Some(context.turn.turn_id),
                );
                input.extend([
                    ("tool_name".into(), Value::String(tool.name)),
                    ("tool_input".into(), tool.input),
                ]);
                let subjects = tool.subjects.iter().map(String::as_str).collect::<Vec<_>>();
                let outcomes = self
                    .run_hooks(
                        hooks::HookEvent::PermissionRequest,
                        Value::Object(input),
                        &subjects,
                    )
                    .await;
                push_hook_notices(context.events, &outcomes);
                if let Some(outcome) = outcomes.iter().find(|outcome| {
                    outcome.permission_decision == Some(hooks::PermissionDecision::Deny)
                }) {
                    return context.deny(hook_stop_reason(outcome));
                }
                all_allowed &= outcomes.iter().any(|outcome| {
                    outcome.permission_decision == Some(hooks::PermissionDecision::Allow)
                });
            }
            if all_allowed {
                context.allow();
            }
            Ok(())
        })
    }

    fn post_tool_use<'a>(
        &'a self,
        context: &'a mut PostToolUseContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let tool = context.tools.hook_tool(context.call, None);
            let mut input = hook_input(
                context.turn.session_id,
                context.turn.model,
                Some(context.turn.approval_policy),
                Some(context.turn.turn_id),
            );
            input.extend([
                ("tool_name".into(), Value::String(tool.name)),
                (
                    "tool_use_id".into(),
                    Value::String(context.call.call_id.clone()),
                ),
                ("tool_input".into(), tool.input),
                (
                    "tool_response".into(),
                    Value::String(context.result().output.clone()),
                ),
            ]);
            let subjects = tool.subjects.iter().map(String::as_str).collect::<Vec<_>>();
            let outcomes = self
                .run_hooks(
                    hooks::HookEvent::PostToolUse,
                    Value::Object(input),
                    &subjects,
                )
                .await;
            push_hook_notices(context.events, &outcomes);
            if let Some(outcome) = outcomes.iter().find(|outcome| {
                outcome.decision == Some(hooks::HookDecision::Block)
                    || outcome.continue_session == Some(false)
            }) {
                context.replace(hook_stop_reason(outcome));
            }
            if let Some(additional) = hook_context(&outcomes) {
                context.push_input(internal_user_message("extension_tool_hook", &additional));
            }
            Ok(())
        })
    }

    fn pre_compact<'a>(&'a self, context: &'a mut CompactContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.run_compact_hook(hooks::HookEvent::PreCompact, context)
                .await
        })
    }

    fn post_compact<'a>(
        &'a self,
        context: &'a mut CompactContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.run_compact_hook(hooks::HookEvent::PostCompact, context)
                .await
        })
    }

    fn stop<'a>(&'a self, context: &'a mut StopContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let (event, mut input, subjects) = match context.role() {
                AgentRole::Main => (
                    hooks::HookEvent::Stop,
                    hook_input(
                        context.turn.session_id,
                        context.turn.model,
                        Some(context.turn.approval_policy),
                        Some(context.turn.turn_id),
                    ),
                    Vec::new(),
                ),
                AgentRole::Subagent {
                    parent_session_id,
                    parent_turn_id,
                } => {
                    let mut input = hook_input(
                        parent_session_id,
                        context.turn.model,
                        Some(context.turn.approval_policy),
                        Some(parent_turn_id),
                    );
                    input.extend([
                        (
                            "agent_id".into(),
                            Value::String(context.turn.session_id.into()),
                        ),
                        ("agent_type".into(), Value::String("subagent".into())),
                        ("agent_transcript_path".into(), Value::Null),
                    ]);
                    (hooks::HookEvent::SubagentStop, input, vec!["subagent"])
                }
            };
            input.extend([
                (
                    "stop_hook_active".into(),
                    Value::Bool(context.stop_hook_active()),
                ),
                (
                    "last_assistant_message".into(),
                    context
                        .last_assistant_message()
                        .map_or(Value::Null, |message| Value::String(message.into())),
                ),
            ]);
            let outcomes = self.run_hooks(event, Value::Object(input), &subjects).await;
            push_hook_notices(context.events, &outcomes);
            if outcomes
                .iter()
                .any(|outcome| outcome.continue_session == Some(false))
            {
                return Ok(());
            }
            if let Some(outcome) = outcomes
                .iter()
                .find(|outcome| outcome.decision == Some(hooks::HookDecision::Block))
            {
                if context.stop_hook_active() {
                    context.events.push(EventMsg::Warning(WarningEvent {
                        message: "extension stop hook cannot continue a turn twice".into(),
                    }));
                } else {
                    context.continue_with(hook_stop_reason(outcome))?;
                }
            }
            Ok(())
        })
    }

    fn session_end<'a>(&'a self, runtime: &'a RuntimeContext) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if runtime.role == AgentRole::Main {
                let mut input = hook_input(&runtime.session_id, &runtime.model, None, None);
                input.insert("reason".into(), Value::String("other".into()));
                let outcomes = self
                    .run_hooks(
                        hooks::HookEvent::SessionEnd,
                        Value::Object(input),
                        &["other"],
                    )
                    .await;
                publish_hook_notices(runtime, &outcomes)?;
            }
            Ok(())
        })
    }

    fn frontend(&self) -> FrontendContribution {
        let count = self.plugins.len()
            + self
                .skills
                .keys()
                .filter(|name| {
                    name.split_once(':')
                        .is_none_or(|(plugin, _)| !self.plugins.contains(plugin))
                })
                .count();
        FrontendContribution {
            capability: self.name().into(),
            accepts_file_attachments: false,
            count: Some(count),
            commands: Vec::new(),
            widgets: vec![FrontendWidget {
                id: "count".into(),
                slot: FrontendSlot::Header,
                text: format!("extensions {count}"),
                tone: FrontendTone::Neutral,
                symbol: None,
                icon_only: false,
                progress: None,
                content: None,
                action: None,
            }],
            references: self
                .skills
                .values()
                .map(|skill| FrontendReference {
                    trigger: '$',
                    value: skill.name.clone(),
                    description: skill.description.clone(),
                })
                .collect(),
            active_input: None,
        }
    }
}

fn read_skill_resource(directory: &Dir, path: &Path) -> Result<String> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(unavailable_skill_resource());
    }
    // Avoid blocking on static special files, then verify the opened handle again.
    if !directory
        .metadata(path)
        .map_err(|_| unavailable_skill_resource())?
        .is_file()
    {
        return Err(unavailable_skill_resource());
    }
    let file = directory
        .open(path)
        .map_err(|_| unavailable_skill_resource())?;
    if !file
        .metadata()
        .map_err(|_| unavailable_skill_resource())?
        .is_file()
    {
        return Err(unavailable_skill_resource());
    }
    let mut bytes = Vec::new();
    file.take(MAX_SKILL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable_skill_resource())?;
    if bytes.len() as u64 > MAX_SKILL_BYTES {
        return Err(Error::Tool(format!(
            "skill resource exceeds {MAX_SKILL_BYTES} bytes"
        )));
    }
    String::from_utf8(bytes).map_err(|_| Error::Tool("skill resource is not valid UTF-8".into()))
}

fn unavailable_skill_resource() -> Error {
    Error::Tool("skill resource is unavailable".into())
}

fn skill_metadata(path: &std::path::Path, content: &str) -> (String, String) {
    let fallback = path
        .parent()
        .and_then(std::path::Path::file_name)
        .map_or_else(
            || "skill".into(),
            |name| name.to_string_lossy().into_owned(),
        );
    let frontmatter = content
        .strip_prefix("---\n")
        .and_then(|content| content.split("\n---").next());
    let name = frontmatter.and_then(|value| frontmatter_value(value, "name"));
    let description = frontmatter.and_then(|value| frontmatter_value(value, "description"));
    (
        name.filter(|value| !value.is_empty()).unwrap_or(fallback),
        description
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| text::FALLBACK_SKILL_DESCRIPTION.into())
            .chars()
            .take(500)
            .collect(),
    )
}

fn frontmatter_value(frontmatter: &str, key: &str) -> Option<String> {
    let lines = frontmatter.lines().collect::<Vec<_>>();
    let (index, value) = lines.iter().enumerate().find_map(|(index, line)| {
        line.strip_prefix(key)
            .and_then(|line| line.strip_prefix(':'))
            .map(|value| (index, value.trim()))
    })?;
    if !matches!(value, ">" | ">-" | ">+" | "|" | "|-" | "|+") {
        return Some(unquote(value));
    }
    let literal = value.starts_with('|');
    let values = lines[index + 1..]
        .iter()
        .take_while(|line| line.is_empty() || line.starts_with(' ') || line.starts_with('\t'))
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    Some(if literal {
        values.join("\n")
    } else {
        values.join(" ")
    })
}

fn unquote(value: &str) -> String {
    value
        .trim()
        .trim_matches(|character| character == '"' || character == '\'')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn trusted_hooks() -> Option<HookAuthorization> {
        Some(Arc::new(|launch| launch()))
    }

    #[test]
    fn prompt_section_is_absent_without_skills() {
        let temporary = tempfile::tempdir().expect("temporary skills");
        let extensions =
            Extensions::discover([temporary.path().to_path_buf()]).expect("empty skills");

        assert_eq!(extensions.section(), None);
    }

    #[test]
    fn prompt_section_advertises_locations_in_skill_name_order() {
        let temporary = tempfile::tempdir().expect("temporary skills");
        let root = temporary.path().join("skills");
        write_skill(&root, "second", "zebra", "Last alphabetically");
        write_skill(&root, "first", "alpha", "First alphabetically");
        let root = std::fs::canonicalize(root).expect("canonical skills");
        let extensions = Extensions::discover([root.clone()]).expect("skills");
        let alpha = root.join("first/SKILL.md");
        let zebra = root.join("second/SKILL.md");

        assert_eq!(
            extensions.section(),
            Some(PromptSection::new(format!(
                "{}\n\n- name: \"alpha\"\n  description: \"First alphabetically\"\n  location: {}\n- name: \"zebra\"\n  description: \"Last alphabetically\"\n  location: {}",
                text::PROMPT_DEFAULT,
                prompt_value(&alpha.display().to_string()),
                prompt_value(&zebra.display().to_string())
            )))
        );
        assert_eq!(
            extensions.resource_roots(),
            vec![root.join("first"), root.join("second")]
        );
    }

    #[test]
    fn blank_hook_stop_fields_use_the_default_reason() {
        for outcome in [
            hooks::HookOutcome {
                reason: Some(" \n".into()),
                ..Default::default()
            },
            hooks::HookOutcome {
                stop_reason: Some("\t".into()),
                ..Default::default()
            },
            hooks::HookOutcome {
                additional_context: Some("  ".into()),
                ..Default::default()
            },
        ] {
            assert_eq!(hook_stop_reason(&outcome), "stopped by extension hook");
        }
    }

    #[test]
    fn duplicate_skill_names_are_rejected() {
        let temporary = tempfile::tempdir().expect("temporary skills");
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        write_skill(&first, "shared", "shared", "first");
        write_skill(&second, "shared", "shared", "second");

        let error = match Extensions::discover([first, second]) {
            Ok(_) => panic!("duplicate skill was accepted"),
            Err(error) => error,
        };

        assert!(matches!(error, Error::Duplicate(message) if message == "skill `shared`"));
    }

    #[test]
    fn installed_skills_do_not_replace_explicit_skills() {
        let temporary = tempfile::tempdir().expect("temporary skills");
        let explicit = temporary.path().join("explicit");
        let installed = temporary.path().join("installed");
        write_skill(&explicit, "shared", "shared", "explicit");
        write_skill(&installed, "shared", "shared", "installed");
        write_skill(&installed, "global", "global", "installed");
        let mut discovered = Extensions::discover([explicit]).expect("explicit skills");

        discover_roots([installed], &mut discovered.skills, None, true).expect("installed skills");

        assert_eq!(
            discovered
                .skills
                .iter()
                .map(|(name, skill)| (name.as_str(), skill.description.as_str()))
                .collect::<Vec<_>>(),
            vec![("global", "installed"), ("shared", "explicit")]
        );
    }

    #[test]
    fn activated_plugin_namespaces_bundled_skills_and_folds_descriptions() {
        use crate::backend::sandbox::local::LocalSandbox;

        let temporary = tempfile::tempdir().expect("temporary extensions");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let plugin = temporary.path().join("ponytail");
        std::fs::create_dir_all(plugin.join(".codex-plugin")).expect("plugin manifest directory");
        std::fs::write(
            plugin.join(".codex-plugin/plugin.json"),
            r#"{
                "name": "ponytail",
                "version": "4.9.0",
                "description": "Minimal coding workflows",
                "skills": "./skills/"
            }"#,
        )
        .expect("plugin manifest");
        let skill = plugin.join("skills/review");
        std::fs::create_dir_all(&skill).expect("plugin skill");
        std::fs::write(
            skill.join(SKILL_FILE),
            "---\nname: review\ndescription: >\n  Find unnecessary abstractions and\n  remove them.\n---\n",
        )
        .expect("plugin skill manifest");
        let backend = Arc::new(LocalSandbox::new(&workspace).expect("sandbox"));

        let extensions = Extensions::discover(Vec::<PathBuf>::new())
            .expect("extensions")
            .activate_plugins([(plugin, trusted_hooks())], &workspace, backend)
            .expect("activate plugin");

        let skill = extensions
            .skills
            .get("ponytail:review")
            .expect("namespaced skill");
        assert_eq!(
            skill.description,
            "Find unnecessary abstractions and remove them."
        );
        assert_eq!(extensions.plugins, BTreeSet::from(["ponytail".into()]));
    }

    #[test]
    fn untrusted_plugin_keeps_bundled_skills_without_loading_hooks() {
        use crate::backend::sandbox::local::LocalSandbox;

        let temporary = tempfile::tempdir().expect("temporary extensions");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let plugin = temporary.path().join("ponytail");
        std::fs::create_dir_all(plugin.join(".codex-plugin")).expect("manifest directory");
        std::fs::create_dir_all(plugin.join("skills/review")).expect("plugin skill");
        std::fs::create_dir_all(plugin.join("hooks")).expect("hooks directory");
        std::fs::write(
            plugin.join(".codex-plugin/plugin.json"),
            r#"{"name":"ponytail","skills":"./skills","hooks":"./hooks/hooks.json"}"#,
        )
        .expect("plugin manifest");
        std::fs::write(
            plugin.join("skills/review/SKILL.md"),
            "---\nname: review\ndescription: Review code.\n---\n",
        )
        .expect("skill manifest");
        std::fs::write(
            plugin.join("hooks/hooks.json"),
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"true"}]}]}}"#,
        )
        .expect("hook manifest");
        let backend = Arc::new(LocalSandbox::new(&workspace).expect("sandbox"));

        let extensions = Extensions::discover(Vec::<PathBuf>::new())
            .expect("extensions")
            .activate_plugins([(plugin, None)], &workspace, backend)
            .expect("activate plugin skills");

        assert!(extensions.skills.contains_key("ponytail:review"));
        assert!(extensions.hooks.is_empty());
    }

    #[tokio::test]
    async fn ponytail_shaped_session_hook_adds_hidden_context() {
        use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
        use crate::backend::sandbox::local::LocalSandbox;
        use crate::protocol::SessionContext;

        let temporary = tempfile::tempdir_in(std::env::current_dir().expect("current directory"))
            .expect("temporary extensions");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let plugin = temporary.path().join("ponytail");
        std::fs::create_dir_all(plugin.join(".codex-plugin")).expect("manifest directory");
        std::fs::create_dir_all(plugin.join("hooks")).expect("hooks directory");
        std::fs::write(
            plugin.join(".codex-plugin/plugin.json"),
            r#"{"name":"ponytail","version":"4.9.0","hooks":"./hooks/hooks.json"}"#,
        )
        .expect("plugin manifest");
        std::fs::write(
            plugin.join("hooks/hooks.json"),
            r#"{"description":"Ponytail activation","hooks":{"SessionStart":[{"matcher":"startup|resume|compact","hooks":[{"type":"command","command":"sh \"${CLAUDE_PLUGIN_ROOT}/hooks/activate.sh\"","timeout":5}]}]}}"#,
        )
        .expect("hook manifest");
        std::fs::write(
            plugin.join("hooks/activate.sh"),
            r#"#!/bin/sh
payload=$(cat)
printf '%s' "$payload" | grep -q '"source":"startup"' || exit 1
printf '%s' "$payload" | grep -q '"model":"model"' || exit 1
printf '%s' "$payload" | grep -q '"permission_mode":"default"' || exit 1
printf '%s\n' '{"systemMessage":"PONYTAIL:FULL","hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"Ponytail rules active."}}'
"#,
        )
        .expect("hook script");

        let backend = Arc::new(LocalSandbox::new(&workspace).expect("sandbox"));
        let extensions = Extensions::discover(Vec::<PathBuf>::new())
            .expect("extensions")
            .activate_plugins([(plugin, trusted_hooks())], &workspace, backend)
            .expect("activate plugin");
        let notices = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&notices);
        let runtime = RuntimeContext {
            checkpoints: Arc::new(
                SqliteCheckpoint::new(temporary.path().join("checkpoints.sqlite3"))
                    .expect("checkpoints"),
            ),
            session_id: "session".into(),
            model_route: "model".into(),
            model: "model".into(),
            approval_policy: ApprovalPolicy::Ask,
            session_context: SessionContext::default(),
            metadata: BTreeMap::new(),
            role: AgentRole::Main,
            frontend: Arc::new(move |event| {
                captured.lock().expect("notices").push(event);
                Ok(())
            }),
        };
        let mut input = Vec::new();
        let mut context = SessionStartContext {
            runtime: &runtime,
            source: SessionStartSource::Startup,
            queued_input: Default::default(),
            input: &mut input,
            input_changed: false,
            stop_reason: None,
        };

        extensions
            .session_start(&mut context)
            .await
            .expect("session hook");

        assert_eq!(
            input
                .iter()
                .filter(|item| {
                    crate::protocol::internal_message_kind(item) == Some(SESSION_HOOK_CONTEXT_KIND)
                })
                .count(),
            1
        );
        assert!(input[0].to_string().contains("Ponytail rules active."));
        assert_eq!(notices.lock().expect("notices").len(), 1);
    }

    #[test]
    fn agent_plugin_rejects_mcp_even_with_a_portable_skill() {
        let temporary = tempfile::tempdir().expect("temporary extensions");
        let plugin = temporary.path().join("remote");
        std::fs::create_dir(&plugin).expect("plugin directory");
        std::fs::write(
            plugin.join("plugin.json"),
            r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"remote"}"#,
        )
        .expect("plugin manifest");
        write_skill(&plugin, "skills/portable", "portable", "Portable skill");
        std::fs::write(plugin.join("mcp.json"), b"{}").expect("MCP component");

        let error = inspect_package(plugin).expect_err("MCP must be unsupported");

        assert!(error.to_string().contains("unsupported MCP contribution"));
    }

    #[test]
    fn codex_plugin_rejects_a_root_mcp_component() {
        let temporary = tempfile::tempdir().expect("temporary extensions");
        let plugin = temporary.path().join("remote");
        std::fs::create_dir_all(plugin.join(".codex-plugin")).expect("plugin manifest directory");
        std::fs::write(
            plugin.join(".codex-plugin/plugin.json"),
            r#"{"name":"remote","skills":"./skills"}"#,
        )
        .expect("plugin manifest");
        write_skill(&plugin, "skills/portable", "portable", "Portable skill");
        std::fs::write(plugin.join(".mcp.json"), b"{}").expect("MCP component");

        let error = inspect_package(plugin).expect_err("MCP must be unsupported");

        assert!(error.to_string().contains("unsupported MCP contribution"));
    }

    #[test]
    fn schema_declared_agent_plugin_wins_over_legacy_manifest() {
        let temporary = tempfile::tempdir().expect("temporary extensions");
        let plugin = temporary.path().join("remote");
        std::fs::create_dir_all(plugin.join(".codex-plugin")).expect("plugin manifest directory");
        std::fs::write(
            plugin.join(".codex-plugin/plugin.json"),
            r#"{"name":"legacy","skills":"./skills"}"#,
        )
        .expect("legacy manifest");
        std::fs::write(
            plugin.join("plugin.json"),
            r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"remote"}"#,
        )
        .expect("Agent Plugin manifest");
        write_skill(&plugin, "skills/portable", "portable", "Portable skill");

        let package = inspect_package(plugin).expect("Agent Plugin package");
        assert_eq!(package.name, "remote");
    }

    #[test]
    fn unsupported_agent_manifest_never_falls_back_to_legacy() {
        let temporary = tempfile::tempdir().expect("temporary extensions");
        let plugin = temporary.path().join("remote");
        std::fs::create_dir_all(plugin.join(".codex-plugin")).expect("plugin manifest directory");
        std::fs::write(
            plugin.join(".codex-plugin/plugin.json"),
            r#"{"name":"legacy","skills":"./skills"}"#,
        )
        .expect("legacy manifest");
        std::fs::write(
            plugin.join("plugin.json"),
            r#"{"$schema":"https://example.com/unsupported.json","name":"remote"}"#,
        )
        .expect("unsupported Agent Plugin manifest");
        write_skill(&plugin, "skills/portable", "portable", "Portable skill");

        let error = inspect_package(plugin).expect_err("unsupported schema must fail closed");

        assert!(
            error
                .to_string()
                .contains("unsupported Agent Plugin schema")
        );
    }

    #[test]
    fn plugin_snapshot_cannot_share_the_writable_workspace() {
        use crate::backend::sandbox::local::LocalSandbox;

        let workspace = tempfile::tempdir().expect("workspace");
        let plugin = workspace.path().join("plugin");
        std::fs::create_dir(&plugin).expect("plugin");
        let backend = Arc::new(LocalSandbox::new(workspace.path()).expect("sandbox"));

        let error = Extensions::discover(Vec::<PathBuf>::new())
            .expect("extensions")
            .activate_plugins([(plugin, trusted_hooks())], workspace.path(), backend)
            .err()
            .expect("overlapping plugin must fail");

        assert!(error.to_string().contains("must not overlap"));
    }

    #[cfg(unix)]
    #[test]
    fn plugin_data_root_rejects_symlink_without_writing_outside() {
        use std::os::unix::fs::symlink;

        use crate::backend::sandbox::local::LocalSandbox;

        let temporary = tempfile::tempdir().expect("temporary extensions");
        let workspace = temporary.path().join("workspace");
        let outside = temporary.path().join("outside");
        let plugin = temporary.path().join("plugin");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::create_dir(&outside).expect("outside directory");
        std::fs::create_dir(&plugin).expect("plugin");
        symlink(&outside, workspace.join(".mobius")).expect("symlink data root");
        let backend = Arc::new(LocalSandbox::new(&workspace).expect("sandbox"));

        let result = Extensions::discover(Vec::<PathBuf>::new())
            .expect("extensions")
            .activate_plugins([(plugin, trusted_hooks())], &workspace, backend);

        assert!(result.is_err());
        assert!(!outside.join("extensions").exists());
    }

    #[test]
    fn non_directory_entries_are_ignored() {
        let temporary = tempfile::tempdir().expect("temporary skills");
        let root = temporary.path().join("skills");
        write_skill(&root, "valid", "valid", "valid");
        std::fs::write(root.join(".installed"), "").expect("write marker");

        let discovered = Extensions::discover([root]).expect("discover skills");

        assert_eq!(discovered.skills.keys().collect::<Vec<_>>(), vec!["valid"]);
    }

    #[test]
    fn skill_resource_rejects_parent_escape() {
        let temporary = tempfile::tempdir().expect("temporary skills");
        let skill = temporary.path().join("skill");
        std::fs::create_dir(&skill).expect("create skill");
        std::fs::write(temporary.path().join("outside.md"), "outside").expect("write outside");
        let directory = Dir::open_ambient_dir(&skill, ambient_authority()).expect("open skill");

        assert!(read_skill_resource(&directory, Path::new("../outside.md")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn skill_resource_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary skills");
        let skill = temporary.path().join("skill");
        std::fs::create_dir(&skill).expect("create skill");
        let outside = temporary.path().join("outside.md");
        std::fs::write(&outside, "outside").expect("write outside");
        symlink(outside, skill.join("escape.md")).expect("create escape");
        let directory = Dir::open_ambient_dir(&skill, ambient_authority()).expect("open skill");

        assert!(read_skill_resource(&directory, Path::new("escape.md")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn discovery_rejects_skill_directory_symlink_escape() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary skills");
        let root = temporary.path().join("root");
        let outside = temporary.path().join("outside");
        std::fs::create_dir(&root).expect("create root");
        write_skill(&outside, "escaped", "escaped", "escaped");
        symlink(outside.join("escaped"), root.join("escaped")).expect("create escape");

        assert!(Extensions::discover([root]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn package_inspection_rejects_symlinked_manifest_directory() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary plugin");
        let plugin = temporary.path().join("plugin");
        let outside = temporary.path().join("outside");
        std::fs::create_dir(&plugin).expect("plugin directory");
        std::fs::create_dir(&outside).expect("outside directory");
        std::fs::write(
            outside.join("plugin.json"),
            r#"{"name":"escaped","hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"true"}]}]}}"#,
        )
        .expect("outside manifest");
        symlink(&outside, plugin.join(".codex-plugin")).expect("manifest symlink");

        let error = inspect_package(&plugin).expect_err("manifest symlink must be rejected");

        assert!(error.to_string().contains("contains a symlink"));
    }

    fn write_skill(root: &std::path::Path, directory: &str, name: &str, description: &str) {
        let path = root.join(directory);
        std::fs::create_dir_all(&path).expect("create skill directory");
        std::fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n"),
        )
        .expect("write skill");
    }
}
