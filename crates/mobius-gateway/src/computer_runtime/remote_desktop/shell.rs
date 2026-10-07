use std::fs;
use std::path::{Path, PathBuf};

use tokio::process::Command;

use super::processes::{OwnedProcess, private_directory, private_file};
use crate::computer_runtime::{ComputerConfig, config};
use crate::{Error, Result};
use config::ApplicationRole;

pub(super) async fn start(
    directory: &Path,
    display: &str,
    authority: &Path,
    chromium: &Path,
    profile: &Path,
    children: &mut Vec<OwnedProcess>,
    settings: &ComputerConfig,
) -> Result<()> {
    let branding = &settings.desktop.branding;
    let wallpaper = artwork(
        directory,
        "wallpaper.png",
        branding.wallpaper.as_deref(),
        include_bytes!("wallpaper.png"),
    )?;
    if let Some((program, arguments)) =
        config::application(&settings.desktop.wallpaper, ApplicationRole::Wallpaper)?
    {
        super::run_short(
            Command::new(program)
                .args(arguments)
                .arg(wallpaper)
                .env("DISPLAY", display)
                .env("XAUTHORITY", authority),
        )
        .await?;
    }
    let Some((panel, arguments)) =
        config::application(&settings.desktop.panel, ApplicationRole::Panel)?
    else {
        return Ok(());
    };
    let setpriv = config::resolve_executable(Path::new(settings.desktop.tools.setpriv.as_ref()))?;
    let config_directory = directory.join("config");
    private_directory(&config_directory)?;
    artwork(
        directory,
        "logo.png",
        branding.logo.as_deref(),
        include_bytes!("logo.png"),
    )?;
    let terminal_icon = artwork(
        directory,
        "terminal.png",
        branding.terminal_icon.as_deref(),
        include_bytes!("terminal.png"),
    )?;
    let files_icon = artwork(
        directory,
        "files.png",
        branding.files_icon.as_deref(),
        include_bytes!("files.png"),
    )?;
    let browser_icon = if let Some(source) = &branding.browser_icon {
        artwork(directory, "browser.png", Some(source), &[])?
            .display()
            .to_string()
    } else {
        chromium
            .parent()
            .map(|directory| directory.join("product_logo_48.png"))
            .filter(|path| path.is_file())
            .map_or_else(|| "web-browser".into(), |path| path.display().to_string())
    };
    launcher(
        directory,
        "browser",
        &branding.browser_label,
        &browser_icon,
        chromium,
        &settings.browser.headed_arguments(profile),
        &setpriv,
    )?;
    for (id, name, icon, application, role) in [
        (
            "terminal",
            branding.terminal_label.as_ref(),
            terminal_icon,
            &settings.desktop.terminal,
            ApplicationRole::Terminal,
        ),
        (
            "files",
            branding.files_label.as_ref(),
            files_icon,
            &settings.desktop.files,
            ApplicationRole::Files,
        ),
    ] {
        let destination = directory.join(format!("{id}.desktop"));
        if let Some((program, arguments)) = config::application(application, role)? {
            launcher(
                directory,
                id,
                name,
                &icon.display().to_string(),
                &program,
                arguments,
                &setpriv,
            )?;
        } else if destination.exists() {
            fs::remove_file(destination)?;
        }
    }
    for (name, source, bundled) in [
        (
            "status",
            branding.status_config.as_deref(),
            include_str!("status.tint2rc"),
        ),
        (
            "panel",
            branding.panel_config.as_deref(),
            include_str!("panel.tint2rc"),
        ),
    ] {
        let path = panel_config(directory, name, source, bundled)?;
        children.push(OwnedProcess::spawn(
            Command::new(&panel)
                .args(arguments)
                .arg("-c")
                .arg(path)
                .env("DISPLAY", display)
                .env("XAUTHORITY", authority)
                .env("XDG_CONFIG_HOME", &config_directory),
            directory.join(format!("{name}.json")),
        )?);
    }
    Ok(())
}

fn artwork(directory: &Path, name: &str, source: Option<&Path>, bundled: &[u8]) -> Result<PathBuf> {
    let path = directory.join(name);
    match source {
        Some(source) => {
            fs::copy(source, &path)?;
        }
        None => fs::write(&path, bundled)?,
    }
    private_file(&path)?;
    Ok(path)
}

