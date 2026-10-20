use mobius::backend::model::provider::BaseUrl;
use mobius::backend::model::provider::HostedWebSearch;

use super::*;

#[tokio::test]
async fn canceled_discovery_waiter_keeps_the_gate_until_the_batch_finishes() {
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    let (first_started, first_started_receiver) = tokio::sync::oneshot::channel();
    let (release_first, release_receiver) = std::sync::mpsc::channel();
    let first = tokio::spawn(run_discovery(Arc::clone(&gate), move || {
        first_started.send(()).expect("first started receiver");
        release_receiver.recv().expect("release first discovery");
        Ok(())
    }));
    first_started_receiver
        .await
        .expect("first discovery started");
    first.abort();

    let (second_started, mut second_started_receiver) = tokio::sync::oneshot::channel();
    let second = tokio::spawn(run_discovery(Arc::clone(&gate), move || {
        second_started.send(()).expect("second started receiver");
        Ok(())
    }));
    tokio::select! {
        _ = &mut second_started_receiver => panic!("second discovery bypassed the gate"),
        () = tokio::time::sleep(Duration::from_millis(50)) => {}
    }

    release_first.send(()).expect("release first discovery");
    second
        .await
        .expect("second discovery task")
        .expect("second discovery");
}

#[test]
fn failed_usage_save_keeps_committed_counters() {
    let root = tempfile::tempdir().expect("root");
    let state = root.path().join("state");
    let (store, config) = ConfigStore::initialize(
        state.clone(),
        "127.0.0.1:8741".parse().expect("listen address"),
        None,
    )
    .expect("config");
    let gateway = Mutex::new(config);
    let usage = TokenUsage {
        input_tokens: 11,
        total_tokens: 11,
        ..TokenUsage::default()
    };
    persist_usage(&gateway, &store, "openai_socket", &usage).expect("initial usage");
    let before = gateway.lock().unwrap().profile().daily_usage;
    let path = state.join("gateway.toml");
    std::fs::remove_file(&path).expect("remove test config");
    std::fs::create_dir(&path).expect("block config replacement");

    assert!(persist_usage(&gateway, &store, "openai_socket", &usage).is_err());
    assert_eq!(gateway.lock().unwrap().profile().daily_usage, before);
}

#[tokio::test]
async fn usage_publication_waits_off_executor_and_keeps_failed_totals() {
    let root = tempfile::tempdir().expect("root");
    let state = root.path().join("state");
    let (store, config) = ConfigStore::initialize(
        state.clone(),
        "127.0.0.1:8741".parse().expect("listen address"),
        None,
    )
    .expect("config");
    let gateway = Arc::new(Mutex::new(config));
    let usage = TokenUsage {
        input_tokens: 11,
        total_tokens: 11,
        ..TokenUsage::default()
    };
    let (locked, acquired) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let blocking_gateway = Arc::clone(&gateway);
    let blocker = tokio::task::spawn_blocking(move || {
        let _guard = blocking_gateway.lock().expect("configuration lock");
        locked.send(()).expect("signal lock");
        released.recv().expect("release lock");
    });
    acquired.await.expect("lock acquired");
    let publish = publish_usage(&gateway, &store, "openai_socket", &usage);
    tokio::pin!(publish);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut publish)
            .await
            .is_err()
    );
    release.send(()).expect("release worker");
    blocker.await.expect("blocking owner");
    publish.await.expect("durable publication");
    let (_, restored) = ConfigStore::open(state.clone()).expect("reopen config");
    assert_eq!(restored.profile().daily_usage[0].usage, usage);
    let before = gateway.lock().expect("gateway").profile().daily_usage;
    let path = state.join("gateway.toml");
    std::fs::remove_file(&path).expect("remove config");
    std::fs::create_dir(&path).expect("block publication");
    assert!(
        publish_usage(&gateway, &store, "openai_socket", &usage)
            .await
            .is_err()
    );
    assert_eq!(
        gateway.lock().expect("gateway").profile().daily_usage,
        before
    );
}

