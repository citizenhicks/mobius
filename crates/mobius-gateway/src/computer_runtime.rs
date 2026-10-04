//! On-demand, owner-installed dependencies for the computer middleware.

pub(crate) mod browser;
pub mod config;
pub(crate) mod desktop;
pub(crate) mod remote_desktop;

use crate::wire::ServerMessage;

use std::borrow::Cow;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use mobius::backend::model::provider::{HttpCertificate, HttpClient};
#[cfg(unix)]
use mobius::backend::sandbox::ProcessGroupGuard;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;

use crate::wire::MiddlewareConfig;
use crate::{Error, Result};
pub use config::{BrowserConfig, ComputerConfig, DesktopConfig, RuntimeMode};

const WORKER: &str = include_str!("computer_runtime/worker.cjs");
const DOCUMENTATION: &str = include_str!("computer_runtime/computer-control.md");
const PACKAGE: &str = include_str!("computer_runtime/package.json");
const LOCKFILE: &str = include_str!("computer_runtime/package-lock.json");
const MAX_DOWNLOAD_BYTES: usize = 128 * 1024 * 1024;
const ROOT_SANDBOX_ERROR: &str = "Chromium cannot run as root with its sandbox enabled; run the gateway as a non-root user or explicitly set computer.browser.sandbox = false.";

pub(crate) enum AppUpdate {
    Desktop(Option<ServerMessage>),
    Browser(Option<ServerMessage>),
}

pub(crate) async fn next_app_update(
    desktop: &mut Option<desktop::DesktopConnection>,
    browser: &mut Option<browser::BrowserConnection>,
) -> AppUpdate {
    tokio::select! {
        message = desktop::next_update(desktop) => AppUpdate::Desktop(message),
        message = browser::next_update(browser) => AppUpdate::Browser(message),
    }
}

pub(crate) async fn write_app_update(
    update: AppUpdate,
    desktop: &mut Option<desktop::DesktopConnection>,
    browser: &mut Option<browser::BrowserConnection>,
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<()> {
    match update {
        AppUpdate::Desktop(message) => desktop::write_update(desktop, message, writer).await,
        AppUpdate::Browser(message) => browser::write_update(browser, message, writer).await,
    }
}

pub(crate) async fn prepare(
    state_dir: &Path,
    settings: &MiddlewareConfig,
    config: &ComputerConfig,
) -> Result<Option<PathBuf>> {
    config.validate()?;
    remote_desktop::prepare_configured_profile(config)?;
    if !settings.enabled("computer_control") {
        return Ok(None);
    }
    prepare_desktop(state_dir, config).await.map(Some)
}

pub(crate) async fn prepare_desktop(state_dir: &Path, config: &ComputerConfig) -> Result<PathBuf> {
    config.validate()?;
    remote_desktop::prepare_configured_profile(config)?;
    let override_path = config.directory.as_deref().map(Cow::Borrowed).or_else(|| {
        std::env::var_os("MOBIUS_COMPUTER_RUNTIME").map(|path| Cow::Owned(PathBuf::from(path)))
    });
    let path = override_path.as_deref().map_or_else(
        || Cow::Owned(managed_directory_for(state_dir, config)),
        Cow::Borrowed,
    );
    if config.mode != RuntimeMode::Managed
        || (config.directory.is_none() && override_path.is_some())
    {
        validate(&path, config)?;
        return Ok(fs::canonicalize(path)?);
    }
    install(&path, config).await.map_err(|error| {
        Error::Config(format!(
            "computer control setup failed: {error}; check the operator computer configuration"
        ))
    })?;
    Ok(fs::canonicalize(path)?)
}

#[cfg(test)]
pub(crate) fn managed_directory(state_dir: &Path) -> PathBuf {
    managed_directory_for(state_dir, &ComputerConfig::default())
}

fn managed_directory_for(state_dir: &Path, config: &ComputerConfig) -> PathBuf {
    // Like extensions, executable resources must remain outside sandbox-masked state.
    let mut name = state_dir
        .file_name()
        .map_or_else(|| OsString::from("mobius"), OsString::from);
    name.push("-runtimes");
    let mut hash = Sha256::new();
    for value in [
        config::DEFAULTS_TEXT,
        PACKAGE,
        LOCKFILE,
        WORKER,
        DOCUMENTATION,
    ] {
        hash.update(value.as_bytes());
    }
    // Different dependency layouts must never reuse an install missing bundled Node/Chromium.
    for path in [
        &config.node_executable,
        &config.npm_executable,
        &config.playwright_module,
        &config.browsers_directory,
        &config.browser.executable,
    ] {
        hash.update([u8::from(path.is_some())]);
        if let Some(path) = path {
            hash.update(path.as_os_str().as_encoded_bytes());
        }
        hash.update([0]);
    }
    let revision = format!("{:x}", hash.finalize());
    state_dir
        .with_file_name(name)
        .join("computer-control")
        .join(revision)
}

async fn install(destination: &Path, config: &ComputerConfig) -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(config.install_timeout_seconds),
        install_locked(destination, config),
    )
    .await
    .map_err(|_| Error::Config("runtime installation timed out".into()))?
}

