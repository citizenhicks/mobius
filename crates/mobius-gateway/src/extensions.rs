//! Owner-managed extension sources and immutable package snapshots.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mobius::middleware::extensions::{
    ExtensionHook, ExtensionPackageKind, HookAuthorization, MANIFEST, inspect_package,
    valid_package_name,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::process::Command;
use url::Url;

use crate::config::{ConfigStore, GatewayConfig};
use crate::wire::{ExtensionHookRecord, ExtensionKind, ExtensionRecord};
use crate::{Error, Result};

const MAX_EXTENSIONS: usize = 64;
const MAX_SOURCE_BYTES: usize = 4_096;
const MAX_REFERENCE_BYTES: usize = 256;
const MAX_SUBDIRECTORY_BYTES: usize = 1_024;
const MAX_PACKAGE_FILES: usize = 4_096;
const MAX_PACKAGE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4_096;
const GIT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExtensionSource {
    pub(crate) url: String,
    pub(crate) reference: Option<String>,
    pub(crate) subdirectory: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstalledExtension {
    pub(crate) kind: ExtensionKind,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) version: Option<String>,
    pub(crate) source: ExtensionSource,
    pub(crate) resolved_revision: String,
    pub(crate) digest: String,
    pub(crate) skills: Vec<String>,
    pub(crate) hooks: Vec<ExtensionHookRecord>,
    pub(crate) trusted_hook_digest: Option<String>,
}

pub(crate) struct StagedExtension {
    pub(crate) id: String,
    pub(crate) installed: InstalledExtension,
    pub(crate) snapshot_created: bool,
}

#[derive(Default)]
pub(crate) struct ResolvedExtensions {
    pub(crate) skill_roots: Vec<PathBuf>,
    pub(crate) plugins: Vec<ResolvedPlugin>,
}

pub(crate) struct ResolvedPlugin {
    pub(crate) id: String,
    pub(crate) digest: String,
    pub(crate) root: PathBuf,
    pub(crate) hooks_trusted: bool,
}

impl ResolvedPlugin {
    pub(crate) fn activation(
        &self,
        gateway: Arc<Mutex<GatewayConfig>>,
    ) -> (PathBuf, Option<HookAuthorization>) {
        let id = self.id.clone();
        let digest = self.digest.clone();
        let authorization = self.hooks_trusted.then(|| {
            Arc::new(move |launch: &mut dyn FnMut() -> mobius::Result<()>| {
                let Ok(config) = gateway.lock() else {
                    return Ok(());
                };
                if config
                    .installed_extensions
                    .get(&id)
                    .is_some_and(|installed| {
                        installed.digest == digest
                            && installed.trusted_hook_digest.as_deref() == Some(&digest)
                    })
                {
                    launch()?;
                }
                Ok(())
            }) as HookAuthorization
        });
        (self.root.clone(), authorization)
    }
}

#[derive(Clone)]
pub(crate) struct ExtensionStore {
    root: PathBuf,
}

impl ExtensionStore {
    pub(crate) fn new(store: &ConfigStore) -> Self {
        Self {
            root: store.extensions_path(),
        }
    }

