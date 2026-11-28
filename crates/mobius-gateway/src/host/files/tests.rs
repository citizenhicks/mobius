use super::*;

use crate::host::tests::test_sandbox;

#[tokio::test]
async fn read_opens_relative_and_absolute_paths_in_ranges() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (_state, sandbox) = test_sandbox(workspace.path(), Duration::from_secs(5));
    let home = std::env::home_dir().expect("gateway home");
    let elsewhere = tempfile::tempdir_in(&home).expect("elsewhere");
    std::fs::write(workspace.path().join("main.rs"), b"fn main() {}").expect("file");
    std::fs::write(elsewhere.path().join("fetch_images.py"), b"print(1)").expect("file");
    let file = elsewhere
        .path()
        .join("fetch_images.py")
        .display()
        .to_string();
    let root = Some(workspace.path());

    let first = read(&sandbox, root, &file, 0, 5)
        .await
        .expect("first range");
    assert_eq!(
        (first.data.as_slice(), first.next_offset),
        (&b"print"[..], Some(5))
    );
    let rest = read(&sandbox, None, &file, 5, 64).await.expect("rest");
    assert_eq!(
        (rest.data.as_slice(), rest.next_offset),
        (&b"(1)"[..], None)
    );
    let relative = elsewhere.path().strip_prefix(&home).expect("home-relative");
    let home_path = format!("~/{}/fetch_images.py", relative.display());
    assert_eq!(
        read(&sandbox, None, &home_path, 0, 64)
            .await
            .expect("home path")
            .data,
        b"print(1)"
    );
    assert_eq!(
        read(&sandbox, root, "main.rs", 0, 64)
            .await
            .expect("relative")
            .data,
        b"fn main() {}"
    );
    assert!(read(&sandbox, None, "main.rs", 0, 64).await.is_err());
    assert!(read(&sandbox, root, "missing.rs", 0, 64).await.is_err());
    assert!(read(&sandbox, root, &file, 9, 64).await.is_err());
}

#[tokio::test]
#[expect(
    clippy::significant_drop_tightening,
    reason = "the sandbox serves every read in the test"
)]
async fn host_reads_follow_safe_links_and_reject_protected_paths_and_special_files() {
    use std::os::unix::fs::{OpenOptionsExt as _, symlink};

    let workspace = tempfile::tempdir().expect("workspace");
    let state = tempfile::tempdir().expect("state");
    let host = tempfile::tempdir().expect("host files");
    let tls = host.path().join("tls.key");
    let profile = host.path().join("profile");
    let bearer = host.path().join("bearer");
    std::fs::create_dir(&profile).expect("profile");
    let protected = [
        state.path().join("gateway.toml"),
        tls,
        profile.join("secret"),
        bearer,
    ];
    for path in &protected {
        std::fs::write(path, b"secret").expect("protected file");
    }
    let sandbox = GatewaySandbox::new(
        workspace.path(),
        state.path(),
        Some(&protected[1]),
        Duration::from_secs(5),
    )
    .expect("gateway sandbox")
    .with_file_roots([profile, protected[3].clone()], &[], &[])
    .expect("protected paths");
    let root = Some(workspace.path());
    let public = host.path().join("public.txt");
    std::fs::write(&public, b"visible").expect("public file");
    symlink(&public, workspace.path().join("visible")).expect("safe link");
    assert_eq!(
        read(&sandbox, root, "visible", 0, 64)
            .await
            .expect("safe link")
            .data,
        b"visible"
    );
    for (index, path) in protected.iter().enumerate() {
        let link = format!("private-{index}");
        symlink(path, workspace.path().join(&link)).expect("private link");
        for requested in [path.to_str().expect("test path"), link.as_str()] {
            assert!(
                read(&sandbox, root, requested, 0, 64).await.is_err(),
                "{requested}"
            );
        }
    }
    let fifo = workspace.path().join("fifo");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("FIFO")
            .success()
    );
    let result =
        tokio::time::timeout(Duration::from_secs(2), read(&sandbox, root, "fifo", 0, 64)).await;
    if result.is_err() {
        // Unblock a regressed reader so the test runtime can still shut down.
        drop(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(nix::libc::O_NONBLOCK)
                .open(&fifo),
        );
    }
    assert!(result.expect("FIFO must not block").is_err());
    assert!(read(&sandbox, root, ".", 0, 64).await.is_err());
}

