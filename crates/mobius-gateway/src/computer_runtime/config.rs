//! Operator-owned computer runtime and desktop configuration.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

pub(super) const DEFAULTS_TEXT: &str = include_str!("defaults.toml");

// Required DTOs never call public Default implementations while parsing the
// embedded source. Missing fields therefore fail instead of recursing into LazyLock.
#[expect(
    clippy::expect_used,
    reason = "the embedded defaults are compiled in; invalid ones are a build defect tests catch"
)]
static DEFAULTS: LazyLock<ComputerDefaults> = LazyLock::new(|| {
    let defaults: ComputerDefaults = mobius::config::embedded(DEFAULTS_TEXT);
    for application in [
        &defaults.applications.window_manager,
        &defaults.applications.panel,
        &defaults.applications.wallpaper,
        &defaults.applications.terminal,
        &defaults.applications.files,
    ] {
        command_name(&application.executable).expect("valid embedded desktop command");
        for argument in &application.arguments {
            valid_argument(argument).expect("valid embedded desktop argument");
        }
    }
    defaults
});

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ComputerDefaults {
    mode: RuntimeMode,
    install_timeout_seconds: u64,
    download_connect_timeout_seconds: u64,
    download_timeout_seconds: u64,
    node_download_base_url: String,
    tar_executable: String,
    node: NodeDefaults,
    browser: BrowserDefaults,
    desktop: DesktopDefaults,
    applications: ApplicationDefaults,
    labels: LabelDefaults,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserDefaults {
    sandbox: bool,
    arguments: Vec<String>,
    window_size: [u32; 2],
    window_position: [i32; 2],
    start_page: String,
    viewport: [u32; 2],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DesktopDefaults {
    resolution: [u32; 2],
    depth: u8,
    display_start: u16,
    display_end: u16,
    startup_timeout_seconds: u64,
    tools: ToolDefaults,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolDefaults {
    xauth: String,
    xvnc: Vec<String>,
    vncconfig: Vec<String>,
    setpriv: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplicationDefaults {
    window_manager: ApplicationCommand,
    panel: ApplicationCommand,
    wallpaper: ApplicationCommand,
    terminal: ApplicationCommand,
    files: ApplicationCommand,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplicationCommand {
    executable: String,
    arguments: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeDefaults {
    version: String,
    distributions: BTreeMap<String, NodeDistribution>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeDistribution {
    platform: String,
    checksum: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelDefaults {
    browser: String,
    terminal: String,
    files: String,
}

/// How executable computer resources are supplied by the operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMode {
    /// Install pinned dependencies on demand into the managed directory.
    Managed,
    /// Use an operator-provisioned directory without downloading or changing it.
    Preinstalled,
}

/// Computer resources and policy supplied by the gateway operator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ComputerConfig {
    /// Managed installation or preinstalled resources without any downloads.
    pub mode: RuntimeMode,
    /// Resource directory; omitted means the gateway-derived managed directory.
    pub directory: Option<PathBuf>,
    /// System Node executable; omitted uses the pinned bundled Node.
    pub node_executable: Option<PathBuf>,
    /// npm executable for installation with a system Node.
    pub npm_executable: Option<PathBuf>,
    /// External Playwright module directory, containing its package.json.
    pub playwright_module: Option<PathBuf>,
    /// Browser installation directory; omitted uses resources/browsers.
    pub browsers_directory: Option<PathBuf>,
    /// Deadline for the complete managed installation.
    pub install_timeout_seconds: u64,
    /// Deadline for connecting to a runtime distribution server.
    pub download_connect_timeout_seconds: u64,
    /// Deadline for one pinned distribution download.
    pub download_timeout_seconds: u64,
    /// HTTPS mirror of the pinned Node distribution directory.
    pub node_download_base_url: Cow<'static, str>,
    /// tar command or absolute executable path used for the pinned Node archive.
    pub tar_executable: Cow<'static, str>,
    /// Additional PEM CA bundle for runtime downloads and npm/Playwright installation.
    /// Omitted means SSL_CERT_FILE (and Node's own CA environment) is respected.
    pub ca_file: Option<PathBuf>,
    /// Browser launch policy shared by all launch paths.
    pub browser: BrowserConfig,
    /// Linux virtual desktop policy.
    pub desktop: DesktopConfig,
}

impl Default for ComputerConfig {
    fn default() -> Self {
        Self {
            mode: DEFAULTS.mode,
            directory: None,
            node_executable: None,
            npm_executable: None,
            playwright_module: None,
            browsers_directory: None,
            install_timeout_seconds: DEFAULTS.install_timeout_seconds,
            download_connect_timeout_seconds: DEFAULTS.download_connect_timeout_seconds,
            download_timeout_seconds: DEFAULTS.download_timeout_seconds,
            node_download_base_url: Cow::Borrowed(&DEFAULTS.node_download_base_url),
            tar_executable: Cow::Borrowed(&DEFAULTS.tar_executable),
            ca_file: None,
            browser: BrowserConfig::default(),
            desktop: DesktopConfig::default(),
        }
    }
}

/// Browser settings; the gateway always owns its private CDP endpoint and profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserConfig {
    /// Chromium-compatible executable path or command; omitted uses Playwright discovery.
    pub executable: Option<PathBuf>,
    /// Private persistent profile directory; omitted uses gateway desktop state.
    pub profile_directory: Option<PathBuf>,
    /// Keep Chromium's internal sandbox enabled. Disabling requires an explicit operator choice.
    pub sandbox: bool,
    /// Additional launch switches. Profile, CDP, and sandbox switches are rejected.
    pub arguments: Cow<'static, [String]>,
    /// Headed window width and height in pixels.
    pub window_size: [u32; 2],
    /// Headed window x and y position in pixels.
    pub window_position: [i32; 2],
    /// Initial page for a newly launched browser or session tab.
    pub start_page: Cow<'static, str>,
    /// Headless browser viewport width and height in pixels.
    pub viewport: [u32; 2],
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            executable: None,
            profile_directory: None,
            sandbox: DEFAULTS.browser.sandbox,
            arguments: Cow::Borrowed(&DEFAULTS.browser.arguments),
            window_size: DEFAULTS.browser.window_size,
            window_position: DEFAULTS.browser.window_position,
            start_page: Cow::Borrowed(&DEFAULTS.browser.start_page),
            viewport: DEFAULTS.browser.viewport,
        }
    }
}

/// Optional desktop component. Missing default applications are skipped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DesktopApplication {
    /// Enable this component when its executable is available.
    pub enabled: bool,
    /// Command or absolute path; omitted uses this component's manifest default.
    pub executable: Option<String>,
    /// Arguments; omitted uses this component's manifest default, while [] clears them.
    pub arguments: Option<Vec<String>>,
}

impl Default for DesktopApplication {
    fn default() -> Self {
        Self {
            enabled: true,
            executable: None,
            arguments: None,
        }
    }
}

/// Executables needed for the private virtual display and launcher lifecycle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DesktopTools {
    /// Xauthority writer command or path.
    pub xauth: Cow<'static, str>,
    /// Ordered private VNC server command/path candidates.
    pub xvnc: Cow<'static, [String]>,
    /// Ordered VNC input-control command/path candidates.
    pub vncconfig: Cow<'static, [String]>,
    /// Launcher parent-death helper command or path.
    pub setpriv: Cow<'static, str>,
}

impl Default for DesktopTools {
    fn default() -> Self {
        let tools = &DEFAULTS.desktop.tools;
        Self {
            xauth: Cow::Borrowed(&tools.xauth),
            xvnc: Cow::Borrowed(&tools.xvnc),
            vncconfig: Cow::Borrowed(&tools.vncconfig),
            setpriv: Cow::Borrowed(&tools.setpriv),
        }
    }
}

/// Operator-provided artwork and tint2 templates, with bundled defaults when omitted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DesktopBranding {
    /// Wallpaper image.
    pub wallpaper: Option<PathBuf>,
    /// Status-panel logo image.
    pub logo: Option<PathBuf>,
    /// Browser launcher icon; omitted uses the selected browser's installation icon.
    pub browser_icon: Option<PathBuf>,
    /// Terminal launcher icon.
    pub terminal_icon: Option<PathBuf>,
    /// File-manager launcher icon.
    pub files_icon: Option<PathBuf>,
    /// tint2 launcher-panel template. @DIRECTORY@ resolves to private desktop state.
    pub panel_config: Option<PathBuf>,
    /// tint2 status-panel template. @DIRECTORY@ resolves to private desktop state.
    pub status_config: Option<PathBuf>,
    /// Browser launcher display name.
    pub browser_label: Cow<'static, str>,
    /// Terminal launcher display name.
    pub terminal_label: Cow<'static, str>,
    /// File-manager launcher display name.
    pub files_label: Cow<'static, str>,
}

