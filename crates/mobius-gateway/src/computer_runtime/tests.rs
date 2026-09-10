use super::*;

#[tokio::test]
async fn disabled_capability_does_not_install_anything() {
    let root = tempfile::tempdir().expect("root");
    let mut settings = crate::wire::AgentComposition::default().middleware;
    settings.set_enabled("computer_control", false);
    assert!(
        prepare(&root.path().join("state"), &settings)
            .await
            .expect("disabled")
            .is_none()
    );
    assert_eq!(fs::read_dir(root.path()).expect("directory").count(), 0);
}

#[tokio::test]
async fn incomplete_cached_runtime_is_not_accepted() {
    let root = tempfile::tempdir().expect("root");
    let error = install(root.path()).await.expect_err("missing executable");
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
            .read(runtime.join("computer-control.md").to_str().expect("path"))
            .await
            .expect("public runtime"),
        DOCUMENTATION
    );
    assert!(
        sandbox
            .read(state.join("credentials.json").to_str().expect("path"))
            .await
            .is_err()
    );
}

#[tokio::test]
#[ignore = "downloads the pinned Node/Playwright runtime and launches Chromium"]
async fn downloaded_runtime_works() {
    let root = tempfile::tempdir().expect("root");
    let runtime = managed_directory(&root.path().join("state"));
    install(&runtime).await.expect("download and install");
    let installed = fs::metadata(runtime.join("node"))
        .expect("node")
        .modified()
        .expect("modified");
    install(&runtime).await.expect("reuse installed runtime");
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
        .read_bytes("ready.png", 50 * 1024 * 1024)
        .await
        .expect("sandbox image access");
    assert!(bytes.starts_with(b"\x89PNG"));
}