async fn install_locked(destination: &Path, config: &ComputerConfig) -> Result<()> {
    if destination.exists() {
        return validate(destination, config);
    }
    let parent = destination
        .parent()
        .ok_or_else(|| Error::Config("runtime directory has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let lock_path = parent.join("install.lock");
    let _lock = {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        loop {
            match file.try_lock() {
                Ok(()) => break file,
                Err(std::fs::TryLockError::WouldBlock) => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
    };
    if destination.exists() {
        return validate(destination, config);
    }
    let stage = tempfile::Builder::new()
        .prefix(".install-")
        .tempdir_in(parent)?;
    populate(stage.path(), config).await?;
    validate(stage.path(), config)?;
    // Publish only a complete runtime. Interrupted attempts leave no usable partial install.
    fs::rename(stage.path(), destination)?;
    Ok(())
}

fn validate(path: &Path, config: &ComputerConfig) -> Result<()> {
    if !path.is_absolute() {
        return Err(Error::Config(
            "computer runtime directory must be absolute".into(),
        ));
    }
    if config.node_executable.is_none() && !path.join("node").is_file() {
        return Err(Error::Config(format!(
            "computer runtime is missing node in {}",
            path.display()
        )));
    }
    node_executable(path, config)?;
    for file in ["worker.cjs", "computer-control.md"] {
        if !path.join(file).is_file() {
            return Err(Error::Config(format!(
                "computer runtime is missing {file} in {}",
                path.display()
            )));
        }
    }
    let mut worker = Vec::new();
    File::open(path.join("worker.cjs"))?
        .take(128 * 1024)
        .read_to_end(&mut worker)?;
    if worker != WORKER.as_bytes() {
        return Err(Error::Config(
            "computer worker does not match this gateway; export its current computer resources"
                .into(),
        ));
    }
    if !playwright_module(path, config)
        .join("package.json")
        .is_file()
    {
        return Err(Error::Config(
            "computer runtime is missing Playwright".into(),
        ));
    }
    if let Some(executable) = &config.browser.executable {
        config::resolve_executable(executable)?;
    } else if !browsers_directory(path, config).is_dir() {
        return Err(Error::Config(
            "computer runtime is missing its browser installation".into(),
        ));
    }
    Ok(())
}

async fn install_node(path: &Path, config: &ComputerConfig) -> Result<()> {
    if let Some(node) = &config.node_executable {
        config::resolve_executable(node)?;
        return Ok(());
    }
    let (platform, checksum) =
        config::node_distribution(std::env::consts::OS, std::env::consts::ARCH)?;
    let node_version = config::node_version();
    let archive = path.join("node.tar.gz");
    download(
        &format!(
            "{}/v{node_version}/node-v{node_version}-{platform}.tar.gz",
            config.node_download_base_url.trim_end_matches('/')
        ),
        &archive,
        checksum,
        config,
    )
    .await?;
    let node_directory = path.join("distribution");
    fs::create_dir(&node_directory)?;
    let mut extract = Command::new(config::resolve_executable(Path::new(
        config.tar_executable.as_ref(),
    ))?);
    extract
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&node_directory)
        .args(["--strip-components", "1"]);
    run(extract, path, config).await?;
    fs::remove_file(archive)?;
    fs::hard_link(node_directory.join("bin/node"), path.join("node"))?;
    Ok(())
}

/// Export the gateway-owned worker, documentation, and pinned package manifests.
/// Operators can install Playwright themselves and select system Node/Chromium.
///
/// # Errors
/// Returns an error if the destination is not absolute or a resource cannot be written.
pub fn export_resources(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(Error::Config(
            "computer resource destination must be absolute".into(),
        ));
    }
    fs::create_dir_all(path)?;
    for (name, content) in [
        ("worker.cjs", WORKER),
        ("computer-control.md", DOCUMENTATION),
        ("package.json", PACKAGE),
        ("package-lock.json", LOCKFILE),
        ("LICENSE", include_str!("../LICENSE")),
        ("NOTICE", include_str!("../NOTICE")),
    ] {
        fs::write(path.join(name), content)?;
    }
    let config = ComputerConfig::default();
    let mut options = worker_options(path, &config)?;
    options.playwright_module = Cow::Borrowed(Path::new("playwright"));
    options.browsers_directory = Cow::Borrowed(Path::new("browsers"));
    fs::write(
        path.join("browser.json"),
        serde_json::to_vec_pretty(&options)?,
    )?;
    Ok(())
}

async fn populate(path: &Path, config: &ComputerConfig) -> Result<()> {
    config
        .browser
        .validate_root_sandbox(nix::unistd::geteuid().is_root())?;
    install_node(path, config).await?;
    export_resources(path)?;
    if config.playwright_module.is_none() {
        let mut npm = npm_command(path, config)?;
        npm.args(["ci", "--ignore-scripts", "--no-fund", "--no-audit"]);
        run(npm, path, config).await?;
    }
    if config.browser.executable.is_none() {
        let mut browser = Command::new(node_executable(path, config)?);
        browser
            .arg(playwright_module(path, config).join("cli.js"))
            .args(["install", "chromium"]);
        run(browser, path, config).await?;
    }
    let mut verify = Command::new(node_executable(path, config)?);
    verify.args(["-e", "(async()=>{const o=JSON.parse(process.argv[1]); const {chromium}=require(o.playwright_module); const b=await chromium.launch({headless:true,chromiumSandbox:o.sandbox,args:o.arguments,...(o.executable?{executablePath:o.executable}:{})}); await b.close()})().catch(e=>{console.error(e.message);process.exitCode=1})"])
        .arg(serde_json::to_string(&worker_options(path, config)?)?);
    run(verify, path, config).await?;
    fs::remove_dir_all(path.join(".home"))?;
    fs::remove_dir_all(path.join(".tmp"))?;
    fs::remove_file(path.join("install.log"))?;
    Ok(())
}

fn npm_command(path: &Path, config: &ComputerConfig) -> Result<Command> {
    if config.node_executable.is_some() || config.npm_executable.is_some() {
        let npm = config
            .npm_executable
            .as_deref()
            .unwrap_or_else(|| Path::new("npm"));
        return Ok(Command::new(config::resolve_executable(npm)?));
    }
    let mut command = Command::new(node_executable(path, config)?);
    command.arg(path.join("distribution/lib/node_modules/npm/bin/npm-cli.js"));
    Ok(command)
}

pub(crate) fn node_executable(path: &Path, config: &ComputerConfig) -> Result<PathBuf> {
    match config.node_executable.as_deref() {
        Some(executable) => config::resolve_executable(executable),
        None => config::resolve_executable(&path.join("node")),
    }
}

fn playwright_module<'a>(path: &Path, config: &'a ComputerConfig) -> Cow<'a, Path> {
    config.playwright_module.as_deref().map_or_else(
        || Cow::Owned(path.join("node_modules/playwright")),
        Cow::Borrowed,
    )
}