    pub(crate) async fn stage(
        &self,
        url: &str,
        reference: Option<&str>,
        subdirectory: Option<&str>,
    ) -> Result<StagedExtension> {
        let source = ExtensionSource::parse(url, reference, subdirectory)?;
        prepare_private_directory(&self.root)?;
        let staging = tempfile::Builder::new()
            .prefix("stage-")
            .tempdir_in(&self.root)?;
        let checkout = staging.path().join("checkout");
        clone_source(&source, &checkout).await?;
        let revision = git_revision(&checkout).await?;
        let selected = confined_checkout_path(&checkout, source.subdirectory.as_deref())?;
        let package = staging.path().join("package");
        tokio::task::spawn_blocking(move || export_package(&selected, &package))
            .await
            .map_err(|error| Error::Config(format!("extension export failed: {error}")))??;
        let package = staging.path().join("package");
        let inspected = inspect_package(&package)?;
        let kind = inspected.kind.into();
        let id = extension_id(kind, &inspected.name);
        let digest = tree_digest(&package)?;
        let snapshot = self.snapshot_root(&digest);
        let parent = snapshot
            .parent()
            .ok_or_else(|| Error::Config("extension snapshot has no parent directory".into()))?;
        let created = !snapshot.exists();
        if !created {
            verify_snapshot(&snapshot, &digest)?;
        } else {
            fs::create_dir_all(parent)?;
            fs::rename(&package, &snapshot)?;
        }
        if let Err(error) = freeze_tree(parent) {
            if created {
                let _ = thaw_tree(parent);
                let _ = fs::remove_dir_all(parent);
            }
            return Err(error);
        }
        Ok(StagedExtension {
            id,
            installed: InstalledExtension {
                kind,
                name: inspected.name,
                description: inspected.description,
                version: inspected.version,
                source,
                resolved_revision: revision,
                digest,
                skills: inspected.skills,
                hooks: inspected.hooks.into_iter().map(Into::into).collect(),
                trusted_hook_digest: None,
            },
            snapshot_created: created,
        })
    }

    pub(crate) fn resolve(
        &self,
        config: &GatewayConfig,
        ids: &BTreeSet<String>,
    ) -> Result<ResolvedExtensions> {
        validate_ids(ids)?;
        let mut resolved = ResolvedExtensions::default();
        for id in ids {
            let Some(installed) = config.installed_extensions.get(id) else {
                continue;
            };
            let package = self.snapshot_root(&installed.digest);
            match installed.kind {
                ExtensionKind::Skill => resolved.skill_roots.push(
                    package
                        .parent()
                        .ok_or_else(|| Error::Config("skill snapshot has no parent".into()))?
                        .to_path_buf(),
                ),
                ExtensionKind::Plugin => resolved.plugins.push(ResolvedPlugin {
                    id: id.clone(),
                    digest: installed.digest.clone(),
                    root: package,
                    hooks_trusted: installed.hooks.is_empty()
                        || installed.trusted_hook_digest.as_deref() == Some(&installed.digest),
                }),
            }
        }
        Ok(resolved)
    }

    pub(crate) fn verify_installed_snapshots(&self, config: &GatewayConfig) -> Result<()> {
        for (id, installed) in &config.installed_extensions {
            let package = self.snapshot_root(&installed.digest);
            verify_snapshot(&package, &installed.digest)?;
            verify_installed_metadata(id, installed, &package)?;
        }
        Ok(())
    }

    pub(crate) fn remove_snapshot(&self, digest: &str) -> Result<()> {
        if !valid_digest(digest) {
            return Err(Error::Config("extension snapshot digest is invalid".into()));
        }
        let snapshots = self.root.join("snapshots");
        let directory = self.snapshot_directory(digest);
        for path in [&self.root, &snapshots, &directory] {
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                    return Err(Error::Config(format!(
                        "extension store path is not a regular directory: {}",
                        path.display()
                    )));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
        thaw_tree(&directory)?;
        fs::remove_dir_all(directory)?;
        Ok(())
    }

    pub(crate) fn prune(&self, config: &GatewayConfig) -> Result<()> {
        let snapshots = self.root.join("snapshots");
        let metadata = match fs::symlink_metadata(&snapshots) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(Error::Config(
                "extension snapshot store is not a regular directory".into(),
            ));
        }
        let retained = config
            .installed_extensions
            .values()
            .map(|extension| extension.digest.as_str())
            .collect::<BTreeSet<_>>();
        for entry in fs::read_dir(snapshots)? {
            let entry = entry?;
            let Some(digest) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if valid_digest(&digest) && !retained.contains(digest.as_str()) {
                self.remove_snapshot(&digest)?;
            }
        }
        Ok(())
    }