#[tokio::test]
async fn remove_refuses_traversal_git_directories_and_symlinks() {
    let workspace = tempfile::tempdir().unwrap();
    let (_state, sandbox) = test_sandbox(workspace.path(), Duration::from_secs(5));
    std::fs::create_dir(workspace.path().join("folder")).unwrap();
    std::fs::create_dir(workspace.path().join(".git")).unwrap();
    std::fs::write(workspace.path().join(".git/config"), b"config").unwrap();
    std::fs::write(workspace.path().join("file.txt"), b"data").unwrap();
    for path in ["../outside", ".git/config", "folder"] {
        assert!(remove(&sandbox, path).await.is_err(), "{path}");
    }
    {
        std::os::unix::fs::symlink("file.txt", workspace.path().join("link")).unwrap();
        assert!(
            remove(&sandbox, "link")
                .await
                .unwrap_err()
                .message
                .contains("symbolic link")
        );
        assert!(workspace.path().join("link").symlink_metadata().is_ok());
    }
    assert!(
        remove(&sandbox, "folder")
            .await
            .unwrap_err()
            .message
            .contains("non-regular file")
    );
    remove(&sandbox, "file.txt").await.unwrap();
    assert!(!workspace.path().join("file.txt").exists());
    assert!(workspace.path().join(".git/config").exists());
}

#[tokio::test]
async fn non_git_catalog_lists_files_without_following_symlinks_or_build_directories() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("source.txt"), b"source").expect("source");
    std::fs::create_dir(workspace.path().join("target")).expect("build directory");
    std::fs::write(workspace.path().join("target/artifact"), b"artifact").expect("artifact");
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("private.txt"), b"private").expect("private");
    {
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("linked-directory"))
            .expect("directory symlink");
        std::os::unix::fs::symlink(
            outside.path().join("private.txt"),
            workspace.path().join("linked-file"),
        )
        .expect("file symlink");
    }
    let (_state, sandbox) = test_sandbox(workspace.path(), Duration::from_secs(5));

    let catalog = list(&sandbox, workspace.path(), WorkspaceFileScope::All)
        .await
        .expect("catalog");

    assert!(!catalog.truncated);
    assert_eq!(catalog.files.len(), 1);
    assert_eq!(
        (catalog.files[0].path.as_str(), catalog.files[0].size),
        ("source.txt", 6)
    );
    std::fs::remove_file(workspace.path().join("source.txt")).expect("remove source");
    std::fs::write(workspace.path().join("new.txt"), b"new").expect("new file");
    let refreshed = list(&sandbox, workspace.path(), WorkspaceFileScope::All)
        .await
        .expect("refreshed catalog");
    assert_eq!(refreshed.files[0].path, "new.txt");
    assert!(
        list(&sandbox, workspace.path(), WorkspaceFileScope::Modified)
            .await
            .expect("modified catalog")
            .files
            .is_empty()
    );
}

#[test]
fn truncated_git_catalog_keeps_only_complete_records() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("first.txt"), b"file").expect("file");
    let mut output = "first.txt\0partial".to_owned();

    let truncated = retain_complete_git_paths(&mut output, true).expect("Git paths");
    let catalog =
        git_workspace_files(workspace.path(), &output, truncated).expect("workspace files");

    assert!(catalog.truncated);
    assert_eq!(catalog.files.len(), 1);
    assert_eq!(catalog.files[0].path, "first.txt");
}