fn launcher(
    directory: &Path,
    id: &str,
    name: &str,
    icon: &str,
    program: &Path,
    arguments: &[impl AsRef<str>],
    setpriv: &Path,
) -> Result<()> {
    let program = program.to_string_lossy();
    let setpriv = setpriv.to_string_lossy();
    let command = [setpriv.as_ref(), "--pdeathsig", "TERM", program.as_ref()]
        .into_iter()
        .chain(arguments.iter().map(AsRef::as_ref))
        .map(quote)
        .collect::<Result<Vec<_>>>()?
        .join(" ");
    let path = directory.join(format!("{id}.desktop"));
    // tint2 detaches launchers with setsid. exec preserves the panel parent,
    // so setpriv closes a launched app when its owned panel stops.
    fs::write(
        &path,
        format!(
            "[Desktop Entry]\nType=Application\nName={name}\nIcon={icon}\nExec=exec {command}\nTerminal=false\nStartupNotify=false\n"
        ),
    )?;
    private_file(&path)
}

fn panel_config(
    directory: &Path,
    name: &str,
    source: Option<&Path>,
    bundled: &str,
) -> Result<PathBuf> {
    let directory_text = directory.to_str().ok_or_else(invalid_path)?;
    if directory_text.contains(['\n', '\r']) {
        return Err(invalid_path());
    }
    let loaded = source.map(fs::read_to_string).transpose()?;
    let template = loaded.as_deref().unwrap_or(bundled);
    let template = template.replace("@DIRECTORY@", directory_text);
    let template = if source.is_none() {
        template
            .lines()
            .filter(|line| {
                line.strip_prefix("launcher_item_app = ")
                    .is_none_or(|path| Path::new(path).is_file())
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        template
    };
    let path = directory.join(format!("{name}.tint2rc"));
    fs::write(&path, template)?;
    private_file(&path)?;
    Ok(path)
}

fn quote(value: &str) -> Result<String> {
    if value.contains(['\n', '\r']) {
        return Err(invalid_path());
    }
    Ok(format!(
        "'{}'",
        value.replace('\'', "'\\''").replace('%', "%%")
    ))
}

fn invalid_path() -> Error {
    Error::Config("invalid desktop launcher path".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_components_do_not_require_desktop_packages() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = ComputerConfig::default();
        settings.desktop.wallpaper.enabled = false;
        settings.desktop.panel.enabled = false;
        let mut children = Vec::new();
        start(
            directory.path(),
            ":100",
            &directory.path().join("Xauthority"),
            Path::new("/unavailable/chromium"),
            &directory.path().join("profile"),
            &mut children,
            &settings,
        )
        .await
        .unwrap();
        assert!(children.is_empty());
    }

    #[test]
    fn bundled_panel_contains_only_available_launchers() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("browser.desktop"), "browser").unwrap();
        let panel = panel_config(
            directory.path(),
            "panel",
            None,
            include_str!("panel.tint2rc"),
        )
        .unwrap();
        let text = fs::read_to_string(panel).unwrap();
        assert!(text.contains("browser.desktop"));
        assert!(!text.contains("terminal.desktop"));
        assert!(!text.contains("files.desktop"));
    }

    #[test]
    fn launcher_keeps_configured_browser_switches_and_lifecycle_guard() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = ComputerConfig::default();
        settings.browser.sandbox = false;
        settings
            .browser
            .arguments
            .to_mut()
            .push("--force-color-profile=srgb".into());
        launcher(
            directory.path(),
            "browser",
            "Browser",
            "web-browser",
            Path::new("/opt/chromium"),
            &settings
                .browser
                .headed_arguments(Path::new("/private/profile")),
            Path::new("/usr/bin/setpriv"),
        )
        .unwrap();
        let text = fs::read_to_string(directory.path().join("browser.desktop")).unwrap();
        assert!(text.contains("--pdeathsig"));
        assert!(text.contains("--no-sandbox"));
        assert!(text.contains("--force-color-profile=srgb"));
        assert!(text.contains("--remote-debugging-address=127.0.0.1"));
    }
}