    fn snapshot_root(&self, digest: &str) -> PathBuf {
        self.snapshot_directory(digest).join("package")
    }

    fn snapshot_directory(&self, digest: &str) -> PathBuf {
        self.root.join("snapshots").join(digest)
    }

    #[cfg(test)]
    pub(crate) fn commit_test_snapshot(&self, package: &Path) -> Result<String> {
        prepare_private_directory(&self.root)?;
        let digest = tree_digest(package)?;
        let snapshot = self.snapshot_root(&digest);
        let parent = snapshot
            .parent()
            .ok_or_else(|| Error::Config("extension snapshot has no parent directory".into()))?;
        fs::create_dir_all(parent)?;
        fs::rename(package, &snapshot)?;
        freeze_tree(parent)?;
        Ok(digest)
    }
}

impl ExtensionSource {
    fn parse(url: &str, reference: Option<&str>, subdirectory: Option<&str>) -> Result<Self> {
        let mut url = Url::parse(url.trim())
            .map_err(|error| Error::Config(format!("invalid extension URL: {error}")))?;
        let mut reference = reference
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let mut subdirectory = subdirectory
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        if url.host_str() == Some("github.com") {
            let segments = url
                .path_segments()
                .map(|segments| segments.map(str::to_owned).collect::<Vec<_>>())
                .unwrap_or_default();
            if segments.len() >= 4 && segments[2] == "tree" {
                if reference.is_some() || subdirectory.is_some() {
                    return Err(Error::Config(
                        "a GitHub tree URL cannot be combined with ref or subdirectory fields"
                            .into(),
                    ));
                }
                reference = Some(segments[3].clone());
                let path = format!("/{}/{}", segments[0], segments[1]);
                url.set_path(&path);
                if segments.len() > 4 {
                    subdirectory = Some(segments[4..].join("/"));
                }
            }
        }
        let source = Self {
            url: url.to_string().trim_end_matches('/').to_owned(),
            reference,
            subdirectory,
        };
        source.validate()?;
        Ok(source)
    }

    fn validate(&self) -> Result<()> {
        if self.url.len() > MAX_SOURCE_BYTES || self.url.trim() != self.url {
            return Err(Error::Config("extension URL is invalid".into()));
        }
        let parsed = Url::parse(&self.url)
            .map_err(|error| Error::Config(format!("invalid extension URL: {error}")))?;
        if parsed.scheme() != "https"
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.port().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(Error::Config(
                "extension source must be a credential-free HTTPS Git URL".into(),
            ));
        }
        if self.reference.as_ref().is_some_and(|reference| {
            reference.is_empty()
                || reference.len() > MAX_REFERENCE_BYTES
                || reference.starts_with('-')
                || reference.chars().any(char::is_whitespace)
        }) {
            return Err(Error::Config("extension Git ref is invalid".into()));
        }
        if let Some(path) = self.subdirectory.as_deref() {
            validate_relative_path(path)?;
        }
        Ok(())
    }
}

pub(crate) fn records(config: &GatewayConfig) -> Vec<ExtensionRecord> {
    config
        .installed_extensions
        .iter()
        .map(|(id, installed)| ExtensionRecord {
            id: id.clone(),
            capability: MANIFEST.id.into(),
            kind: installed.kind,
            name: installed.name.clone(),
            description: installed.description.clone(),
            version: installed.version.clone(),
            source: installed.source.url.clone(),
            reference: installed.source.reference.clone(),
            subdirectory: installed.source.subdirectory.clone(),
            resolved_revision: installed.resolved_revision.clone(),
            digest: installed.digest.clone(),
            skills: installed.skills.clone(),
            hooks: installed.hooks.clone(),
            hooks_trusted: installed.hooks.is_empty()
                || installed.trusted_hook_digest.as_deref() == Some(&installed.digest),
        })
        .collect()
}

