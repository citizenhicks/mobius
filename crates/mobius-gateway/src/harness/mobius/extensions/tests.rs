use super::*;

fn commit_snapshot(store: &ExtensionStore, package: &Path) -> String {
    let digest = tree_digest(package).expect("package digest");
    let snapshot = store.snapshot_root(&digest);
    fs::create_dir_all(snapshot.parent().expect("snapshot parent")).expect("snapshot directory");
    fs::rename(package, &snapshot).expect("commit snapshot");
    digest
}

fn installed_plugin(digest: String, hooks: Vec<ExtensionHookRecord>) -> InstalledExtension {
    InstalledExtension {
        kind: ExtensionKind::Plugin,
        name: "hooked".into(),
        description: String::new().into(),
        version: None,
        source: ExtensionSource {
            url: "https://example.com/hooked.git".into(),
            reference: None,
            subdirectory: None,
        },
        resolved_revision: "a".repeat(40).into(),
        digest: digest.into(),
        skills: Vec::new().into(),
        hooks: hooks.into(),
        trusted_hook_digest: None,
    }
}

#[test]
fn prune_removes_only_unreferenced_snapshots() {
    let temporary = tempfile::tempdir().expect("temporary extensions");
    let store = ExtensionStore {
        root: temporary.path().join("store"),
    };
    let retained = "a".repeat(64);
    let orphan = "b".repeat(64);
    fs::create_dir_all(store.snapshot_root(&retained)).expect("retained snapshot");
    fs::create_dir_all(store.snapshot_root(&orphan)).expect("orphan snapshot");
    let mut config = GatewayConfig::new(crate::config::DEFAULT_LISTEN, None).expect("config");
    config.installed_extensions.insert(
        "plugin:hooked".into(),
        installed_plugin(retained.clone(), Vec::new()),
    );

    store.prune(&config).expect("prune snapshots");

    assert!(store.snapshot_root(&retained).exists());
    assert!(!store.snapshot_directory(&orphan).exists());
}

#[test]
fn startup_validation_rejects_tampered_snapshot() {
    let temporary = tempfile::tempdir().expect("temporary extensions");
    let store = ExtensionStore {
        root: temporary.path().join("store"),
    };
    let package = temporary.path().join("package");
    fs::create_dir_all(package.join(".codex-plugin")).expect("manifest directory");
    fs::create_dir_all(package.join("hooks")).expect("hooks directory");
    fs::write(
        package.join(".codex-plugin/plugin.json"),
        r#"{"name":"hooked"}"#,
    )
    .expect("manifest");
    fs::write(
        package.join("hooks/hooks.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"true"}]}]}}"#,
    )
    .expect("hooks");
    let digest = commit_snapshot(&store, &package);
    let mut config = GatewayConfig::new(crate::config::DEFAULT_LISTEN, None).expect("config");
    config.installed_extensions.insert(
        "plugin:hooked".into(),
        installed_plugin(
            digest.clone(),
            vec![ExtensionHookRecord {
                event: "SessionStart".into(),
                matcher: None,
                command: "true".into(),
                timeout_seconds: 10,
            }],
        ),
    );
    fs::write(
        store.snapshot_root(&digest).join("hooks/hooks.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"false"}]}]}}"#,
    )
    .expect("tamper snapshot");

    let error = store
        .verify_installed_snapshots(&config)
        .expect_err("tampered snapshot must fail at startup");

    assert!(error.to_string().contains("digest changed"));
}

#[cfg(unix)]
#[test]
fn tree_digest_has_unambiguous_path_and_content_framing() {
    let temporary = tempfile::tempdir().expect("temporary packages");
    let first = temporary.path().join("first");
    let second = temporary.path().join("second");
    fs::create_dir(&first).expect("first package");
    fs::create_dir(&second).expect("second package");
    let first_file = first.join("a");
    let second_file = second.join("a\u{1}");
    fs::write(&first_file, b"\0content").expect("first file");
    fs::write(&second_file, b"content").expect("second file");
    fs::set_permissions(&first_file, mobius::owner_only::dir()).expect("first mode");
    fs::set_permissions(&second_file, mobius::owner_only::file()).expect("second mode");

    assert_ne!(
        tree_digest(&first).expect("first digest"),
        tree_digest(&second).expect("second digest")
    );
}