#[test]
fn custom_openai_roots_require_endpoint_bound_stored_credentials() {
    for (provider_id, model) in [
        ("responses", "custom-model"),
        ("openai_socket", "gpt-6.1-sol"),
    ] {
        let root = tempfile::tempdir().expect("root");
        let state = root.path().join("state");
        let (store, _) = ConfigStore::initialize(
            state,
            "127.0.0.1:8741".parse().expect("listen address"),
            None,
        )
        .expect("config");
        let credentials =
            CredentialStore::open(store.credentials_path()).expect("credential store");
        let selection = ProviderConfig {
            tool_discovery: None,
            instance: provider_id.into(),
            provider: provider_id.into(),
            model: model.into(),
            base_url: Some(BaseUrl::new("https://example.com/v1").unwrap()),
            endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
            reasoning_effort: None,
            service_tier: None,
            web_search: HostedWebSearch::Off,
        };

        let error = resolve_credential(
            provider_id,
            provider(provider_id).expect("provider"),
            selection.base_url.as_ref(),
            &store,
            &credentials,
            &mobius::backend::model::ModelTransportSettings::default(),
        )
        .expect_err("custom provider must require stored credentials");

        assert!(
            error
                .to_string()
                .contains(&format!("set a credential for `{provider_id}`"))
        );

        credentials
            .set(
                provider_id,
                provider_id,
                "official-secret",
                Some(&BaseUrl::new("https://api.openai.com/v1").unwrap()),
                None,
            )
            .expect("store endpoint-bound credential");
        assert!(
            resolve_credential(
                provider_id,
                provider(provider_id).expect("provider"),
                selection.base_url.as_ref(),
                &store,
                &credentials,
                &mobius::backend::model::ModelTransportSettings::default(),
            )
            .is_err()
        );
        credentials
            .set(
                provider_id,
                provider_id,
                "endpoint-token",
                selection.base_url.as_ref(),
                None,
            )
            .expect("endpoint credential");
        let (credential, _) = resolve_credential(
            provider_id,
            provider(provider_id).expect("provider"),
            selection.base_url.as_ref(),
            &store,
            &credentials,
            &mobius::backend::model::ModelTransportSettings::default(),
        )
        .expect("resolve endpoint credential");
        assert!(matches!(credential, ProviderCredential::ApiKey(key) if key == "endpoint-token"));
    }
}

#[test]
fn selected_trusted_plugin_snapshot_reaches_extensions_assembly_only_when_active() {
    use std::collections::BTreeSet;

    use mobius::backend::sandbox::local::LocalSandbox;

    use crate::extensions::{ExtensionSource, InstalledExtension};
    use crate::wire::{ExtensionHookRecord, ExtensionKind};

    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let (store, mut gateway) = ConfigStore::initialize(
        root.path().join("state"),
        "127.0.0.1:8741".parse().expect("listen address"),
        None,
    )
    .expect("config");
    let package = root.path().join("package");
    std::fs::create_dir_all(package.join(".codex-plugin")).expect("manifest directory");
    std::fs::create_dir_all(package.join("skills/review")).expect("skill directory");
    std::fs::write(
        package.join(".codex-plugin/plugin.json"),
        r#"{"name":"fixture","description":"Fixture plugin","skills":"./skills","hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"true"}]}]}}"#,
    )
    .expect("plugin manifest");
    std::fs::write(
        package.join("skills/review/SKILL.md"),
        "---\nname: review\ndescription: Review fixture code.\n---\n",
    )
    .expect("skill manifest");
    let inspected = mobius::middleware::extensions::inspect_package(&package).expect("package");
    assert!(!inspected.hooks.is_empty());
    let extension_store = ExtensionStore::new(&store);
    let digest = extension_store
        .commit_test_snapshot(&package)
        .expect("snapshot");
    gateway.installed_extensions.insert(
        "plugin:fixture".into(),
        InstalledExtension {
            kind: ExtensionKind::Plugin,
            name: inspected.name.into(),
            description: inspected.description.into(),
            version: inspected.version.map(Into::into),
            source: ExtensionSource {
                url: "https://example.com/fixture.git".into(),
                reference: None,
                subdirectory: None,
            },
            resolved_revision: "a".repeat(40).into(),
            digest: digest.clone().into(),
            skills: inspected.skills.into(),
            hooks: inspected
                .hooks
                .into_iter()
                .map(|hook| ExtensionHookRecord {
                    event: hook.event,
                    matcher: hook.matcher,
                    command: hook.command,
                    timeout_seconds: hook.timeout_seconds,
                })
                .collect(),
            trusted_hook_digest: Some(digest.clone().into()),
        },
    );
    let active = extension_store
        .resolve(&gateway, &BTreeSet::from(["plugin:fixture".into()]))
        .expect("active extension");
    let inactive = extension_store
        .resolve(&gateway, &BTreeSet::new())
        .expect("inactive extensions");
    let gateway = Arc::new(Mutex::new(gateway));
    let backend: Arc<dyn SandboxBackend> =
        Arc::new(LocalSandbox::new(&workspace).expect("sandbox"));
    let build = |resolved: &ResolvedExtensions| {
        Extensions::installed(
            &workspace,
            resolved.skill_roots.iter().cloned(),
            resolved
                .plugins
                .iter()
                .map(|plugin| plugin.activation(&gateway)),
        )
        .and_then(|extensions| extensions.start_hooks(Arc::clone(&backend)))
        .expect("middleware")
    };
    let active = build(&active);
    let inactive = build(&inactive);
    let active = active.frontend("session");
    let inactive = inactive.frontend("session");

    assert_eq!(
        active.count,
        inactive.count.map(|count| count + 1),
        "the selected plugin adds exactly one skill"
    );
    assert!(
        active
            .references
            .iter()
            .any(|reference| &*reference.value == "fixture:review")
    );
    assert!(
        inactive
            .references
            .iter()
            .all(|reference| &*reference.value != "fixture:review")
    );

    extension_store
        .remove_snapshot(&digest)
        .expect("remove snapshot");
}