pub(crate) fn validate_ids(ids: &BTreeSet<String>) -> Result<()> {
    if ids.len() > MAX_EXTENSIONS {
        return Err(Error::Config(format!(
            "an agent may activate at most {MAX_EXTENSIONS} extensions"
        )));
    }
    for id in ids {
        let Some((kind, name)) = id.split_once(':') else {
            return Err(Error::Config(format!("invalid extension ID `{id}`")));
        };
        if !matches!(kind, "skill" | "plugin") || !valid_package_name(name) {
            return Err(Error::Config(format!("invalid extension ID `{id}`")));
        }
    }
    Ok(())
}

pub(crate) fn validate_installed(installed: &BTreeMap<String, InstalledExtension>) -> Result<()> {
    if installed.len() > MAX_EXTENSIONS {
        return Err(Error::Config(format!(
            "installed extension count exceeds {MAX_EXTENSIONS}"
        )));
    }
    let mut digests = BTreeSet::new();
    for (id, extension) in installed {
        extension.source.validate()?;
        if id != &extension_id(extension.kind, &extension.name)
            || !valid_package_name(&extension.name)
        {
            return Err(Error::Config(format!(
                "invalid installed extension ID `{id}`"
            )));
        }
        if !valid_digest(&extension.digest)
            || !valid_revision(&extension.resolved_revision)
            || extension
                .trusted_hook_digest
                .as_ref()
                .is_some_and(|digest| digest != &extension.digest)
        {
            return Err(Error::Config(format!(
                "extension `{id}` has invalid snapshot metadata"
            )));
        }
        if !digests.insert(&extension.digest) {
            return Err(Error::Config(format!(
                "extension `{id}` reuses another extension snapshot"
            )));
        }
        if extension.description.len() > 4_096
            || extension
                .version
                .as_ref()
                .is_some_and(|value| value.len() > 128)
            || extension.skills.len() > 64
            || extension.hooks.len() > 64
        {
            return Err(Error::Config(format!(
                "extension `{id}` metadata is too large"
            )));
        }
    }
    Ok(())
}

fn extension_id(kind: ExtensionKind, name: &str) -> String {
    let kind = match kind {
        ExtensionKind::Skill => "skill",
        ExtensionKind::Plugin => "plugin",
    };
    format!("{kind}:{name}")
}

impl From<ExtensionPackageKind> for ExtensionKind {
    fn from(kind: ExtensionPackageKind) -> Self {
        match kind {
            ExtensionPackageKind::Skill => Self::Skill,
            ExtensionPackageKind::Plugin => Self::Plugin,
        }
    }
}

