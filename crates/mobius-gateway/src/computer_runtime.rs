//! On-demand, owner-installed dependencies for the computer middleware.

pub(crate) mod desktop;

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use mobius::backend::model::provider::HttpClient;
#[cfg(unix)]
use mobius::backend::sandbox::ProcessGroupGuard;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::wire::MiddlewareConfig;
use crate::{Error, Result};

const NODE_VERSION: &str = "26.8.1";
const WORKER: &str = include_str!("computer_runtime/worker.cjs");
const DOCUMENTATION: &str = include_str!("computer_runtime/computer-control.md");
const PACKAGE: &str = include_str!("computer_runtime/package.json");
const LOCKFILE: &str = include_str!("computer_runtime/package-lock.json");
const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_DOWNLOAD_BYTES: usize = 128 * 1024 * 1024;

pub(crate) async fn prepare(
    state_dir: &Path,
    settings: &MiddlewareConfig,
) -> Result<Option<PathBuf>> {
    if !settings.enabled("computer_control") {
        return Ok(None);
    }
    if let Some(path) = std::env::var_os("MOBIUS_COMPUTER_RUNTIME") {
        let path = PathBuf::from(path);
        validate(&path)?;
        return Ok(Some(fs::canonicalize(path)?));
    }
    let path = managed_directory(state_dir);
    install(&path).await.map_err(|error| {
        Error::Config(format!(
            "Computer control setup failed: {error}. Try enabling Computer control again."
        ))
    })?;
    Ok(Some(fs::canonicalize(path)?))
}

pub(crate) fn managed_directory(state_dir: &Path) -> PathBuf {
    // Like extensions, executable resources must remain outside sandbox-masked state.
    let mut name = state_dir
        .file_name()
        .map_or_else(|| OsString::from("mobius"), OsString::from);
    name.push("-runtimes");
    let revision = format!(
        "{:x}",
        Sha256::digest(format!("{NODE_VERSION}{LOCKFILE}{WORKER}{DOCUMENTATION}"))
    );
    state_dir
        .with_file_name(name)
        .join("computer-control")
        .join(revision)
}

async fn install(destination: &Path) -> Result<()> {
    if destination.exists() {
        return validate(destination);
    }
    let parent = destination
        .parent()
        .ok_or_else(|| Error::Config("runtime directory has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let lock_path = parent.join("install.lock");
    let _lock = tokio::task::spawn_blocking(move || -> Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        file.lock()?;
        Ok(file)
    })
    .await
    .map_err(|error| Error::Config(error.to_string()))??;
    if destination.exists() {
        return validate(destination);
    }
    let stage = tempfile::Builder::new()
        .prefix(".install-")
        .tempdir_in(parent)?;
    tokio::time::timeout(INSTALL_TIMEOUT, populate(stage.path()))
        .await
        .map_err(|_| Error::Config("runtime download timed out".into()))??;
    validate(stage.path())?;
    // Publish only a complete runtime. Interrupted attempts leave no usable partial install.
    fs::rename(stage.path(), destination)?;
    Ok(())
}

fn validate(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(Error::Config(
            "computer runtime directory must be absolute".into(),
        ));
    }
    for file in [
        "node",
        "worker.cjs",
        "computer-control.md",
        "node_modules/playwright/package.json",
    ] {
        if !path.join(file).is_file() {
            return Err(Error::Config(format!(
                "computer runtime is missing {file} in {}",
                path.display()
            )));
        }
    }
    if !path.join("browsers").is_dir() {
        return Err(Error::Config(
            "computer runtime is missing its browser installation".into(),
        ));
    }
    Ok(())
}

fn node_distribution(os: &str, arch: &str) -> Result<(&'static str, &'static str)> {
    match (os, arch) {
        ("macos", "aarch64") => Ok((
            "darwin-arm64",
            "6e577fd0d9db776db82306629e441a9dace416702622aebdd171c9dfaa41f4d2",
        )),
        ("macos", "x86_64") => Ok((
            "darwin-x64",
            "fe9c6dbf9c8e1b4443803d75e2a20366e420dae650c747dbb116b22975751baf",
        )),
        ("linux", "aarch64") => Ok((
            "linux-arm64",
            "d5f973ce975e4bd03e6c2038260f7e9201615aa8e1ee293c72f8dcc2a6d9fddb",
        )),
        ("linux", "x86_64") => Ok((
            "linux-x64",
            "b2b76660fa4ded4e0b2a41ee3c0c651cd52ea8170ead91ebac1e147ac3d55643",
        )),
        _ => Err(Error::Config(format!(
            "computer control has no runtime for {os}/{arch}"
        ))),
    }
}

async fn populate(path: &Path) -> Result<()> {
    let (platform, checksum) = node_distribution(std::env::consts::OS, std::env::consts::ARCH)?;
    let archive = path.join("node.tar.gz");
    download(
        &format!("https://nodejs.org/dist/v{NODE_VERSION}/node-v{NODE_VERSION}-{platform}.tar.gz"),
        &archive,
        checksum,
    )
    .await?;
    let node_directory = path.join("distribution");
    fs::create_dir(&node_directory)?;
    let mut extract = Command::new("/usr/bin/tar");
    extract
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&node_directory)
        .args(["--strip-components", "1"]);
    run(extract, path).await?;
    fs::remove_file(archive)?;
    fs::hard_link(node_directory.join("bin/node"), path.join("node"))?;
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
    let mut npm = Command::new(path.join("node"));
    npm.arg(node_directory.join("lib/node_modules/npm/bin/npm-cli.js"))
        .args(["ci", "--ignore-scripts", "--no-fund", "--no-audit"]);
    run(npm, path).await?;
    let mut browser = Command::new(path.join("node"));
    browser
        .arg(path.join("node_modules/playwright/cli.js"))
        .args(["install", "--only-shell", "chromium"]);
    run(browser, path).await?;
    let mut verify = Command::new(path.join("node"));
    verify.args(["-e", "(async()=>{const b=await require('playwright').chromium.launch({headless:true}); await b.close()})().catch(e=>{console.error(e.message);process.exitCode=1})"]);
    run(verify, path).await?;
    fs::remove_dir_all(path.join(".home"))?;
    fs::remove_dir_all(path.join(".tmp"))?;
    fs::remove_file(path.join("install.log"))?;
    Ok(())
}

async fn download(url: &str, destination: &Path, checksum: &str) -> Result<()> {
    let client = HttpClient::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(180))
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

async fn run(mut command: Command, path: &Path) -> Result<()> {
    fs::create_dir_all(path.join(".home"))?;
    fs::create_dir_all(path.join(".tmp"))?;
    command
        .current_dir(path)
        .kill_on_drop(true)
        .env_clear()
        .env("HOME", path.join(".home"))
        .env("TMPDIR", path.join(".tmp"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", path.join("distribution/bin").display()),
        )
        .env("PLAYWRIGHT_BROWSERS_PATH", path.join("browsers"))
        .env("PLAYWRIGHT_SKIP_BROWSER_GC", "1")
        .env("npm_config_userconfig", "/dev/null")
        .env("npm_config_globalconfig", path.join(".home/global-npmrc"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(File::create(path.join("install.log"))?);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn()?;
    #[cfg(unix)]
    let _group = ProcessGroupGuard::new(&child)?;
    if !child.wait().await?.success() {
        let mut bytes = Vec::new();
        File::open(path.join("install.log"))?
            .take(4_000)
            .read_to_end(&mut bytes)?;
        return Err(Error::Config(format!(
            "runtime installer failed: {}",
            String::from_utf8_lossy(&bytes)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
