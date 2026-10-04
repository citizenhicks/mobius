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
            .map_err(|_| Error::Config("invalid Node download mirror".into()))?;
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
            .map_err(|_| Error::Config("invalid browser start page".into()))?;
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
#[cfg(any(target_os = "linux", test))]
pub(crate) enum ApplicationRole {
    WindowManager,
    Panel,
    Wallpaper,
    Terminal,
    Files,
}

#[cfg(any(target_os = "linux", test))]
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
    if width == 0
        || height == 0
        || width > 16384
        || height > 16384
        || u64::from(width) * u64::from(height) > 64 * 1024 * 1024
    {
        return Err(Error::Config("invalid computer viewport dimensions".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_defaults_borrow_immutable_storage() {
        let defaults = ComputerConfig::default();
        assert!(matches!(defaults.node_download_base_url, Cow::Borrowed(_)));
        assert!(matches!(defaults.tar_executable, Cow::Borrowed(_)));
        assert!(matches!(defaults.browser.arguments, Cow::Borrowed(_)));
        assert!(matches!(defaults.browser.start_page, Cow::Borrowed(_)));
        assert!(matches!(defaults.desktop.tools.xauth, Cow::Borrowed(_)));
        assert!(matches!(defaults.desktop.tools.xvnc, Cow::Borrowed(_)));
        assert!(matches!(defaults.desktop.tools.vncconfig, Cow::Borrowed(_)));
        assert!(matches!(defaults.desktop.tools.setpriv, Cow::Borrowed(_)));
        assert!(matches!(
            defaults.desktop.branding.browser_label,
            Cow::Borrowed(_)
        ));
        assert!(matches!(
            defaults.desktop.branding.terminal_label,
            Cow::Borrowed(_)
        ));
        assert!(matches!(
            defaults.desktop.branding.files_label,
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn mutating_a_borrowed_default_preserves_other_operator_configurations() {
        let mut defaults = ComputerConfig::default();
        defaults
            .browser
            .arguments
            .to_mut()
            .push("--mute-audio".into());
        assert!(matches!(defaults.browser.arguments, Cow::Owned(_)));
        assert!(
            !ComputerConfig::default()
                .browser
                .arguments
                .iter()
                .any(|argument| argument == "--mute-audio")
        );
    }

    #[test]
    fn configured_values_own_their_storage_and_remain_editable() {
        let mut operator: ComputerConfig = toml::from_str(
            "[browser]\narguments = ['--mute-audio']\nstart_page = 'https://example.invalid'",
        )
        .unwrap();
        assert!(matches!(operator.browser.arguments, Cow::Owned(_)));
        assert!(matches!(operator.browser.start_page, Cow::Owned(_)));
        operator.browser.arguments.to_mut().clear();
        operator.browser.start_page.to_mut().push_str("/start");
        operator.validate().unwrap();
    }

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
    fn typed_defaults_preserve_pinned_runtime_and_desktop_policy() {
        let defaults = ComputerConfig::default();
        assert_eq!(defaults.install_timeout_seconds, 600);
        assert_eq!(defaults.browser.window_size, [1200, 640]);
        assert_eq!(defaults.browser.window_position, [80, 42]);
        assert_eq!(defaults.browser.viewport, [1365, 768]);
        assert_eq!(defaults.desktop.resolution, [1365, 768]);
        assert_eq!(defaults.desktop.depth, 24);
        assert_eq!(
            (defaults.desktop.display_start, defaults.desktop.display_end),
            (100, 200)
        );
        assert_eq!(defaults.desktop.tools.xvnc.as_ref(), ["Xtigervnc", "Xvnc"]);
        assert_eq!(node_version(), "26.8.1");
        assert_eq!(
            node_distribution("linux", "aarch64").unwrap(),
            (
                "linux-arm64",
                "d5f973ce975e4bd03e6c2038260f7e9201615aa8e1ee293c72f8dcc2a6d9fddb"
            )
        );
        assert_eq!(
            node_distribution("linux", "x86_64").unwrap(),
            (
                "linux-x64",
                "b2b76660fa4ded4e0b2a41ee3c0c651cd52ea8170ead91ebac1e147ac3d55643"
            )
        );
        assert_eq!(
            node_distribution("macos", "aarch64").unwrap(),
            (
                "darwin-arm64",
                "6e577fd0d9db776db82306629e441a9dace416702622aebdd171c9dfaa41f4d2"
            )
        );
        assert_eq!(
            node_distribution("macos", "x86_64").unwrap(),
            (
                "darwin-x64",
                "fe9c6dbf9c8e1b4443803d75e2a20366e420dae650c747dbb116b22975751baf"
            )
        );
    }

    #[test]
    fn embedded_defaults_require_operational_fields_without_public_default_recursion() {
        let incomplete = DEFAULTS_TEXT.replace("viewport = [1365, 768]", "");
        let error = toml::from_str::<ComputerDefaults>(&incomplete)
            .err()
            .unwrap();
        assert!(error.to_string().contains("viewport"));
    }

    #[test]
    fn partial_policy_keeps_manifest_defaults_and_requires_explicit_sandbox_disable() {
        let mut config: ComputerConfig =
            toml::from_str("[browser]\nviewport = [800, 600]\n").unwrap();
        config.validate().unwrap();
        assert_eq!(config.browser.viewport, [800, 600]);
        assert!(
            !config
                .browser
                .headed_arguments(Path::new("/tmp/profile"))
                .contains(&"--no-sandbox".into())
        );
        config.browser.sandbox = false;
        assert!(
            config
                .browser
                .headed_arguments(Path::new("/tmp/profile"))
                .contains(&"--no-sandbox".into())
        );
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

    #[test]
    fn disabled_or_missing_default_application_is_optional() {
        let application = DesktopApplication {
            enabled: false,
            ..DesktopApplication::default()
        };
        for role in [
            ApplicationRole::WindowManager,
            ApplicationRole::Panel,
            ApplicationRole::Wallpaper,
            ApplicationRole::Terminal,
            ApplicationRole::Files,
        ] {
            assert!(super::application(&application, role).unwrap().is_none());
        }
        assert!(resolve_executable(Path::new("relative/path")).is_err());
    }
}
