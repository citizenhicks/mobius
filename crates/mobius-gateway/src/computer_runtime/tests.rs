use super::*;

#[test]
fn automatic_managed_layout_changes_when_system_dependencies_are_selected() {
    let state = Path::new("/tmp/gateway-state");
    let default = managed_directory(state);
    let config = ComputerConfig {
        node_executable: Some("node".into()),
        ..ComputerConfig::default()
    };
    assert_ne!(default, managed_directory_for(state, &config));
}

#[tokio::test]
async fn invalid_or_oversized_ca_bundle_is_rejected_before_any_download() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("ca.pem");
    fs::write(&path, "not a certificate").unwrap();
    assert!(ca_certificates(&path).await.is_err());
    fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap();
    assert!(ca_certificates(&path).await.is_err());
}

#[tokio::test]
async fn preinstalled_resources_accept_system_node_and_chromium_without_a_bundle() {
    let root = tempfile::tempdir().expect("resources");
    let resources = root.path().join("resources");
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    fs::create_dir(&state).unwrap();
    fs::create_dir(&workspace).unwrap();
    export_resources(&resources).expect("export");
    let playwright = root.path().join("playwright");
    fs::create_dir(&playwright).expect("module");
    fs::write(playwright.join("package.json"), "{}").expect("module manifest");
    let mut config = ComputerConfig {
        mode: RuntimeMode::Preinstalled,
        directory: Some(resources.clone()),
        node_executable: Some("/bin/sh".into()),
        playwright_module: Some(playwright),
        ..ComputerConfig::default()
    };
    config.browser.executable = Some("/bin/sh".into());
    config.browser.viewport = [800, 600];
    config.browser.sandbox = false;
    let runtime = prepare_desktop(&state, &config)
        .await
        .expect("preinstalled");
    assert!(!resources.join("node").exists());
    assert!(!resources.join("browsers").exists());
    let command = worker_command(&runtime, &config).expect("worker command");
    assert_eq!(command.executable, fs::canonicalize("/bin/sh").unwrap());
    let options: serde_json::Value = serde_json::from_str(&command.arguments[1]).unwrap();
    assert_eq!(options["viewport"], serde_json::json!([800, 600]));
    assert_eq!(options["sandbox"], false);
    assert_eq!(options["root_sandbox_error"], ROOT_SANDBOX_ERROR);
    assert!(
        options["arguments"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("--no-sandbox"))
    );
    assert!(
        resource_roots(&runtime, &config, &state, &workspace, &[])
            .unwrap()
            .iter()
            .all(|path| path.is_dir())
    );
}

#[test]
fn computer_resources_reject_canonical_private_or_writable_aliases_and_ancestors() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    let attached = root.path().join("attached");
    for directory in [&state, &workspace, &attached] {
        fs::create_dir(directory).unwrap();
    }
    let runtime = managed_directory(&state);
    fs::create_dir_all(&runtime).unwrap();
    let mut config = ComputerConfig {
        node_executable: Some("/bin/sh".into()),
        ..ComputerConfig::default()
    };
    config.browser.executable = Some("/bin/sh".into());
    let attached_roots = [attached.clone()];
    let roots = resource_roots(&runtime, &config, &state, &workspace, &attached_roots).unwrap();
    assert!(roots.contains(&fs::canonicalize(&runtime).unwrap()));
    assert!(!roots.contains(&state.join("desktop/profile")));
    for forbidden in [&state, &workspace, &attached] {
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(forbidden, &alias).unwrap();
        let error =
            resource_roots(&alias, &config, &state, &workspace, &attached_roots).unwrap_err();
        assert!(error.to_string().contains("outside gateway state"));
        config.playwright_module = Some(alias.clone());
        assert!(resource_roots(&runtime, &config, &state, &workspace, &attached_roots).is_err());
        config.playwright_module = None;
        fs::remove_file(alias).unwrap();
    }
    assert!(resource_roots(root.path(), &config, &state, &workspace, &attached_roots).is_err());
}

