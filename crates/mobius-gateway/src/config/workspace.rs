use super::*;

pub(crate) fn prepare_background_workspace(
    state_dir: &Path,
    tls: Option<&TlsConfig>,
) -> Result<PathBuf> {
    let state_dir = fs::canonicalize(state_dir)?;
    let mut name = state_dir
        .file_name()
        .ok_or_else(|| Error::Config("gateway state directory must have a name".into()))?
        .to_os_string();
    name.push(".background");
    let path = state_dir
        .parent()
        .ok_or_else(|| Error::Config("gateway state directory must have a parent".into()))?
        .join(name);
    let created = match fs::create_dir(&path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error.into()),
    };
    let prepared = (|| {
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_dir() {
            return Err(Error::Config(
                "gateway background workspace must be a real directory".into(),
            ));
        }
        #[cfg(unix)]
        if created {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        } else if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::Config(
                "gateway background workspace must not be accessible by group or others (use mode 0700)"
                    .into(),
            ));
        }
        let path = validate_chat_workspace(&path, &state_dir, tls)?;
        initialize_workspace_repository(&path)?;
        Ok(path)
    })();
    if created && prepared.is_err() {
        let _ = fs::remove_dir_all(&path);
    }
    prepared
}

pub(super) fn validate_chat_workspace(
    path: &Path,
    state_dir: &Path,
    tls: Option<&TlsConfig>,
) -> Result<PathBuf> {
    let path = fs::canonicalize(path)?;
    if !path.is_dir() || path.parent().is_none() {
        return Err(Error::Config(
            "workspace must be an existing non-root directory".into(),
        ));
    }
    validate_workspace_boundaries(&path, state_dir, tls)?;
    Ok(path)
}

pub(crate) fn create_workspace_directory(
    parent: &Path,
    name: &str,
    state_dir: &Path,
    tls: Option<&TlsConfig>,
) -> Result<PathBuf> {
    let parent = fs::canonicalize(parent)?;
    if !parent.is_dir() {
        return Err(Error::Config("workspace parent must be a directory".into()));
    }

    let name = name.trim();
    if name.is_empty()
        || name.len() > MAX_WORKSPACE_DIRECTORY_NAME_BYTES
        || name.as_bytes().contains(&0)
        || name.bytes().any(|byte| byte == b'/' || byte == b'\\')
    {
        return Err(Error::Config(format!(
            "workspace directory name must be 1–{MAX_WORKSPACE_DIRECTORY_NAME_BYTES} bytes and contain no path separators"
        )));
    }
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(Error::Config(
            "workspace directory name must be one path component".into(),
        ));
    }

    let path = parent.join(name);
    validate_workspace_boundaries(&path, state_dir, tls)?;
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            return Err(Error::Config("workspace directory already exists".into()));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    fs::create_dir(&path)?;
    let created = validate_chat_workspace(&path, state_dir, tls).and_then(|path| {
        initialize_workspace_repository(&path)?;
        Ok(path)
    });
    match created {
        Ok(path) => Ok(path),
        Err(error) => {
            let _ = fs::remove_dir_all(&path);
            Err(error)
        }
    }
}

fn initialize_workspace_repository(path: &Path) -> Result<()> {
    let mut command = std::process::Command::new("git");
    command
        .args(["init", "--quiet", "--initial-branch", "main"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .current_dir(path);
    for name in crate::sandbox::REPOSITORY_LOCAL_GIT_ENVIRONMENT {
        command.env_remove(name);
    }
    let output = command.output()?;
    if !output.status.success() {
        return Err(Error::Config(
            "failed to initialize workspace Git repository".into(),
        ));
    }
    Ok(())
}

fn validate_workspace_boundaries(
    path: &Path,
    state_dir: &Path,
    tls: Option<&TlsConfig>,
) -> Result<()> {
    let state_dir = fs::canonicalize(state_dir)?;
    if path.starts_with(&state_dir) || state_dir.starts_with(path) {
        return Err(Error::Config(
            "gateway state directory and chat workspace must not overlap".into(),
        ));
    }
    let extensions = crate::extensions::extensions_path(&state_dir);
    if path.starts_with(&extensions)
        || extensions.starts_with(path)
        || fs::canonicalize(&extensions)
            .is_ok_and(|extensions| path.starts_with(&extensions) || extensions.starts_with(path))
    {
        return Err(Error::Config(
            "extension store and chat workspace must not overlap".into(),
        ));
    }
    if tls.is_some_and(|tls| {
        fs::canonicalize(&tls.private_key).is_ok_and(|key| key.starts_with(path))
    }) {
        return Err(Error::Config(
            "TLS private key must be stored outside every chat workspace".into(),
        ));
    }
    Ok(())
}

pub(super) fn workspace_id(path: &Path) -> String {
    let digest = sha2::Sha256::digest(path.as_os_str().as_encoded_bytes());
    let mut id = String::from("path-v1:");
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut id, "{byte:02x}").expect("writing to a string cannot fail");
    }
    id
}

pub(crate) fn local_user_name() -> Option<String> {
    ["USER", "USERNAME"]
        .into_iter()
        .find_map(|name| env::var(name).ok().filter(|value| !value.trim().is_empty()))
}