impl Default for DesktopBranding {
    fn default() -> Self {
        Self {
            wallpaper: None,
            logo: None,
            browser_icon: None,
            terminal_icon: None,
            files_icon: None,
            panel_config: None,
            status_config: None,
            browser_label: Cow::Borrowed(&DEFAULTS.labels.browser),
            terminal_label: Cow::Borrowed(&DEFAULTS.labels.terminal),
            files_label: Cow::Borrowed(&DEFAULTS.labels.files),
        }
    }
}

/// Linux desktop geometry, tool discovery, optional components, and artwork.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DesktopConfig {
    /// Private virtual display width and height in pixels.
    pub resolution: [u32; 2],
    /// Virtual display colour depth: 16, 24, or 32.
    pub depth: u8,
    /// First candidate X11 display number, inclusive.
    pub display_start: u16,
    /// Last candidate X11 display number, exclusive.
    pub display_end: u16,
    /// Display/browser startup deadline.
    pub startup_timeout_seconds: u64,
    /// Private display tools.
    pub tools: DesktopTools,
    /// Optional window manager.
    pub window_manager: DesktopApplication,
    /// Optional tint2-compatible panel.
    pub panel: DesktopApplication,
    /// Optional wallpaper setter, whose last argument is the artwork path.
    pub wallpaper: DesktopApplication,
    /// Optional terminal launcher.
    pub terminal: DesktopApplication,
    /// Optional file-manager launcher.
    pub files: DesktopApplication,
    /// Artwork and panel templates.
    pub branding: DesktopBranding,
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            resolution: DEFAULTS.desktop.resolution,
            depth: DEFAULTS.desktop.depth,
            display_start: DEFAULTS.desktop.display_start,
            display_end: DEFAULTS.desktop.display_end,
            startup_timeout_seconds: DEFAULTS.desktop.startup_timeout_seconds,
            tools: DesktopTools::default(),
            window_manager: DesktopApplication::default(),
            panel: DesktopApplication::default(),
            wallpaper: DesktopApplication::default(),
            terminal: DesktopApplication::default(),
            files: DesktopApplication::default(),
            branding: DesktopBranding::default(),
        }
    }
}

