use mobius::backend::checkpoint::{Checkpoint, sqlite::SqliteCheckpoint};
use mobius::backend::model::provider::HostedWebSearch;

use crate::provider_catalog::*;

use super::*;

#[test]
fn configured_provider_status_requires_the_selected_credential_endpoint() {
    let root = tempfile::tempdir().expect("root");
    let (store, config) = ConfigStore::initialize(
        root.path().join("state"),
        "127.0.0.1:8741".parse().expect("listen address"),
        None,
    )
    .expect("config");
    let credentials = CredentialStore::open(store.credentials_path()).expect("credential store");
    credentials
        .set(
            "openrouter",
            "openrouter",
            "openrouter-secret",
            Some("https://other.example/v1"),
        )
        .expect("mismatched credential");
    let selection = ProviderConfig {
        instance: "openrouter".into(),
        provider: "openrouter".into(),
        model: "openai/gpt-5.6-luna".into(),
        base_url: Some("https://openrouter.ai/api/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: None,
        web_search: HostedWebSearch::Off,
    };
    let config = config
        .registering_provider(
            selection,
            "Test".into(),
            Default::default(),
            vec!["openai/gpt-5.6-luna".into()],
            Vec::new(),
        )
        .expect("register provider");

    let instance = provider_instances(&config, &store, &credentials)
        .expect("provider instances")
        .into_iter()
        .find(|entry| entry.selection.provider == "openrouter")
        .expect("OpenRouter instance");

    assert!(!instance.configured);
}

#[test]
fn configured_catalog_resolves_manifest_and_opaque_custom_routes() {
    let root = tempfile::tempdir().expect("root");
    let state = root.path().join("state");
    let (store, config) = ConfigStore::initialize(
        state,
        "127.0.0.1:8741".parse().expect("listen address"),
        None,
    )
    .expect("config");
    let credentials = CredentialStore::open(store.credentials_path()).expect("credential store");
    credentials
        .set(
            "kimi",
            "kimi",
            "kimi-secret",
            Some("https://api.moonshot.ai/v1"),
        )
        .expect("Kimi credential");
    credentials
        .set(
            "responses",
            "responses",
            "custom-secret",
            Some("https://example.com/v1"),
        )
        .expect("custom credential");
    let kimi = ProviderConfig {
        instance: "kimi".into(),
        provider: "kimi".into(),
        model: "kimi-k3".into(),
        base_url: Some("https://api.moonshot.ai/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: Some("max".into()),
        web_search: HostedWebSearch::Off,
    };
    let custom = ProviderConfig {
        instance: "responses".into(),
        provider: "responses".into(),
        model: "vendor/model-opaque".into(),
        base_url: Some("https://example.com/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: Some("provider-defined".into()),
        web_search: HostedWebSearch::Off,
    };
    let alternate_model = "vendor/model-alternate".to_string();
    let config = config
        .registering_provider(
            kimi,
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .and_then(|config| {
            config.registering_provider(
                custom.clone(),
                "Test".into(),
                Default::default(),
                vec![custom.model.clone(), alternate_model.clone()],
                vec!["provider-defined".into(), "minimal".into()],
            )
        })
        .expect("register providers");

    let choices = configured_model_choices(&config, &store, &credentials).expect("catalog");
    let custom_route = choices
        .iter()
        .find(|choice| choice.model == custom.model)
        .expect("custom choice");
    let resolved =
        configured_provider_for_route(&config, &store, &credentials, &custom_route.route)
            .expect("resolve custom route");
    let model_providers =
        configured_model_providers(&config, &store, &credentials).expect("provider IDs");

    assert!(
        choices
            .first()
            .is_some_and(|choice| choice.route.starts_with("kimi::"))
    );
    assert_eq!(resolved, custom);
    assert_eq!(model_providers[&custom_route.route], "responses");
    assert!(choices.iter().any(|choice| choice.model == alternate_model));
    assert!(choices.iter().any(|choice| {
        choice.model == alternate_model && choice.reasoning_effort.as_deref() == Some("minimal")
    }));
    assert_eq!(custom_route.group, format!("Test · {}", custom.model));
}

#[test]
fn same_provider_instances_have_distinct_routes_and_models() {
    let root = tempfile::tempdir().expect("root");
    let (store, config) = ConfigStore::initialize(
        root.path().join("state"),
        "127.0.0.1:8741".parse().expect("listen address"),
        None,
    )
    .expect("config");
    let credentials = CredentialStore::open(store.credentials_path()).expect("credential store");
    let work = ProviderConfig {
        instance: "responses-work".into(),
        provider: "responses".into(),
        model: "work-model".into(),
        base_url: Some("https://work.example/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::Credentialless,
        reasoning_effort: None,
        web_search: HostedWebSearch::Off,
    };
    let personal = ProviderConfig {
        instance: "responses-personal".into(),
        model: "personal-model".into(),
        base_url: Some("https://personal.example/v1".into()),
        ..work.clone()
    };
    let config = config
        .registering_provider(
            work.clone(),
            "Work".into(),
            Default::default(),
            vec![work.model.clone()],
            Vec::new(),
        )
        .and_then(|config| {
            config.registering_provider(
                personal.clone(),
                "Personal".into(),
                Default::default(),
                vec![personal.model.clone()],
                Vec::new(),
            )
        })
        .expect("register providers");

    let choices = configured_model_choices(&config, &store, &credentials).expect("catalog");

    assert_eq!(
        choices
            .into_iter()
            .map(|choice| (choice.route, choice.model))
            .collect::<Vec<_>>(),
        vec![
            (
                "responses-work::work-model::default".into(),
                "work-model".into(),
            ),
            (
                "responses-personal::personal-model::default".into(),
                "personal-model".into(),
            ),
        ]
    );
}

#[test]
fn usage_sink_attributes_a_model_route_to_its_provider() {
    let root = tempfile::tempdir().expect("root");
    let state = root.path().join("state");
    let (store, config) = ConfigStore::initialize(
        state.clone(),
        "127.0.0.1:8741".parse().expect("listen address"),
        None,
    )
    .expect("config");
    let gateway = Mutex::new(config);
    let model_providers = BTreeMap::from([("primary".into(), "openai_socket".into())]);
    let usage = TokenUsage {
        input_tokens: 11,
        total_tokens: 11,
        ..TokenUsage::default()
    };

    persist_usage(&gateway, &store, &model_providers, "primary", &usage).expect("persist usage");

    let (_, restored) = ConfigStore::open(state).expect("reopen config");
    let daily_usage = restored.profile().daily_usage;
    assert_eq!(daily_usage.len(), 1);
    assert_eq!(daily_usage[0].provider, "openai_socket");
    assert_eq!(daily_usage[0].usage, usage);
}

#[test]
fn custom_selection_without_reasoning_uses_the_first_configured_effort() {
    let root = tempfile::tempdir().expect("root");
    let (store, config) = ConfigStore::initialize(
        root.path().join("state"),
        "127.0.0.1:8741".parse().expect("listen address"),
        None,
    )
    .expect("config");
    let credentials = CredentialStore::open(store.credentials_path()).expect("credential store");
    credentials
        .set(
            "responses",
            "responses",
            "custom-secret",
            Some("http://127.0.0.1:11434/v1"),
        )
        .expect("custom credential");
    let selection = ProviderConfig {
        instance: "responses".into(),
        provider: "responses".into(),
        model: "local-model".into(),
        base_url: Some("http://127.0.0.1:11434/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: None,
        web_search: HostedWebSearch::Off,
    };
    let config = config
        .registering_provider(
            selection.clone(),
            "Test".into(),
            Default::default(),
            vec![selection.model.clone()],
            vec!["high".into(), "medium".into()],
        )
        .expect("register provider");

    let choices = configured_model_choices(&config, &store, &credentials).expect("catalog");
    let (router, _) =
        build_models(&config, &selection, &store, &credentials).expect("build selected model");
    let selected = router.choices().next().expect("selected route");

    assert_eq!(choices[0].reasoning_effort.as_deref(), Some("high"));
    assert_eq!(selected.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(router.default_provider(), choices[0].route);
}

#[test]
fn custom_responses_requires_an_endpoint_bound_stored_credential() {
    let root = tempfile::tempdir().expect("root");
    let state = root.path().join("state");
    let (store, _) = ConfigStore::initialize(
        state,
        "127.0.0.1:8741".parse().expect("listen address"),
        None,
    )
    .expect("config");
    let credentials = CredentialStore::open(store.credentials_path()).expect("credential store");
    let selection = ProviderConfig {
        instance: "responses".into(),
        provider: "responses".into(),
        model: "custom-model".into(),
        base_url: Some("https://example.com/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: None,
        web_search: HostedWebSearch::Off,
    };

    let error = resolve_credential(
        "responses",
        provider("responses").expect("provider"),
        selection.base_url.as_deref(),
        &store,
        &credentials,
    )
    .err()
    .expect("custom provider must require stored credentials");

    assert!(
        error
            .to_string()
            .contains("set a credential for `responses`")
    );

    credentials
        .set(
            "responses",
            "responses",
            "official-secret",
            Some("https://api.openai.com/v1"),
        )
        .expect("store endpoint-bound credential");
    assert!(
        resolve_credential(
            "responses",
            provider("responses").expect("provider"),
            selection.base_url.as_deref(),
            &store,
            &credentials,
        )
        .is_err()
    );
}

#[tokio::test]
async fn updating_the_chat_recipe_preserves_capability_metadata() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let skill = workspace.join(".agents/skills/fixture/SKILL.md");
    std::fs::create_dir_all(skill.parent().expect("skill directory")).expect("skill directory");
    std::fs::write(
        &skill,
        "---\nname: fixture\ndescription: Fixture skill.\n---\n",
    )
    .expect("skill");
    let (store, gateway) = ConfigStore::initialize(
        root.path().join("state"),
        "127.0.0.1:8741".parse().expect("listen address"),
        None,
    )
    .expect("config");
    let gateway = gateway
        .registering_provider(
            crate::wire::AgentComposition::default().provider,
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("register provider");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(SqliteCheckpoint::new(store.checkpoints_path()).expect("checkpoints"));
    let mut original_config = crate::wire::AgentComposition::default();
    original_config.middleware.set_enabled("extensions", true);
    let original = ChatSpec::new(
        &workspace,
        crate::wire::VersionedAgentConfig {
            revision: 1,
            config: original_config,
        },
        store.state_dir(),
        None,
    )
    .expect("chat spec");
    let mut checkpoint = Checkpoint::empty("chat");
    checkpoint.metadata = original.metadata().expect("chat metadata");
    checkpoint.metadata.insert(
        "capability.test".into(),
        serde_json::json!({"identity": "preserved"}),
    );
    checkpoints
        .save(&checkpoint, &[], None)
        .await
        .expect("seed checkpoint");
    let (reusable_router, _) = unavailable_models(&gateway, &original.agent.config.provider)
        .expect("unavailable model router");
    let mut composition = original.agent.config.clone();
    composition.middleware.set_enabled("scratchpad", false);
    composition.system_prompt = "updated instructions".into();
    let updated = original
        .replacing_agent(1, composition, &gateway, store.state_dir(), None)
        .expect("updated chat spec");
    let gateway = Arc::new(Mutex::new(gateway));

    let built = assemble(
        gateway,
        &updated,
        &store,
        credentials,
        Arc::clone(&checkpoints),
        ScratchpadStore::new(Arc::clone(&checkpoints)),
        SessionFileStore::new(store.state_dir()),
        Some("chat".into()),
        "test",
        true,
        Some(Arc::clone(&reusable_router)),
    )
    .await
    .expect("assemble chat");
    let skill = std::fs::canonicalize(skill).expect("canonical skill");
    assert_eq!(
        built
            .gateway_sandbox
            .read(skill.to_str().expect("UTF-8 skill path"))
            .await
            .expect("read skill"),
        "---\nname: fixture\ndescription: Fixture skill.\n---\n"
    );
    assert!(Arc::ptr_eq(&reusable_router, &built.model_router));
    let scratchpad = built
        .agent
        .frontend()
        .contributions()
        .iter()
        .find(|contribution| contribution.capability == "scratchpad")
        .expect("disabled scratchpad management surface");
    assert_eq!(scratchpad.commands.len(), 1);
    assert_eq!(scratchpad.commands[0].name, "scratchpad");
    assert_eq!(scratchpad.widgets.len(), 2);
    let (sender, mut events) = built.agent.into_parts();
    drop(sender);
    while events.recv().await.is_some() {}
    let checkpoint = checkpoints
        .load("chat")
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");
    let saved = ChatSpec::from_metadata(&checkpoint.metadata, store.state_dir(), None)
        .expect("saved chat spec");

    assert_eq!(
        checkpoint.metadata["capability.test"],
        serde_json::json!({"identity": "preserved"})
    );
    assert_eq!(saved.agent.revision, 2);
    assert_eq!(saved.agent.config.system_prompt, "updated instructions");
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
            name: inspected.name,
            description: inspected.description,
            version: inspected.version,
            source: ExtensionSource {
                url: "https://example.com/fixture.git".into(),
                reference: None,
                subdirectory: None,
            },
            resolved_revision: "a".repeat(40),
            digest: digest.clone(),
            skills: inspected.skills,
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
            trusted_hook_digest: Some(digest.clone()),
        },
    );
    let active = extension_store
        .resolve(&gateway, &BTreeSet::from(["plugin:fixture".into()]))
        .expect("active extension");
    let inactive = extension_store
        .resolve(&gateway, &BTreeSet::new())
        .expect("inactive extensions");
    let gateway = Arc::new(Mutex::new(gateway));
    let mut settings = crate::middleware_manifest::default_config();
    for feature in &MIDDLEWARE {
        if !feature.manifest.required {
            settings.set_enabled(feature.manifest.id, false);
        }
    }
    settings.set_enabled("extensions", true);
    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(SqliteCheckpoint::new(store.checkpoints_path()).expect("checkpoints"));
    let scratchpad = ScratchpadStore::new(Arc::clone(&checkpoints));
    let session_files = SessionFileStore::new(store.state_dir());
    let backend: Arc<dyn SandboxBackend> =
        Arc::new(LocalSandbox::new(&workspace).expect("sandbox"));
    let discover = |resolved: &ResolvedExtensions| {
        Extensions::discover_installed(
            [
                workspace.join(".agents/skills"),
                workspace.join(".codex/skills"),
            ]
            .into_iter()
            .chain(resolved.skill_roots.iter().cloned()),
        )
        .expect("extensions")
    };
    let active_extensions = discover(&active);
    let inactive_extensions = discover(&inactive);
    let (active, _) = build_middleware(
        &settings,
        &workspace,
        Arc::clone(&gateway),
        scratchpad.clone(),
        session_files.clone(),
        Arc::clone(&backend),
        &active,
        Some(active_extensions),
    )
    .expect("active middleware");
    let (inactive, _) = build_middleware(
        &settings,
        &workspace,
        gateway,
        scratchpad,
        session_files,
        backend,
        &inactive,
        Some(inactive_extensions),
    )
    .expect("inactive middleware");
    let active = active
        .frontend()
        .expect("active frontend")
        .into_iter()
        .find(|contribution| contribution.capability == "extensions")
        .expect("active extensions");
    let inactive = inactive
        .frontend()
        .expect("inactive frontend")
        .into_iter()
        .find(|contribution| contribution.capability == "extensions")
        .expect("inactive extensions");

    assert_eq!(
        active.count,
        inactive.count.map(|count| count + 1),
        "the selected plugin adds exactly one skill"
    );
    assert!(
        active
            .references
            .iter()
            .any(|reference| reference.value == "fixture:review")
    );
    assert!(
        inactive
            .references
            .iter()
            .all(|reference| reference.value != "fixture:review")
    );

    extension_store
        .remove_snapshot(&digest)
        .expect("remove snapshot");
}