impl From<ExtensionHook> for ExtensionHookRecord {
    fn from(hook: ExtensionHook) -> Self {
        Self {
            event: hook.event,
            matcher: hook.matcher,
            command: hook.command,
            timeout_seconds: hook.timeout_seconds,
        }
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_revision(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

async fn clone_source(source: &ExtensionSource, checkout: &Path) -> Result<()> {
    // ponytail: clone is time-bounded; use a quota-aware fetcher before accepting untrusted catalogs.
    let mut command = git_command();
    command.args(["clone", "--quiet", "--depth", "1", "--no-tags"]);
    if let Some(reference) = &source.reference {
        command.arg("--branch").arg(reference);
    }
    command.arg("--").arg(&source.url).arg(checkout);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let status = tokio::time::timeout(GIT_TIMEOUT, command.status())
        .await
        .map_err(|_| Error::Config("extension Git clone timed out".into()))??;
    if !status.success() {
        return Err(Error::Config("extension Git clone failed".into()));
    }
    Ok(())
}

async fn git_revision(checkout: &Path) -> Result<String> {
    let mut command = git_command();
    command
        .current_dir(checkout)
        .args(["rev-parse", "--verify", "HEAD"])
        .stderr(Stdio::null());
    let output = tokio::time::timeout(GIT_TIMEOUT, command.output())
        .await
        .map_err(|_| Error::Config("extension Git revision lookup timed out".into()))??;
    let revision = String::from_utf8(output.stdout)
        .map_err(|_| Error::Config("extension Git revision is not UTF-8".into()))?;
    let revision = revision.trim().to_owned();
    if !output.status.success() || !valid_revision(&revision) {
        return Err(Error::Config("extension Git revision is invalid".into()));
    }
    Ok(revision)
}

fn git_command() -> Command {
    let mut command = Command::new("git");
    command
        .kill_on_drop(true)
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("credential.helper=");
    for name in [
        "PATH",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "NO_PROXY",
        "https_proxy",
        "http_proxy",
        "no_proxy",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

fn confined_checkout_path(checkout: &Path, subdirectory: Option<&str>) -> Result<PathBuf> {
    let checkout = fs::canonicalize(checkout)?;
    let Some(subdirectory) = subdirectory else {
        return Ok(checkout);
    };
    validate_relative_path(subdirectory)?;
    let mut path = checkout.clone();
    for component in Path::new(subdirectory).components() {
        let Component::Normal(component) = component else {
            return Err(Error::Config("extension subdirectory is invalid".into()));
        };
        path.push(component);
        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(Error::Config(
                "extension subdirectory contains a symlink".into(),
            ));
        }
    }
    let path = fs::canonicalize(path)?;
    if !path.is_dir() || !path.starts_with(&checkout) {
        return Err(Error::Config(
            "extension subdirectory escapes its checkout".into(),
        ));
    }
    Ok(path)
}

fn validate_relative_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    if value.is_empty()
        || value.trim() != value
        || value.len() > MAX_SUBDIRECTORY_BYTES
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::Config(
            "extension subdirectory must be a bounded relative path".into(),
        ));
    }
    Ok(())
}

fn export_package(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir(destination)?;
    let mut files = 0;
    let mut bytes = 0;
    copy_directory(source, destination, Path::new(""), &mut files, &mut bytes)
}

fn copy_directory(
    source: &Path,
    destination: &Path,
    relative: &Path,
    files: &mut usize,
    bytes: &mut u64,
) -> Result<()> {
    let mut entries = fs::read_dir(source)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        if relative.as_os_str().is_empty() && entry.file_name() == ".git" {
            continue;
        }
        let source_path = entry.path();
        let child = relative.join(entry.file_name());
        let text = child
            .to_str()
            .ok_or_else(|| Error::Config("extension paths must be UTF-8".into()))?;
        if text.len() > MAX_PATH_BYTES {
            return Err(Error::Config("extension path is too long".into()));
        }
        let metadata = fs::symlink_metadata(&source_path)?;
        let destination_path = destination.join(entry.file_name());
        if metadata.is_dir() {
            fs::create_dir(&destination_path)?;
            copy_directory(&source_path, &destination_path, &child, files, bytes)?;
        } else if metadata.is_file() {
            *files += 1;
            *bytes = bytes.saturating_add(metadata.len());
            if *files > MAX_PACKAGE_FILES || *bytes > MAX_PACKAGE_BYTES {
                return Err(Error::Config("extension package is too large".into()));
            }
            fs::copy(&source_path, &destination_path)?;
            preserve_executable(&metadata, &destination_path)?;
        } else {
            return Err(Error::Config(format!(
                "extension package contains unsupported entry `{text}`"
            )));
        }
    }
    Ok(())
}

fn tree_digest(root: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    let mut files = 0;
    let mut bytes = 0;
    hash_directory(root, root, &mut hash, &mut files, &mut bytes)?;
    Ok(format!("{:x}", hash.finalize()))
}