impl ComputerConfig {
    /// Validate operator policy without installing or executing anything.
    ///
    /// # Errors
    /// Rejects invalid directories, launch switches, URLs, dimensions, or deadlines.
    pub fn validate(&self) -> Result<()> {
        for path in [
            &self.directory,
            &self.playwright_module,
            &self.browsers_directory,
            &self.ca_file,
        ]
        .into_iter()
        .flatten()
        {
            absolute_path(path)?;
        }
        for path in [&self.node_executable, &self.npm_executable]
            .into_iter()
            .flatten()
        {
            command_name(path.as_os_str().to_str().ok_or_else(invalid_command)?)?;
        }
        deadline(self.install_timeout_seconds)?;
        deadline(self.download_connect_timeout_seconds)?;
        deadline(self.download_timeout_seconds)?;
        let url = url::Url::parse(&self.node_download_base_url)
            .map_err(|error| Error::Config(format!("invalid Node download mirror: {error}")))?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::Config(
                "Node download mirror must be HTTPS without credentials".into(),
            ));
        }
        command_name(&self.tar_executable)?;
        self.browser.validate()?;
        self.desktop.validate()
    }
}

impl BrowserConfig {
    pub(crate) fn validate_root_sandbox(&self, root: bool) -> Result<()> {
        if root && self.sandbox {
            return Err(Error::Config(super::ROOT_SANDBOX_ERROR.into()));
        }
        Ok(())
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if let Some(path) = &self.executable {
            command_name(path.as_os_str().to_str().ok_or_else(invalid_command)?)?;
        }
        if let Some(path) = &self.profile_directory {
            absolute_path(path)?;
        }
        dimensions(self.window_size)?;
        dimensions(self.viewport)?;
        if self.arguments.len() > 128 {
            return Err(Error::Config("too many browser arguments".into()));
        }
        valid_text(&self.start_page)?;
        for argument in self.arguments.iter() {
            valid_text(argument)?;
            let switch = argument.split('=').next().unwrap_or(argument);
            if !argument.starts_with("--")
                || argument.to_ascii_lowercase().contains("sandbox")
                || switch.starts_with("--remote-debugging")
                || [
                    "--remote-debugging-address",
                    "--remote-debugging-port",
                    "--remote-debugging-pipe",
                    "--remote-allow-origins",
                    "--user-data-dir",
                    "--profile-directory",
                    "--no-sandbox",
                    "--disable-setuid-sandbox",
                    "--disable-seccomp-filter-sandbox",
                    "--headless",
                    "--window-size",
                    "--window-position",
                ]
                .contains(&switch)
            {
                return Err(Error::Config(
                    "browser arguments cannot override private control or sandbox policy".into(),
                ));
            }
        }
        let url = url::Url::parse(&self.start_page)
            .map_err(|error| Error::Config(format!("invalid browser start page: {error}")))?;
        if !matches!(url.scheme(), "http" | "https" | "about")
            || (url.scheme() == "about" && self.start_page != "about:blank")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(Error::Config(
                "browser start page must be HTTP(S) or about:blank".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn launch_arguments(&self) -> impl Iterator<Item = &str> {
        self.arguments
            .iter()
            .map(String::as_str)
            .chain((!self.sandbox).then_some("--no-sandbox"))
    }

    pub(crate) fn headed_arguments(&self, profile: &Path) -> Vec<Cow<'_, str>> {
        let mut arguments = self
            .launch_arguments()
            .map(Cow::Borrowed)
            .collect::<Vec<_>>();
        arguments.extend([
            Cow::Owned(format!(
                "--window-size={},{}",
                self.window_size[0], self.window_size[1]
            )),
            Cow::Owned(format!(
                "--window-position={},{}",
                self.window_position[0], self.window_position[1]
            )),
            "--remote-debugging-address=127.0.0.1".into(),
            "--remote-debugging-port=0".into(),
            Cow::Owned(format!("--user-data-dir={}", profile.display())),
            Cow::Borrowed(&self.start_page),
        ]);
        arguments
    }
}

impl DesktopConfig {
    fn validate(&self) -> Result<()> {
        dimensions(self.resolution)?;
        deadline(self.startup_timeout_seconds)?;
        if !matches!(self.depth, 16 | 24 | 32)
            || self.display_start == 0
            || self.display_end <= self.display_start
            || self.display_end > 60000
            || self.display_end - self.display_start > 256
        {
            return Err(Error::Config(
                "invalid desktop depth or display range".into(),
            ));
        }
        for command in [self.tools.xauth.as_ref(), self.tools.setpriv.as_ref()]
            .into_iter()
            .chain(self.tools.xvnc.iter().map(String::as_str))
            .chain(self.tools.vncconfig.iter().map(String::as_str))
        {
            command_name(command)?;
        }
        if self.tools.xvnc.is_empty() || self.tools.vncconfig.is_empty() {
            return Err(invalid_command());
        }
        for application in [
            &self.window_manager,
            &self.panel,
            &self.wallpaper,
            &self.terminal,
            &self.files,
        ] {
            if let Some(executable) = &application.executable {
                command_name(executable)?;
            }
            if let Some(arguments) = &application.arguments {
                if arguments.len() > 128 {
                    return Err(Error::Config("too many desktop arguments".into()));
                }
                for argument in arguments {
                    valid_argument(argument)?;
                }
            }
        }
        for path in [
            &self.branding.wallpaper,
            &self.branding.logo,
            &self.branding.browser_icon,
            &self.branding.terminal_icon,
            &self.branding.files_icon,
            &self.branding.panel_config,
            &self.branding.status_config,
        ]
        .into_iter()
        .flatten()
        {
            absolute_path(path)?;
        }
        for label in [
            &self.branding.browser_label,
            &self.branding.terminal_label,
            &self.branding.files_label,
        ] {
            valid_text(label)?;
        }
        Ok(())
    }
}

/// Resolve an optional desktop app; missing defaults do not disable the whole desktop.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
pub(crate) enum ApplicationRole {
    WindowManager,
    Panel,
    Wallpaper,
    Terminal,
    Files,
}

#[cfg(target_os = "linux")]
pub(crate) fn application(
    config: &DesktopApplication,
    role: ApplicationRole,
) -> Result<Option<(PathBuf, &[String])>> {
    if !config.enabled {
        return Ok(None);
    }
    let defaults = match role {
        ApplicationRole::WindowManager => &DEFAULTS.applications.window_manager,
        ApplicationRole::Panel => &DEFAULTS.applications.panel,
        ApplicationRole::Wallpaper => &DEFAULTS.applications.wallpaper,
        ApplicationRole::Terminal => &DEFAULTS.applications.terminal,
        ApplicationRole::Files => &DEFAULTS.applications.files,
    };
    let executable = config.executable.as_deref().unwrap_or(&defaults.executable);
    let path = match resolve_executable(Path::new(executable)) {
        Ok(path) => path,
        Err(_) if config.executable.is_none() => return Ok(None),
        Err(error) => return Err(error),
    };
    let arguments = config.arguments.as_deref().unwrap_or(&defaults.arguments);
    Ok(Some((path, arguments)))
}

pub(crate) fn resolve_executable(command: &Path) -> Result<PathBuf> {
    command_name(command.as_os_str().to_str().ok_or_else(invalid_command)?)?;
    let path = if command.is_absolute() {
        Some(Cow::Borrowed(command))
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join(command))
            .find(|path| is_executable(path))
            .map(Cow::Owned)
    };
    let path = path.filter(|path| is_executable(path)).ok_or_else(|| {
        Error::Config(format!(
            "computer runtime executable is unavailable: {}",
            command.display()
        ))
    })?;
    Ok(std::fs::canonicalize(path)?)
}

pub(crate) fn resolve_candidates(commands: &[String]) -> Result<PathBuf> {
    for command in commands {
        if let Ok(path) = resolve_executable(Path::new(command)) {
            return Ok(path);
        }
    }
    Err(Error::Config(format!(
        "desktop requires {}",
        commands.join(" or ")
    )))
}

pub(crate) fn node_version() -> &'static str {
    &DEFAULTS.node.version
}

