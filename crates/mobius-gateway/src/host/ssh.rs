use std::env;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use sha2::{Digest as _, Sha256};
use tokio::process::Command;

use crate::wire::SshIdentityRecord;

use super::Rejection;

const GENERATED_KEY_LABEL: &str = "id_ed25519";
const KEYGEN_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_IDENTITIES: usize = 64;
const MAX_PUBLIC_KEY_BYTES: usize = 16 * 1024;
const MAX_ALGORITHM_BYTES: usize = 128;

// ponytail: one process-wide lock is enough for the single fixed key destination.
static SETUP_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(super) fn identities() -> std::result::Result<Vec<SshIdentityRecord>, Rejection> {
    identities_in(&ssh_directory()?)
}

pub(super) async fn generate() -> std::result::Result<(SshIdentityRecord, String), Rejection> {
    generate_in(&ssh_directory()?).await
}

fn ssh_directory() -> std::result::Result<PathBuf, Rejection> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .map(|path| path.join(".ssh"))
        .ok_or_else(|| ssh_error("the host home directory is unavailable"))
}

fn identities_in(directory: &Path) -> std::result::Result<Vec<SshIdentityRecord>, Rejection> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(ssh_error("failed to inspect host SSH identities")),
    };
    let mut identities = Vec::new();
    for entry in entries.flatten() {
        if identities.len() == MAX_IDENTITIES {
            break;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(label) = name.strip_suffix(".pub") else {
            continue;
        };
        if label.is_empty() {
            continue;
        }
        let Ok(public_key) = read_public_key(&entry.path()) else {
            continue;
        };
        if let Some((identity, _)) = parse_public_key(label, &public_key) {
            identities.push(identity);
        }
    }
    identities.sort_unstable_by(|left, right| left.label.cmp(&right.label));
    Ok(identities)
}

async fn generate_in(
    directory: &Path,
) -> std::result::Result<(SshIdentityRecord, String), Rejection> {
    let _guard = SETUP_LOCK.lock().await;
    protect_directory(directory)?;
    let destination = directory.join(GENERATED_KEY_LABEL);
    ensure_destination_available(&destination)?;

    let temporary = tempfile::Builder::new()
        .prefix(".mobius-keygen-")
        .tempdir_in(directory)
        .map_err(|_| ssh_error("failed to prepare SSH key generation"))?;
    let private_key = temporary.path().join("key");
    let status = tokio::time::timeout(KEYGEN_TIMEOUT, async {
        Command::new("ssh-keygen")
            .arg("-q")
            .arg("-t")
            .arg("ed25519")
            .arg("-N")
            .arg("")
            .arg("-C")
            .arg("mobius")
            .arg("-f")
            .arg(&private_key)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await
    })
    .await
    .map_err(|_| ssh_error("SSH key generation exceeded 10 seconds"))?
    .map_err(|_| ssh_error("ssh-keygen is unavailable on the host"))?;
    if !status.success() {
        return Err(ssh_error("ssh-keygen failed to generate an Ed25519 key"));
    }
    install_keypair(&private_key, &destination)
}

fn protect_directory(directory: &Path) -> std::result::Result<(), Rejection> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(ssh_error(
                "the host SSH directory is not a regular directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(directory)
                .map_err(|_| ssh_error("failed to create the host SSH directory"))?;
        }
        Err(_) => return Err(ssh_error("failed to inspect the host SSH directory")),
    }
    set_mode(directory, 0o700)
}

fn ensure_destination_available(destination: &Path) -> std::result::Result<(), Rejection> {
    if path_exists(destination) || path_exists(&destination.with_extension("pub")) {
        return Err(Rejection {
            code: "ssh_identity_exists",
            message: format!("SSH identity {GENERATED_KEY_LABEL} already exists on the host"),
            fatal: false,
        });
    }
    Ok(())
}

fn install_keypair(
    temporary_private: &Path,
    destination_private: &Path,
) -> std::result::Result<(SshIdentityRecord, String), Rejection> {
    ensure_destination_available(destination_private)?;
    let temporary_public = temporary_private.with_extension("pub");
    set_mode(temporary_private, 0o600)?;
    set_mode(&temporary_public, 0o644)?;
    let public_key = read_public_key(&temporary_public)
        .map_err(|_| ssh_error("ssh-keygen returned an invalid public key"))?;
    let (identity, public_key) = parse_public_key(GENERATED_KEY_LABEL, &public_key)
        .ok_or_else(|| ssh_error("ssh-keygen returned an invalid public key"))?;

    fs::hard_link(temporary_private, destination_private).map_err(|_| identity_install_error())?;
    let destination_public = destination_private.with_extension("pub");
    if fs::hard_link(&temporary_public, &destination_public).is_err() {
        let _ = fs::remove_file(destination_private);
        return Err(identity_install_error());
    }
    Ok((identity, public_key))
}

fn parse_public_key(label: &str, value: &[u8]) -> Option<(SshIdentityRecord, String)> {
    if value.is_empty() || value.len() > MAX_PUBLIC_KEY_BYTES {
        return None;
    }
    let line = std::str::from_utf8(value)
        .ok()?
        .trim_end_matches(['\r', '\n']);
    if line.is_empty() || line.contains(['\r', '\n', '\0']) {
        return None;
    }
    let mut fields = line.split_ascii_whitespace();
    let algorithm = fields.next()?;
    let encoded = fields.next()?;
    if algorithm.is_empty() || algorithm.len() > MAX_ALGORITHM_BYTES {
        return None;
    }
    let blob = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    if blob_algorithm(&blob)? != algorithm {
        return None;
    }
    let fingerprint =
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(Sha256::digest(&blob));
    Some((
        SshIdentityRecord {
            label: label.into(),
            algorithm: algorithm.into(),
            fingerprint: format!("SHA256:{fingerprint}"),
        },
        line.into(),
    ))
}

