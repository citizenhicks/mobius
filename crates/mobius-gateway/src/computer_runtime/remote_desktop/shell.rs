use std::fs;
use std::path::Path;

use tokio::process::Command;

use super::processes::{OwnedProcess, executable, private_directory, private_file};
use crate::{Error, Result};

pub(super) async fn start(
    directory: &Path,
    display: &str,
    authority: &Path,
    chromium: &Path,
    profile: &Path,
    children: &mut Vec<OwnedProcess>,
) -> Result<()> {
    let config = directory.join("config");
    private_directory(&config)?;
    let wallpaper = directory.join("wallpaper.png");
    fs::write(&wallpaper, include_bytes!("wallpaper.png"))?;
    private_file(&wallpaper)?;
    let logo = directory.join("logo.png");
    fs::write(&logo, include_bytes!("logo.png"))?;
    private_file(&logo)?;
    super::run_short(
        Command::new(executable(&["feh"])?)
            .args(["--no-fehbg", "--bg-fill"])
            .arg(&wallpaper)
            .env("DISPLAY", display)
            .env("XAUTHORITY", authority),
    )
    .await?;

    let setpriv = executable(&["setpriv"])?;
    let icon = chromium
        .parent()
        .map(|directory| directory.join("product_logo_48.png"))
        .filter(|path| path.is_file())
        .map_or_else(|| "web-browser".into(), |path| path.display().to_string());
    for (id, name, icon, program, arguments) in [
        (
            "chrome",
            "Chrome",
            icon,
            chromium.to_path_buf(),
            super::browser_arguments(profile),
        ),
        (
            "terminal",
            "Terminal",
            "/usr/share/pixmaps/xterm-color_48x48.xpm".into(),
            executable(&["xterm"])?,
            vec![
                "-title".into(),
                "Terminal".into(),
                "-fa".into(),
                "Liberation Mono".into(),
                "-fs".into(),
                "12".into(),
            ],
        ),
        (
            "files",
            "Files",
            "folder".into(),
            executable(&["pcmanfm"])?,
            vec!["--new-win".into(), "--profile=mobius".into()],
        ),
    ] {
        let command = [
            setpriv.display().to_string(),
            "--pdeathsig".into(),
            "TERM".into(),
            program.display().to_string(),
        ]
        .into_iter()
        .chain(arguments)
        .map(|argument| quote(&argument))
        .collect::<Result<Vec<_>>>()?
        .join(" ");
        let path = directory.join(format!("{id}.desktop"));
        // tint2 detaches launchers with setsid. exec keeps its child as the native
        // app's parent, so setpriv closes that app when the owned panel stops.
        fs::write(
            &path,
            format!(
                "[Desktop Entry]\nType=Application\nName={name}\nIcon={icon}\nExec=exec {command}\nTerminal=false\nStartupNotify=false\n"
            ),
        )?;
        private_file(&path)?;
    }
    let directory_text = directory.to_str().ok_or_else(invalid_path)?;
    if directory_text.contains(['\n', '\r']) {
        return Err(invalid_path());
    }
    for (name, template) in [
        ("status", include_str!("status.tint2rc")),
        ("panel", include_str!("panel.tint2rc")),
    ] {
        let path = directory.join(format!("{name}.tint2rc"));
        fs::write(&path, template.replace("@DIRECTORY@", directory_text))?;
        private_file(&path)?;
        children.push(OwnedProcess::spawn(
            Command::new(executable(&["tint2"])?)
                .arg("-c")
                .arg(&path)
                .env("DISPLAY", display)
                .env("XAUTHORITY", authority)
                .env("XDG_CONFIG_HOME", &config),
            directory.join(format!("{name}.json")),
        )?);
    }
    Ok(())
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