#[tokio::test]
async fn preinstalled_mode_never_attempts_to_install_missing_resources() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("missing-runtime");
    let config = ComputerConfig {
        mode: RuntimeMode::Preinstalled,
        directory: Some(directory.clone()),
        ..ComputerConfig::default()
    };
    assert!(
        prepare_desktop(&root.path().join("state"), &config)
            .await
            .is_err()
    );
    assert!(!directory.exists());
}

#[tokio::test]
async fn installer_deadline_includes_waiting_for_another_operator_install() {
    let root = tempfile::tempdir().unwrap();
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.path().join("install.lock"))
        .unwrap();
    lock.lock().unwrap();
    let config = ComputerConfig {
        install_timeout_seconds: 1,
        ..ComputerConfig::default()
    };
    let error = install(&root.path().join("resources"), &config)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("installation timed out"));
    assert!(!root.path().join("resources").exists());
}

#[test]
fn installer_forwards_proxy_and_ca_without_forwarding_provider_credentials() {
    let mut command = Command::new("node");
    command.env_clear();
    forward_installer_environment(&mut command, |name| match name {
        "HTTPS_PROXY" => Some("http://proxy.internal:3128".into()),
        "ALL_PROXY" => Some("socks5://proxy.internal:1080".into()),
        "all_proxy" => Some("socks5://proxy.internal:1080".into()),
        "SSL_CERT_FILE" => Some("/etc/company/ca.pem".into()),
        "NODE_USE_SYSTEM_CA" => Some("1".into()),
        "OPENAI_API_KEY" => Some("should-not-be-forwarded".into()),
        _ => None,
    });
    let environment = command
        .as_std()
        .get_envs()
        .map(|(name, value)| {
            (
                name.to_str().unwrap(),
                value.map(|value| value.to_str().unwrap()),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(
        environment.get("HTTPS_PROXY"),
        Some(&Some("http://proxy.internal:3128"))
    );
    assert_eq!(
        environment.get("NODE_EXTRA_CA_CERTS"),
        Some(&Some("/etc/company/ca.pem"))
    );
    for name in ["ALL_PROXY", "all_proxy"] {
        assert_eq!(
            environment.get(name),
            Some(&Some("socks5://proxy.internal:1080"))
        );
    }
    assert_eq!(environment.get("NODE_USE_SYSTEM_CA"), Some(&Some("1")));
    assert!(!environment.contains_key("OPENAI_API_KEY"));
}

#[test]
fn installer_search_path_contains_only_selected_node_and_standard_system_tools() {
    let config = ComputerConfig {
        node_executable: Some("/bin/sh".into()),
        ..ComputerConfig::default()
    };
    let node = node_executable(Path::new("/unused"), &config).unwrap();
    let expected = [
        node.parent().unwrap(),
        Path::new("/usr/bin"),
        Path::new("/bin"),
    ];
    let path = installer_path(Path::new("/unused"), &config).unwrap();
    assert_eq!(std::env::split_paths(&path).collect::<Vec<_>>(), expected);
}

#[tokio::test]
async fn disabled_capability_does_not_install_anything() {
    let root = tempfile::tempdir().expect("root");
    let mut settings = crate::wire::AgentComposition::default().middleware;
    settings.set_enabled("computer_control", false);
    assert!(
        prepare(
            &root.path().join("state"),
            &settings,
            &ComputerConfig::default()
        )
        .await
        .expect("disabled")
        .is_none()
    );
    assert_eq!(fs::read_dir(root.path()).expect("directory").count(), 0);
}

#[tokio::test]
async fn incomplete_cached_runtime_is_not_accepted() {
    let root = tempfile::tempdir().expect("root");
    let error = install(root.path(), &ComputerConfig::default())
        .await
        .expect_err("missing executable");
    assert!(error.to_string().contains("missing node"));
}

#[tokio::test]
async fn runtime_resources_do_not_expose_private_gateway_state() {
    use mobius::backend::sandbox::SandboxBackend;
    let root = tempfile::tempdir().expect("root");
    let root_path = fs::canonicalize(root.path()).expect("canonical root");
    let workspace = root_path.join("workspace");
    let state = root_path.join("state");
    let runtime = managed_directory(&state);
    for directory in [&workspace, &state, &runtime] {
        fs::create_dir_all(directory).expect("directory");
    }
    fs::write(state.join("credentials.json"), "private").expect("credentials");
    fs::write(runtime.join("computer-control.md"), DOCUMENTATION).expect("documentation");
    let sandbox =
        crate::sandbox::GatewaySandbox::new(&workspace, &state, None, Duration::from_secs(5))
            .expect("sandbox")
            .allow_read_roots([runtime.clone()])
            .expect("runtime read root");
    assert_eq!(
        sandbox
            .read(
                runtime.join("computer-control.md").to_str().expect("path"),
                mobius::backend::sandbox::SandboxMode::WorkspaceWrite,
            )
            .await
            .expect("public runtime"),
        DOCUMENTATION
    );
    assert!(
        sandbox
            .read(
                state.join("credentials.json").to_str().expect("path"),
                mobius::backend::sandbox::SandboxMode::WorkspaceWrite,
            )
            .await
            .is_err()
    );
}

#[tokio::test]
#[ignore = "downloads the pinned Node/Playwright runtime and launches Chromium"]
async fn downloaded_runtime_works() {
    let root = tempfile::tempdir().expect("root");
    let runtime = managed_directory(&root.path().join("state"));
    install(&runtime, &ComputerConfig::default())
        .await
        .expect("download and install");
    let installed = fs::metadata(runtime.join("node"))
        .expect("node")
        .modified()
        .expect("modified");
    install(&runtime, &ComputerConfig::default())
        .await
        .expect("reuse installed runtime");
    assert_eq!(
        fs::metadata(runtime.join("node"))
            .expect("node")
            .modified()
            .expect("modified"),
        installed
    );
    let output = Command::new(runtime.join("node"))
        .args([
            "--test",
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/computer_runtime/worker.test.ts"
            ),
        ])
        .env("MOBIUS_COMPUTER_RUNTIME", &runtime)
        .output()
        .await
        .expect("worker tests");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    use mobius::backend::sandbox::{
        CommandMode, CommandOutputSink, NetworkAccess, SandboxBackend, SandboxMode,
    };
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&state).expect("state");
    let runtime = fs::canonicalize(runtime).expect("runtime path");
    let sandbox =
        crate::sandbox::GatewaySandbox::new(&workspace, &state, None, Duration::from_secs(30))
            .expect("gateway sandbox")
            .allow_read_roots([runtime.clone()])
            .expect("runtime resources");
    fs::write(workspace.join("browser.cjs"), format!(
        "process.env.PLAYWRIGHT_BROWSERS_PATH={}; (async()=>{{ const browser=await require({}).chromium.launch({{headless:true}}); const page=await browser.newPage(); await page.setContent('<h1>Ready</h1>'); await page.screenshot({{path:'ready.png'}}); await browser.close(); }})().catch(e=>{{console.error(e);process.exitCode=1}});",
        serde_json::to_string(&runtime.join("browsers")).expect("browser path"),
        serde_json::to_string(&runtime.join("node_modules/playwright")).expect("module path"),
    )).expect("browser script");
    let output = sandbox
        .execute(
            &format!("'{}' browser.cjs", runtime.join("node").display()),
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
            CommandMode::Foreground,
            CommandOutputSink::default(),
        )
        .await
        .expect("sandboxed browser");
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    let bytes = sandbox
        .read_bytes("ready.png", 50 * 1024 * 1024, SandboxMode::WorkspaceWrite)
        .await
        .expect("sandbox image access");
    assert!(bytes.starts_with(b"\x89PNG"));
}