fn blob_algorithm(blob: &[u8]) -> Option<&str> {
    let length = usize::try_from(u32::from_be_bytes(blob.get(..4)?.try_into().ok()?)).ok()?;
    if length == 0 || length > MAX_ALGORITHM_BYTES || blob.len() <= 4 + length {
        return None;
    }
    std::str::from_utf8(blob.get(4..4 + length)?).ok()
}

fn read_public_key(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular public key"));
    }
    let mut value = Vec::new();
    file.take((MAX_PUBLIC_KEY_BYTES + 1) as u64)
        .read_to_end(&mut value)?;
    if value.len() > MAX_PUBLIC_KEY_BYTES {
        return Err(std::io::Error::other("public key is too large"));
    }
    Ok(value)
}

fn path_exists(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::result::Result<(), Rejection> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|_| ssh_error("failed to protect host SSH credentials"))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> std::result::Result<(), Rejection> {
    Ok(())
}

fn identity_install_error() -> Rejection {
    Rejection {
        code: "ssh_identity_exists",
        message: format!(
            "SSH identity {GENERATED_KEY_LABEL} could not be installed without overwriting a host file"
        ),
        fatal: false,
    }
}

fn ssh_error(message: impl Into<String>) -> Rejection {
    Rejection {
        code: "ssh_error",
        message: message.into(),
        fatal: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public_key(comment: &str) -> String {
        let algorithm = b"ssh-ed25519";
        let mut blob = u32::try_from(algorithm.len())
            .expect("algorithm length")
            .to_be_bytes()
            .to_vec();
        blob.extend_from_slice(algorithm);
        blob.extend_from_slice(&32_u32.to_be_bytes());
        blob.extend_from_slice(&[7; 32]);
        format!(
            "ssh-ed25519 {} {comment}",
            base64::engine::general_purpose::STANDARD.encode(blob)
        )
    }

    #[test]
    fn public_key_summary_is_bounded_and_ignores_comments() {
        let line = public_key("private workstation comment");
        let (identity, returned) = parse_public_key("id_test", line.as_bytes()).expect("identity");

        assert_eq!(identity.label, "id_test");
        assert_eq!(identity.algorithm, "ssh-ed25519");
        assert!(identity.fingerprint.starts_with("SHA256:"));
        assert!(!identity.fingerprint.contains("workstation"));
        assert_eq!(returned, line);
        assert!(parse_public_key("id_test", &[b'x'; MAX_PUBLIC_KEY_BYTES + 1]).is_none());
        assert!(parse_public_key("id_test", b"ssh-rsa AAAA").is_none());
    }

    #[test]
    fn inventory_exposes_only_public_summaries() {
        let directory = tempfile::tempdir().expect("SSH directory");
        fs::write(directory.path().join("id_test"), "PRIVATE MATERIAL").expect("private key");
        let public = public_key("secret comment");
        fs::write(directory.path().join("id_test.pub"), &public).expect("public key");

        let identities = identities_in(directory.path()).expect("identities");
        let serialized = serde_json::to_string(&identities).expect("serialize identities");

        assert_eq!(identities.len(), 1);
        assert!(!serialized.contains("PRIVATE MATERIAL"));
        assert!(!serialized.contains("secret comment"));
        assert!(!serialized.contains(&public));
        assert!(!serialized.contains(&directory.path().display().to_string()));
    }

    #[tokio::test]
    async fn existing_identity_is_rejected_without_overwrite() {
        let directory = tempfile::tempdir().expect("SSH directory");
        let path = directory.path().join(GENERATED_KEY_LABEL);
        fs::write(&path, "PRIVATE MATERIAL").expect("existing identity");

        assert!(generate_in(directory.path()).await.is_err());
        assert_eq!(
            fs::read_to_string(path).expect("existing identity"),
            "PRIVATE MATERIAL"
        );
    }

    #[cfg(unix)]
    #[test]
    fn keypair_install_is_create_new_and_sets_owner_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("SSH directory");
        let temporary = tempfile::tempdir_in(directory.path()).expect("temporary key directory");
        let temporary_private = temporary.path().join("key");
        fs::write(&temporary_private, "PRIVATE MATERIAL").expect("private key");
        fs::write(
            temporary_private.with_extension("pub"),
            public_key("mobius"),
        )
        .expect("public key");
        let destination = directory.path().join(GENERATED_KEY_LABEL);

        let (_, returned_public) =
            install_keypair(&temporary_private, &destination).expect("install keypair");

        assert!(!returned_public.contains("PRIVATE MATERIAL"));
        assert_eq!(
            fs::metadata(&destination)
                .expect("private metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(destination.with_extension("pub"))
                .expect("public metadata")
                .permissions()
                .mode()
                & 0o777,
            0o644
        );

        let replacement = tempfile::tempdir_in(directory.path()).expect("replacement directory");
        let replacement_private = replacement.path().join("key");
        fs::write(&replacement_private, "REPLACEMENT").expect("replacement private key");
        fs::write(
            replacement_private.with_extension("pub"),
            public_key("replacement"),
        )
        .expect("replacement public key");
        assert!(install_keypair(&replacement_private, &destination).is_err());
        assert_eq!(
            fs::read_to_string(&destination).expect("installed private key"),
            "PRIVATE MATERIAL"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ssh_directory_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let parent = tempfile::tempdir().expect("home");
        let directory = parent.path().join(".ssh");
        protect_directory(&directory).expect("protect directory");

        assert_eq!(
            fs::metadata(directory)
                .expect("SSH metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}
