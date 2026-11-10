//! This Mac's own files, outside any gateway: path routing, previews, saves, the app's
//! private folder and thumbnails.

use mobius_gateway::wire::WorkspaceFileRecord;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// The app's private folder for pasted images, previews and thumbnails, removed on quit.
pub use tempfile::TempDir as PrivateDir;

pub fn private_dir() -> Option<PrivateDir> {
    tempfile::tempdir()
        .inspect_err(|error| tracing::warn!(%error, "no private folder for previews"))
        .ok()
}

fn home() -> Option<PathBuf> {
    std::env::home_dir()
}

/// The stable identity of this Mac's gateway state, including a resolved directory alias.
pub fn gateway_identity() -> Option<String> {
    let path = mobius_gateway::config::state_dir().ok()?;
    Some(format!(
        "local:{}",
        path.canonicalize().unwrap_or(path).display()
    ))
}

/// Whether a folder is available on this Mac.
pub fn is_directory(path: &Path) -> bool {
    path.is_dir()
}

/// The folder a terminal may start in, or why none can: shells run on this Mac only.
///
/// # Errors
///
/// When the gateway is remote or the folder is missing, relative or unavailable.
pub fn terminal_folder(local: bool, folder: Option<&Path>) -> Result<PathBuf, &'static str> {
    if !local {
        return Err("Terminals run on this Mac; this chat's gateway is remote.");
    }
    let folder = folder.ok_or("Open a chat to start a terminal in its folder.")?;
    if folder.is_absolute() && folder.is_dir() {
        Ok(folder.to_path_buf())
    } else {
        Err("This chat's folder isn't on this Mac.")
    }
}

/// Routes a chat file through its gateway or this Mac, resolving local directory aliases.
pub fn route_file(path: String, workspace: Option<&Path>, local: bool) -> FileRoute {
    let home = local.then(home).flatten();
    resolve_file(path, workspace, local, home.as_deref(), Path::is_file)
}

/// Where a file named in the chat opens from.
#[derive(Debug, PartialEq, Eq)]
pub enum FileRoute {
    /// An absolute or workspace-relative file path the gateway reads.
    Gateway(String),
    /// A file on this Mac, read directly.
    Local(PathBuf),
    /// Neither can open it; the message says why in plain words.
    Unavailable(String),
}

/// Files inside the chat's folder open through the gateway by relative path. Anything else
/// opens from this Mac when the gateway runs here, and from the gateway's machine otherwise.
fn resolve_file(
    path: String,
    workspace: Option<&Path>,
    local: bool,
    home: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> FileRoute {
    if !local {
        return FileRoute::Gateway(path);
    }
    let name = Path::new(&path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&path);
    let expanded = match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(&path),
    };
    let outside = |absolute: Option<PathBuf>| match absolute {
        Some(absolute) if exists(&absolute) => FileRoute::Local(absolute),
        _ => FileRoute::Unavailable(format!("“{name}” isn’t on this Mac.")),
    };
    if expanded.is_relative() {
        let relative = normalize(&expanded);
        if !relative.starts_with("..") {
            return FileRoute::Gateway(relative.to_string_lossy().into_owned());
        }
        return outside(workspace.map(|workspace| normalize(&workspace.join(&relative))));
    }
    let absolute = normalize(&expanded);
    let Some(workspace) = workspace else {
        return if exists(&absolute) {
            FileRoute::Local(absolute)
        } else {
            FileRoute::Gateway(absolute.to_string_lossy().into_owned())
        };
    };
    let inside = |root: &Path, file: &Path| {
        file.strip_prefix(root)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .map(Path::to_path_buf)
    };
    let relative = inside(workspace, &absolute).or_else(|| {
        // A local folder may be named through a symlink, such as /tmp for /private/tmp.
        let root = workspace.canonicalize().ok()?;
        inside(&root, &absolute.canonicalize().ok()?)
    });
    match relative {
        Some(relative) => FileRoute::Gateway(relative.to_string_lossy().into_owned()),
        None => outside(Some(absolute)),
    }
}

/// Resolves `.` and `..` by name; a relative path keeps the `..` that climb above it.
fn normalize(path: &Path) -> PathBuf {
    let mut parts: Vec<Component> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match parts.last() {
                Some(Component::Normal(_)) => {
                    parts.pop();
                }
                Some(Component::RootDir) => {}
                _ => parts.push(component),
            },
            component => parts.push(component),
        }
    }
    parts.iter().collect()
}