pub(crate) fn node_distribution(os: &str, arch: &str) -> Result<(&'static str, &'static str)> {
    let key = format!("{os}-{arch}");
    let distribution =
        DEFAULTS.node.distributions.get(&key).ok_or_else(|| {
            Error::Config(format!("computer control has no runtime for {os}/{arch}"))
        })?;
    Ok((&distribution.platform, &distribution.checksum))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        path.metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn absolute_path(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(Error::Config(
            "computer resource paths must be absolute without parent traversal".into(),
        ));
    }
    valid_text(path.to_str().ok_or_else(invalid_command)?)
}

fn command_name(command: &str) -> Result<()> {
    valid_text(command)?;
    let path = Path::new(command);
    if !path.is_absolute()
        && (path.components().count() != 1
            || !matches!(
                path.components().next(),
                Some(std::path::Component::Normal(_))
            ))
    {
        return Err(invalid_command());
    }
    Ok(())
}

fn valid_text(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(invalid_command());
    }
    valid_argument(value)
}

fn valid_argument(value: &str) -> Result<()> {
    if value.len() > 4096 || value.chars().any(char::is_control) {
        return Err(invalid_command());
    }
    Ok(())
}

fn invalid_command() -> Error {
    Error::Config("invalid computer executable, argument, or path".into())
}

