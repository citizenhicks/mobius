use super::*;

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

#[test]
fn installer_forwards_proxy_and_ca_without_forwarding_provider_credentials() {
    let mut command = Command::new("node");
    command.env_clear();
    forward_installer_environment(&mut command, |name| match name {
        "HTTPS_PROXY" => Some("http://proxy.internal:3128".into()),
        "ALL_PROXY" | "all_proxy" => Some("socks5://proxy.internal:1080".into()),
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

#[tokio::test]
async fn incomplete_cached_runtime_is_not_accepted() {
    let root = tempfile::tempdir().expect("root");
    let error = install(root.path(), &ComputerConfig::default())
        .await
        .expect_err("missing executable");
    assert!(error.to_string().contains("missing node"));
}