pub(crate) fn browsers_directory<'a>(path: &Path, config: &'a ComputerConfig) -> Cow<'a, Path> {
    config
        .browsers_directory
        .as_deref()
        .map_or_else(|| Cow::Owned(path.join("browsers")), Cow::Borrowed)
}

#[derive(serde::Serialize)]
struct WorkerOptions<'a> {
    executable: Option<PathBuf>,
    sandbox: bool,
    arguments: Vec<&'a str>,
    viewport: [u32; 2],
    start_page: &'a str,
    playwright_module: Cow<'a, Path>,
    browsers_directory: Cow<'a, Path>,
    root_sandbox_error: &'static str,
}

fn worker_options<'a>(path: &Path, config: &'a ComputerConfig) -> Result<WorkerOptions<'a>> {
    Ok(WorkerOptions {
        executable: config
            .browser
            .executable
            .as_deref()
            .map(config::resolve_executable)
            .transpose()?,
        sandbox: config.browser.sandbox,
        arguments: config.browser.launch_arguments().collect(),
        viewport: config.browser.viewport,
        start_page: &config.browser.start_page,
        playwright_module: playwright_module(path, config),
        browsers_directory: browsers_directory(path, config),
        root_sandbox_error: ROOT_SANDBOX_ERROR,
    })
}