fn hash_directory(
    root: &Path,
    directory: &Path,
    hash: &mut Sha256,
    files: &mut usize,
    bytes: &mut u64,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| Error::Config("extension path escaped its snapshot".into()))?;
        let relative = relative
            .to_str()
            .ok_or_else(|| Error::Config("extension paths must be UTF-8".into()))?;
        if relative.len() > MAX_PATH_BYTES {
            return Err(Error::Config("extension path is too long".into()));
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            hash.update(b"d");
            hash.update((relative.len() as u64).to_le_bytes());
            hash.update(relative.as_bytes());
            hash_directory(root, &path, hash, files, bytes)?;
        } else if metadata.is_file() {
            *files += 1;
            *bytes = bytes.saturating_add(metadata.len());
            if *files > MAX_PACKAGE_FILES || *bytes > MAX_PACKAGE_BYTES {
                return Err(Error::Config("extension package is too large".into()));
            }
            hash.update(b"f");
            hash.update((relative.len() as u64).to_le_bytes());
            hash.update(relative.as_bytes());
            hash.update([u8::from(is_executable(&metadata))]);
            hash.update(metadata.len().to_le_bytes());
            let mut file = fs::File::open(&path)?;
            let mut buffer = [0_u8; 16 * 1024];
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                hash.update(&buffer[..read]);
            }
        } else {
            return Err(Error::Config(format!(
                "extension snapshot contains unsupported entry `{relative}`"
            )));
        }
    }
    Ok(())
}

fn verify_installed_metadata(
    id: &str,
    installed: &InstalledExtension,
    package: &Path,
) -> Result<()> {
    let inspected = inspect_package(package)?;
    let kind = inspected.kind.into();
    let hooks = inspected
        .hooks
        .into_iter()
        .map(Into::into)
        .collect::<Vec<_>>();
    if installed.kind != kind
        || installed.name != inspected.name
        || installed.description != inspected.description
        || installed.version != inspected.version
        || installed.skills != inspected.skills
        || installed.hooks != hooks
    {
        return Err(Error::Config(format!(
            "extension `{id}` metadata does not match its snapshot"
        )));
    }
    Ok(())
}

fn verify_snapshot(root: &Path, expected: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|error| Error::Config(format!("extension snapshot is unavailable: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::Config(
            "extension snapshot root is not a regular directory".into(),
        ));
    }
    let actual = tree_digest(root)
        .map_err(|error| Error::Config(format!("extension snapshot is unavailable: {error}")))?;
    if actual != expected {
        return Err(Error::Config("extension snapshot digest changed".into()));
    }
    Ok(())
}

fn prepare_private_directory(path: &Path) -> Result<()> {
    if path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(Error::Config(
            "extension store root cannot be a symlink".into(),
        ));
    }
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn preserve_executable(source: &fs::Metadata, destination: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = if source.permissions().mode() & 0o111 == 0 {
            0o600
        } else {
            0o700
        };
        fs::set_permissions(destination, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

fn freeze_tree(path: &Path) -> Result<()> {
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            freeze_tree(&entry?.path())?;
        }
    }
    set_read_only(path, true)
}

fn thaw_tree(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || (!metadata.is_dir() && !metadata.is_file()) {
        return Err(Error::Config(
            "extension snapshot contains an unsupported entry".into(),
        ));
    }
    set_read_only(path, false)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            thaw_tree(&entry?.path())?;
        }
    }
    Ok(())
}

fn set_read_only(path: &Path, read_only: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let metadata = fs::symlink_metadata(path)?;
        let executable = metadata.is_dir() || is_executable(&metadata);
        let mode = match (read_only, executable) {
            (true, true) => 0o500,
            (true, false) => 0o400,
            (false, true) => 0o700,
            (false, false) => 0o600,
        };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_readonly(read_only);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    false
}

pub(crate) fn extensions_path(state_dir: &Path) -> PathBuf {
    let mut name = state_dir
        .file_name()
        .map_or_else(|| OsString::from("mobius"), OsString::from);
    name.push("-extensions");
    state_dir.with_file_name(name)
}

#[cfg(test)]
#[path = "extensions/tests.rs"]
mod tests;