/// A file's bytes, or `None` when it is larger than `limit`.
///
/// # Errors
///
/// When the file cannot be opened or read.
pub fn read_capped(path: &Path, limit: u64) -> std::io::Result<Option<Vec<u8>>> {
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > limit {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

/// Writes `bytes` to `destination` and hands the path back.
///
/// # Errors
///
/// When the file cannot be written.
pub fn save(destination: PathBuf, bytes: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::write(&destination, bytes).map(|()| destination)
}

/// Copies `source` to `destination` and hands the path back.
///
/// # Errors
///
/// When `source` cannot be read or `destination` written.
pub fn copy(source: &Path, destination: PathBuf) -> std::io::Result<PathBuf> {
    std::fs::copy(source, &destination).map(|_| destination)
}

/// Removes a file the app made; one already gone is not an error worth more than a log.
pub fn discard(path: &Path) {
    if let Err(error) = std::fs::remove_file(path) {
        tracing::warn!(%error, path = %path.display(), "could not remove a private file");
    }
}

/// The bytes the files at `paths` take.
///
/// # Errors
///
/// When a file's size cannot be read, or the total overflows.
pub fn total_size<'p>(paths: impl IntoIterator<Item = &'p Path>) -> std::io::Result<u64> {
    paths.into_iter().try_fold(0_u64, |total, path| {
        let size = std::fs::metadata(path)?.len();
        total
            .checked_add(size)
            .ok_or_else(|| std::io::Error::other("cache size overflow"))
    })
}

/// The local gateway's public endpoint, from the process record it keeps beside its state.
pub fn public_endpoint() -> Option<String> {
    let path = mobius_gateway::config::state_dir()
        .ok()?
        .join("gateway-process.json");
    let record: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    Some(record.get("endpoint")?.as_str()?.to_owned())
}

/// Bounded, background-only folder listing; never follows directory symlinks.
///
/// # Errors
///
/// When a folder or an entry in it cannot be read.
pub fn list(root: &Path) -> std::io::Result<(Vec<WorkspaceFileRecord>, bool)> {
    const LIMIT: usize = 4096;
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    let mut visited = 0;
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            visited += 1;
            if visited > LIMIT {
                return Ok((files, true));
            }
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file()
                && let Ok(path) = entry.path().strip_prefix(root)
            {
                files.push(WorkspaceFileRecord {
                    path: path.to_string_lossy().into_owned(),
                    size: entry.metadata()?.len(),
                });
            }
        }
    }
    Ok((files, false))
}

#[cfg(test)]
mod tests {
    #[test]
    fn lists_relative_files_without_following_symlinks_or_hidden_folders() {
        let directory = tempfile::tempdir().expect("temporary folder");
        let root = directory.path();
        std::fs::create_dir(root.join("nested")).expect("nested folder");
        std::fs::create_dir(root.join(".hidden")).expect("hidden folder");
        std::fs::write(root.join("nested/visible.txt"), b"hello").expect("visible file");
        std::fs::write(root.join(".hidden/private.txt"), b"hidden").expect("hidden file");
        std::os::unix::fs::symlink(root, root.join("nested/loop")).expect("symlink");
        let (files, truncated) = super::list(root).expect("local tree");
        assert!(!truncated);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "nested/visible.txt");
        assert_eq!(files[0].size, 5);
    }
}

#[cfg(test)]
mod routing_tests {
    use super::{FileRoute, resolve_file as route_file};
    use std::path::{Path, PathBuf};

    fn route(path: &str, workspace: Option<&str>, local: bool) -> FileRoute {
        let exists = |path: &Path| path.starts_with("/Users/me/.mobius");
        route_file(
            path.into(),
            workspace.map(Path::new),
            local,
            Some(Path::new("/Users/me")),
            exists,
        )
    }

    #[test]
    fn local_files_resolve_here_and_remote_paths_stay_on_the_gateway() {
        let workspace = Some("/Users/me/project");
        let skill = "/Users/me/.mobius/skills/review/SKILL.md";
        for path in [
            skill,
            "~/.mobius/skills/review/SKILL.md",
            "../.mobius/skills/review/SKILL.md",
        ] {
            assert_eq!(
                route(path, workspace, true),
                FileRoute::Local(PathBuf::from(skill))
            );
        }
        assert_eq!(
            route(skill, None, true),
            FileRoute::Local(PathBuf::from(skill))
        );
        for path in [skill, "~/.mobius/skills/review/SKILL.md", "../../etc/hosts"] {
            assert_eq!(
                route_file(
                    path.into(),
                    workspace.map(Path::new),
                    false,
                    Some(Path::new("/Users/me")),
                    |_| panic!("a remote file must not be inspected on this Mac")
                ),
                FileRoute::Gateway(path.into())
            );
        }
        assert!(matches!(
            route("/Users/me/gone.md", workspace, true),
            FileRoute::Unavailable(message) if message == "“gone.md” isn’t on this Mac."
        ));
    }

    #[test]
    fn directory_aliases_keep_workspace_files_on_the_gateway() {
        let directory = tempfile::tempdir().expect("temporary folder");
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace folder");
        let file = workspace.join("file.txt");
        std::fs::write(&file, "content").expect("workspace file");
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&workspace, &alias).expect("workspace alias");
        let absolute = file.canonicalize().expect("absolute file");
        assert_eq!(
            super::route_file(absolute.to_string_lossy().into_owned(), Some(&alias), true),
            FileRoute::Gateway("file.txt".into())
        );
        assert!(matches!(
            super::route_file(absolute.to_string_lossy().into_owned(), None, true),
            FileRoute::Local(path) if path == absolute
        ));
    }
}

#[cfg(test)]
mod terminal_tests {
    use super::terminal_folder;
    use std::path::Path;

    #[test]
    fn terminals_need_the_local_gateway_and_an_existing_folder() {
        let here = std::env::temp_dir();
        assert_eq!(terminal_folder(true, Some(&here)), Ok(here.clone()));
        assert!(terminal_folder(false, Some(&here)).is_err());
        assert!(terminal_folder(true, None).is_err());
        assert!(terminal_folder(true, Some(Path::new("."))).is_err());
        assert!(terminal_folder(true, Some(Path::new("/nonexistent/mobius"))).is_err());
    }
}