pub(crate) fn worker_command(
    path: &Path,
    config: &ComputerConfig,
) -> Result<mobius::backend::sandbox::WorkerCommand> {
    config.validate()?;
    Ok(mobius::backend::sandbox::WorkerCommand {
        executable: node_executable(path, config)?,
        arguments: vec![
            path.join("worker.cjs").to_string_lossy().into_owned(),
            serde_json::to_string(&worker_options(path, config)?)?,
        ],
    })
}

pub(crate) fn resource_roots(
    path: &Path,
    config: &ComputerConfig,
    state_dir: &Path,
    workspace: &Path,
    attached_folders: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    let forbidden = std::iter::once(state_dir)
        .chain(std::iter::once(workspace))
        .chain(attached_folders.iter().map(PathBuf::as_path))
        .map(fs::canonicalize)
        .collect::<std::io::Result<Vec<_>>>()?;
    let node = node_executable(path, config)?;
    let browser = config
        .browser
        .executable
        .as_deref()
        .map(config::resolve_executable)
        .transpose()?;
    let playwright = playwright_module(path, config);
    let browsers = browsers_directory(path, config);
    let mut roots = [
        Some(path),
        Some(playwright.as_ref()),
        Some(browsers.as_ref()),
        node.parent(),
        browser.as_deref().and_then(Path::parent),
    ]
    .into_iter()
    .flatten()
    .filter(|path| path.exists())
    .map(fs::canonicalize)
    .collect::<std::io::Result<Vec<_>>>()?;
    for root in &roots {
        if !root.is_dir() {
            return Err(Error::Config(
                "computer resource roots must be directories".into(),
            ));
        }
        if forbidden
            .iter()
            .any(|path| root.starts_with(path) || path.starts_with(root))
        {
            return Err(Error::Config(
                "computer resources must remain outside gateway state and all writable workspaces"
                    .into(),
            ));
        }
    }
    roots.sort_unstable();
    roots.dedup();
    Ok(roots)
}

async fn download(
    url: &str,
    destination: &Path,
    checksum: &str,
    config: &ComputerConfig,
) -> Result<()> {
    let mut client = HttpClient::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(config.download_connect_timeout_seconds))
        .timeout(Duration::from_secs(config.download_timeout_seconds));
    let ca_file =
        config.ca_file.as_deref().map(Cow::Borrowed).or_else(|| {
            std::env::var_os("SSL_CERT_FILE").map(|path| Cow::Owned(PathBuf::from(path)))
        });
    if let Some(path) = ca_file {
        for certificate in ca_certificates(&path).await? {
            client = client.add_root_certificate(certificate);
        }
    }
    let client = client
        .build()
        .map_err(|error| Error::Config(error.to_string()))?;
    let mut response = client
        .get(url)
        .send()
        .await
        .and_then(|response| response.error_for_status())
        .map_err(|error| Error::Config(error.to_string()))?;
    let mut file = tokio::fs::File::create(destination).await?;
    let mut hash = Sha256::new();
    let mut size = 0_usize;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| Error::Config(error.to_string()))?
    {
        size = size.saturating_add(chunk.len());
        if size > MAX_DOWNLOAD_BYTES {
            return Err(Error::Config(
                "runtime download exceeds its size limit".into(),
            ));
        }
        hash.update(&chunk);
        file.write_all(&chunk).await?;
    }
    if format!("{:x}", hash.finalize()) != checksum {
        return Err(Error::Config("Node runtime checksum did not match".into()));
    }
    file.flush().await?;
    Ok(())
}