fn deadline(seconds: u64) -> Result<()> {
    crate::config::bounded("computer deadline", seconds, 1..=3600)
}

fn dimensions([width, height]: [u32; 2]) -> Result<()> {
    if !mobius::backend::session_files::ImagePresentation::default().fits(width, height) {
        let limits = mobius::backend::session_files::ImagePresentation::default();
        return Err(Error::Config(format!(
            "computer capture dimensions must be positive and fit {} pixels on the longest side and {} patches of 32 × 32 pixels; reduce the viewport, window size or desktop resolution",
            limits.max_dimension, limits.max_patches
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_browser_requires_an_explicit_sandbox_opt_out() {
        let mut browser = BrowserConfig::default();
        let error = browser.validate_root_sandbox(true).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("run the gateway as a non-root user")
        );
        assert!(
            error
                .to_string()
                .contains("computer.browser.sandbox = false")
        );
        browser.validate_root_sandbox(false).unwrap();
        browser.sandbox = false;
        browser.validate_root_sandbox(true).unwrap();
    }

    #[test]
    fn operator_arguments_cannot_replace_the_private_control_boundary() {
        let mut config = ComputerConfig::default();
        for argument in [
            "--remote-debugging-address=0.0.0.0",
            "--user-data-dir=/tmp/public",
            "--no-sandbox",
            "--remote-debugging-pipe",
        ] {
            config.browser.arguments = vec![argument.into()].into();
            assert!(config.validate().is_err(), "{argument}");
        }
    }
}
