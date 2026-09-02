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
        description: String::new(),
        version: None,
        source: ExtensionSource {
            url: "https://example.com/hooked.git".into(),
            reference: None,
            subdirectory: None,
        },
        resolved_revision: "a".repeat(40),
        digest,
        skills: Vec::new(),
        hooks,
        trusted_hook_digest: None,
    }
}

#[tokio::test]
async fn extension_git_ignores_home_git_config() {
    let home = tempfile::tempdir().expect("home");
    fs::write(
        home.path().join(".gitconfig"),
        "[mobius]\n\textensionMarker = inherited\n",
    )
    .expect("global Git config");
    let output = git_command()
        .env("HOME", home.path())
        .args(["config", "--get", "mobius.extensionMarker"])
        .output()
        .await
        .expect("read extension Git config");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn hook_authorization_tracks_the_authoritative_catalog() {
    let digest = "a".repeat(64);
    let mut installed = installed_plugin(
        digest.clone(),
        vec![ExtensionHookRecord {
            event: "SessionStart".into(),
            matcher: None,
            command: "true".into(),
            timeout_seconds: 5,
        }],
    );
    installed.trusted_hook_digest = Some(digest.clone());
    let mut config = GatewayConfig::new(crate::config::DEFAULT_LISTEN, None).expect("config");
    config
        .installed_extensions
        .insert("plugin:hooked".into(), installed);
    let gateway = Arc::new(Mutex::new(config));
    let (_, authorization) = ResolvedPlugin {
        id: "plugin:hooked".into(),
        digest,
        root: PathBuf::from("package"),
        hooks_trusted: true,
    }
    .activation(Arc::clone(&gateway));
    let authorization = authorization.expect("trusted hook authorization");

    let mut launched = false;
    authorization(&mut || {
        launched = true;
        Ok(())
    })
    .expect("authorized launch");
    assert!(launched);
    gateway
        .lock()
        .expect("config")
        .installed_extensions
        .remove("plugin:hooked");
    launched = false;
    authorization(&mut || {
        launched = true;
        Ok(())
    })
    .expect("revoked launch");
    assert!(!launched);
}

#[test]
fn github_tree_url_selects_ref_and_subdirectory() {
    let source = ExtensionSource::parse(
        "https://github.com/example/repo/tree/main/path/to/skill",
        None,
        None,
    )
    .expect("source");

    assert_eq!(source.url, "https://github.com/example/repo");
    assert_eq!(source.reference.as_deref(), Some("main"));
    assert_eq!(source.subdirectory.as_deref(), Some("path/to/skill"));
}

#[test]
fn persisted_source_rejects_ref_whitespace() {
    let source = ExtensionSource {
        url: "https://example.com/repo.git".into(),
        reference: Some(" main ".into()),
        subdirectory: None,
    };

    assert!(source.validate().is_err());
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
fn resolve_rejects_missing_extensions_and_keeps_untrusted_plugin_skills() {
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
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"true","timeout":5}]}]}}"#,
        )
        .expect("hooks");
    let digest = commit_snapshot(&store, &package);
    let mut config = GatewayConfig::new(crate::config::DEFAULT_LISTEN, None).expect("config");
    config.installed_extensions.insert(
        "plugin:hooked".into(),
        installed_plugin(
            digest,
            vec![ExtensionHookRecord {
                event: "SessionStart".into(),
                matcher: None,
                command: "true".into(),
                timeout_seconds: 5,
            }],
        ),
    );

    let error = match store.resolve(
        &config,
        &BTreeSet::from(["plugin:hooked".into(), "skill:missing".into()]),
    ) {
        Ok(_) => panic!("missing extension must fail"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("is not installed"));

    let resolved = store
        .resolve(&config, &BTreeSet::from(["plugin:hooked".into()]))
        .expect("installed extension");

    assert!(resolved.skill_roots.is_empty());
    assert_eq!(resolved.plugins.len(), 1);
    assert!(!resolved.plugins[0].hooks_trusted);
}

#[test]
fn startup_validation_rejects_catalog_metadata_that_hides_snapshot_hooks() {
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
    config
        .installed_extensions
        .insert("plugin:hooked".into(), installed_plugin(digest, Vec::new()));

    let error = store
        .verify_installed_snapshots(&config)
        .expect_err("hidden hooks must fail closed");

    assert!(error.to_string().contains("metadata does not match"));
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
    use std::os::unix::fs::PermissionsExt as _;

    let temporary = tempfile::tempdir().expect("temporary packages");
    let first = temporary.path().join("first");
    let second = temporary.path().join("second");
    fs::create_dir(&first).expect("first package");
    fs::create_dir(&second).expect("second package");
    let first_file = first.join("a");
    let second_file = second.join("a\u{1}");
    fs::write(&first_file, b"\0content").expect("first file");
    fs::write(&second_file, b"content").expect("second file");
    fs::set_permissions(&first_file, fs::Permissions::from_mode(0o700)).expect("first mode");
    fs::set_permissions(&second_file, fs::Permissions::from_mode(0o600)).expect("second mode");

    assert_ne!(
        tree_digest(&first).expect("first digest"),
        tree_digest(&second).expect("second digest")
    );
}