async fn run(mut command: Command, path: &Path, config: &ComputerConfig) -> Result<()> {
    fs::create_dir_all(path.join(".home"))?;
    fs::create_dir_all(path.join(".tmp"))?;
    command
        .current_dir(path)
        .kill_on_drop(true)
        .env_clear()
        .env("HOME", path.join(".home"))
        .env("TMPDIR", path.join(".tmp"))
        .env("PATH", installer_path(path, config)?)
        .env(
            "PLAYWRIGHT_BROWSERS_PATH",
            browsers_directory(path, config).as_ref(),
        )
        .env("PLAYWRIGHT_SKIP_BROWSER_GC", "1")
        .env("npm_config_userconfig", "/dev/null")
        .env("npm_config_globalconfig", path.join(".home/global-npmrc"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(File::create(path.join("install.log"))?);
    forward_installer_environment(&mut command, |name| std::env::var_os(name));
    if let Some(path) = &config.ca_file {
        command
            .env("SSL_CERT_FILE", path)
            .env("NODE_EXTRA_CA_CERTS", path)
            .env("npm_config_cafile", path);
    }
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn()?;
    #[cfg(unix)]
    let _group = ProcessGroupGuard::new(&child)?;
    if !child.wait().await?.success() {
        // npm/download errors can contain authenticated proxy URLs. Keep them
        // inside the private installation directory rather than returning them to a client.
        return Err(Error::Config("runtime installer failed".into()));
    }
    Ok(())
}

fn installer_path(path: &Path, config: &ComputerConfig) -> Result<OsString> {
    let directory = match &config.node_executable {
        Some(node) => config::resolve_executable(node)?
            .parent()
            .ok_or_else(|| Error::Config("Node executable has no parent".into()))?
            .to_path_buf(),
        None => path.join("distribution/bin"),
    };
    // npm/Playwright subprocesses inherit the selected Node directory and
    // standard system tools, never the gateway's ambient executable search path.
    std::env::join_paths([directory, PathBuf::from("/usr/bin"), PathBuf::from("/bin")])
        .map_err(|error| Error::Config(error.to_string()))
}

fn forward_installer_environment(command: &mut Command, get: impl Fn(&str) -> Option<OsString>) {
    crate::process_environment::forward_network_environment(command, &get);
    for name in [
        "NODE_EXTRA_CA_CERTS",
        "NODE_USE_SYSTEM_CA",
        "npm_config_cafile",
        "NPM_CONFIG_CAFILE",
        "PLAYWRIGHT_DOWNLOAD_HOST",
        "PLAYWRIGHT_CHROMIUM_DOWNLOAD_HOST",
    ] {
        if let Some(value) = get(name) {
            command.env(name, value);
        }
    }
    // Node/npm use their own CA setting rather than OpenSSL's SSL_CERT_FILE.
    if get("NODE_EXTRA_CA_CERTS").is_none()
        && let Some(value) = get("SSL_CERT_FILE")
    {
        command.env("NODE_EXTRA_CA_CERTS", value);
    }
}

async fn ca_certificates(path: &Path) -> Result<Vec<HttpCertificate>> {
    const MAX_CA_BYTES: usize = 1024 * 1024;
    let mut bytes = Vec::new();
    tokio::fs::File::open(path)
        .await?
        .take(
            u64::try_from(MAX_CA_BYTES.saturating_add(1))
                .map_err(|_| Error::Config("computer CA limit is not representable".into()))?,
        )
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > MAX_CA_BYTES {
        return Err(Error::Config(
            "computer CA bundle exceeds its size limit".into(),
        ));
    }
    let certificates = HttpCertificate::from_pem_bundle(&bytes)
        .map_err(|_| Error::Config("invalid computer CA certificate bundle".into()))?;
    if certificates.is_empty() {
        return Err(Error::Config(
            "computer CA bundle contains no certificates".into(),
        ));
    }
    Ok(certificates)
}

#[cfg(test)]
mod tests;
