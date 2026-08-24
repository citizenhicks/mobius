use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

use super::ExtensionPackage;
use super::ExtensionPackageKind;
use super::MAX_PLUGIN_ID_BYTES;
use super::MAX_SKILL_BYTES;
use super::SKILL_FILE;
use super::Skill;
use super::confined_path;
use super::discover_roots;
use super::hooks;
use super::read_bounded_file;
use super::skill_metadata;
use super::valid_package_name;
use crate::Error;
use crate::Result;

const MAX_PLUGIN_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_PLUGIN_VERSION_BYTES: usize = 128;
const OPENAI_PLUGIN_MANIFEST: &str = ".codex-plugin/plugin.json";
const AGENT_PLUGIN_MANIFEST: &str = "plugin.json";
const MCP_COMPONENTS: [&str; 2] = [".mcp.json", "mcp.json"];
const AGENT_PLUGIN_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";

pub(super) struct LoadedPackage {
    pub(super) root: PathBuf,
    pub(super) metadata: ExtensionPackage,
    pub(super) skills: BTreeMap<String, Skill>,
    pub(super) hooks: Option<hooks::HookDefinitions>,
}

pub(super) fn load(root: PathBuf) -> Result<LoadedPackage> {
    match agent_plugin_schema(&root)? {
        Some(schema) if schema == AGENT_PLUGIN_SCHEMA => load_agent_plugin(root),
        Some(schema) => Err(Error::Config(format!(
            "unsupported Agent Plugin schema `{schema}`"
        ))),
        None if root.join(OPENAI_PLUGIN_MANIFEST).exists() => load_openai_plugin(root),
        None => load_skill(root),
    }
}

fn load_openai_plugin(root: PathBuf) -> Result<LoadedPackage> {
    let manifest = load_openai_plugin_manifest(&root)?;
    reject_mcp_components(&root, &manifest.name)?;
    let mut skills = BTreeMap::new();
    discover_openai_plugin_skills(&root, &manifest, &mut skills)?;
    let hooks = hooks::HookDefinitions::load(&root, manifest.hooks.as_ref())?;
    let inspected_hooks = hooks.inspect();
    if skills.is_empty() && inspected_hooks.is_empty() {
        return Err(Error::Config(format!(
            "plugin `{}` has no supported contributions",
            manifest.name
        )));
    }
    Ok(LoadedPackage {
        root,
        metadata: ExtensionPackage {
            kind: ExtensionPackageKind::Plugin,
            name: manifest.name,
            version: manifest.version,
            description: manifest.description.unwrap_or_default(),
            skills: skills.keys().cloned().collect(),
            hooks: inspected_hooks,
        },
        skills,
        hooks: Some(hooks),
    })
}

fn load_agent_plugin(root: PathBuf) -> Result<LoadedPackage> {
    let manifest = load_agent_plugin_manifest(&root)?;
    reject_mcp_components(&root, &manifest.name)?;
    let mut skills = BTreeMap::new();
    discover_agent_plugin_skills(&root, &manifest.name, &mut skills)?;
    if skills.is_empty() {
        return Err(Error::Config(format!(
            "plugin `{}` has no supported contributions",
            manifest.name
        )));
    }
    Ok(LoadedPackage {
        root,
        metadata: ExtensionPackage {
            kind: ExtensionPackageKind::Plugin,
            name: manifest.name,
            version: manifest.version,
            description: manifest.description.unwrap_or_default(),
            skills: skills.keys().cloned().collect(),
            hooks: Vec::new(),
        },
        skills,
        hooks: None,
    })
}

fn load_skill(root: PathBuf) -> Result<LoadedPackage> {
    let path = root.join(SKILL_FILE);
    let content = String::from_utf8(read_bounded_file(&path, MAX_SKILL_BYTES, "skill manifest")?)
        .map_err(|_| Error::Config("skill manifest is not valid UTF-8".into()))?;
    let (name, description) = skill_metadata(&path, &content);
    if !valid_openai_package_name(&name) {
        return Err(Error::Config(format!(
            "skill name `{name}` must be kebab-case"
        )));
    }
    let skills = BTreeMap::from([(
        name.clone(),
        Skill {
            name: name.clone(),
            description: description.clone(),
            location: path,
        },
    )]);
    Ok(LoadedPackage {
        root,
        metadata: ExtensionPackage {
            kind: ExtensionPackageKind::Skill,
            skills: vec![name.clone()],
            name,
            version: None,
            description,
            hooks: Vec::new(),
        },
        skills,
        hooks: None,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenAiPluginManifest {
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

impl OpenAiPluginManifest {
    fn validate(&self) -> Result<()> {
        if !valid_openai_package_name(&self.name) {
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

fn load_openai_plugin_manifest(root: &Path) -> Result<OpenAiPluginManifest> {
    let manifest_path = confined_path(root, OPENAI_PLUGIN_MANIFEST, false)?;
    let manifest: OpenAiPluginManifest = serde_json::from_slice(&read_bounded_file(
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

fn discover_openai_plugin_skills(
    root: &Path,
    manifest: &OpenAiPluginManifest,
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

fn valid_openai_package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PLUGIN_ID_BYTES
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

pub(super) fn canonical_root(path: &Path) -> Result<PathBuf> {
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(Error::Config(format!(
            "plugin root cannot be a symlink: {}",
            path.display()
        )));
    }
    super::canonical_directory(path, "plugin root")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::extensions::inspect_package;

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
    fn openai_plugin_rejects_a_root_mcp_component() {
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
    fn schema_declared_agent_plugin_wins_over_openai_manifest() {
        let temporary = tempfile::tempdir().expect("temporary extensions");
        let plugin = temporary.path().join("remote");
        std::fs::create_dir_all(plugin.join(".codex-plugin")).expect("plugin manifest directory");
        std::fs::write(
            plugin.join(".codex-plugin/plugin.json"),
            r#"{"name":"openai","skills":"./skills"}"#,
        )
        .expect("OpenAI plugin manifest");
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
    fn unsupported_agent_manifest_never_falls_back_to_openai_manifest() {
        let temporary = tempfile::tempdir().expect("temporary extensions");
        let plugin = temporary.path().join("remote");
        std::fs::create_dir_all(plugin.join(".codex-plugin")).expect("plugin manifest directory");
        std::fs::write(
            plugin.join(".codex-plugin/plugin.json"),
            r#"{"name":"openai","skills":"./skills"}"#,
        )
        .expect("OpenAI plugin manifest");
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

    fn write_skill(root: &Path, directory: &str, name: &str, description: &str) {
        let path = root.join(directory);
        std::fs::create_dir_all(&path).expect("create skill directory");
        std::fs::write(
            path.join(SKILL_FILE),
            format!("---\nname: {name}\ndescription: {description}\n---\n"),
        )
        .expect("write skill");
    }
}
