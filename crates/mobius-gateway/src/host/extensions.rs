use super::*;
use crate::extensions::ExtensionStore;

impl GatewayHost {
    pub(crate) async fn install_extension(
        &self,
        source: String,
        reference: Option<String>,
        subdirectory: Option<String>,
    ) -> std::result::Result<ReadyPayload, Rejection> {
        let (mutation, store) = {
            let state = self.state.lock().await;
            (Arc::clone(&state.extension_mutations), state.store.clone())
        };
        let _extension_mutation = mutation.lock_owned().await;
        let staged = ExtensionStore::new(&store)
            .stage(&source, reference.as_deref(), subdirectory.as_deref())
            .await
            .map_err(invalid_config)?;
        let staged_digest = staged.installed.digest.clone();
        let snapshot_created = staged.snapshot_created;
        let id = staged.id.clone();
        let result = async {
            let state = self.state.lock().await;
            let gate = Arc::clone(&state.session_mutations);
            let sessions_guard = gate.write_owned().await;
            let next = {
                let current = state
                    .config
                    .lock()
                    .map_err(|_| internal("gateway configuration lock is poisoned"))?;
                if current.installed_extensions.contains_key(&staged.id) {
                    return Err(invalid_config(Error::Config(format!(
                        "extension `{}` is already installed",
                        staged.id
                    ))));
                }
                let mut next = current.clone();
                next.installed_extensions
                    .insert(staged.id, staged.installed);
                next
            };
            self.commit_extensions(&state, next)?;
            let sessions = state.sessions.values().cloned().collect();
            drop(state);
            self.finish_extension_mutation(sessions, &id, sessions_guard)
                .await
        }
        .await;
        if result.is_err() {
            self.discard_unreferenced_snapshot(&store, &staged_digest, snapshot_created)
                .await;
        }
        result
    }

    pub(crate) async fn update_extension(
        &self,
        id: String,
    ) -> std::result::Result<ReadyPayload, Rejection> {
        let (mutation, store) = {
            let state = self.state.lock().await;
            (Arc::clone(&state.extension_mutations), state.store.clone())
        };
        let _extension_mutation = mutation.lock_owned().await;
        let installed = {
            let state = self.state.lock().await;
            let config = state
                .config
                .lock()
                .map_err(|_| internal("gateway configuration lock is poisoned"))?;
            config
                .installed_extensions
                .get(&id)
                .cloned()
                .ok_or_else(|| unknown_extension(&id))?
        };
        let mut staged = ExtensionStore::new(&store)
            .stage(
                &installed.source.url,
                installed.source.reference.as_deref(),
                installed.source.subdirectory.as_deref(),
            )
            .await
            .map_err(invalid_config)?;
        if staged.id != id || staged.installed.kind != installed.kind {
            if staged.snapshot_created {
                let _ = ExtensionStore::new(&store).remove_snapshot(&staged.installed.digest);
            }
            return Err(invalid_config(Error::Config(
                "an extension update cannot change package identity".into(),
            )));
        }
        if staged.installed.digest == installed.digest {
            staged
                .installed
                .trusted_hook_digest
                .clone_from(&installed.trusted_hook_digest);
        }
        let staged_digest = staged.installed.digest.clone();
        let snapshot_created = staged.snapshot_created;
        let result = async {
            let state = self.state.lock().await;
            let gate = Arc::clone(&state.session_mutations);
            let sessions_guard = gate.write_owned().await;
            let next = {
                let current = state
                    .config
                    .lock()
                    .map_err(|_| internal("gateway configuration lock is poisoned"))?;
                if current.installed_extensions.get(&id) != Some(&installed) {
                    return Err(Rejection {
                        code: "extension_changed",
                        message: format!("extension `{id}` changed while its update was prepared"),
                        fatal: false,
                    });
                }
                let mut next = current.clone();
                next.installed_extensions
                    .insert(id.clone(), staged.installed);
                next
            };
            self.commit_extensions(&state, next)?;
            let sessions = state.sessions.values().cloned().collect();
            drop(state);
            self.finish_extension_mutation(sessions, &id, sessions_guard)
                .await
        }
        .await;
        if result.is_err() {
            self.discard_unreferenced_snapshot(&store, &staged_digest, snapshot_created)
                .await;
        }
        result
    }

    pub(crate) async fn uninstall_extension(
        &self,
        id: String,
    ) -> std::result::Result<ReadyPayload, Rejection> {
        let mutation = {
            let state = self.state.lock().await;
            Arc::clone(&state.extension_mutations)
        };
        let _extension_mutation = mutation.lock_owned().await;
        let state = self.state.lock().await;
        let gate = Arc::clone(&state.session_mutations);
        let sessions_guard = gate.write_owned().await;
        let selected_by = state
            .bots
            .bots()
            .map_err(internal)?
            .into_iter()
            .find(|bot| bot.config.config.extensions.contains(&id))
            .map(|bot| format!("Bot @{}", bot.handle));
        let next = {
            let current = state
                .config
                .lock()
                .map_err(|_| internal("gateway configuration lock is poisoned"))?;
            let selected_by = selected_by.or_else(|| {
                current
                    .bot_defaults
                    .as_ref()
                    .filter(|agent| agent.config.extensions.contains(&id))
                    .map(|_| "the default Bot template".to_owned())
            });
            if let Some(selected_by) = selected_by {
                return Err(Rejection {
                    code: "extension_in_use",
                    message: format!(
                        "extension `{id}` is selected by {selected_by}; remove it from that profile first"
                    ),
                    fatal: false,
                });
            }
            let mut next = current.clone();
            next.installed_extensions
                .remove(&id)
                .ok_or_else(|| unknown_extension(&id))?;
            next
        };
        self.commit_extensions(&state, next)?;
        let sessions = state.sessions.values().cloned().collect();
        drop(state);
        self.finish_extension_mutation(sessions, &id, sessions_guard)
            .await
    }

    pub(crate) async fn set_extension_hooks_trusted(
        &self,
        id: String,
        expected_digest: String,
        trusted: bool,
    ) -> std::result::Result<ReadyPayload, Rejection> {
        let mutation = {
            let state = self.state.lock().await;
            Arc::clone(&state.extension_mutations)
        };
        let _extension_mutation = mutation.lock_owned().await;
        let state = self.state.lock().await;
        let gate = Arc::clone(&state.session_mutations);
        let sessions_guard = gate.write_owned().await;
        let next = {
            let current = state
                .config
                .lock()
                .map_err(|_| internal("gateway configuration lock is poisoned"))?;
            let mut next = current.clone();
            let installed = next
                .installed_extensions
                .get_mut(&id)
                .ok_or_else(|| unknown_extension(&id))?;
            if installed.digest != expected_digest {
                return Err(Rejection {
                    code: "extension_changed",
                    message: format!("extension `{id}` changed before its hook trust changed"),
                    fatal: false,
                });
            }
            if installed.hooks.is_empty() {
                return Err(invalid_config(Error::Config(format!(
                    "extension `{id}` has no executable hooks"
                ))));
            }
            installed.trusted_hook_digest = trusted.then(|| installed.digest.clone());
            next
        };
        self.commit_extensions(&state, next)?;
        let sessions = state.sessions.values().cloned().collect();
        drop(state);
        self.finish_extension_mutation(sessions, &id, sessions_guard)
            .await
    }

    fn commit_extensions(
        &self,
        state: &GatewayState,
        next: GatewayConfig,
    ) -> std::result::Result<(), Rejection> {
        state.store.save(&next).map_err(internal)?;
        *state
            .config
            .lock()
            .map_err(|_| internal("gateway configuration lock is poisoned"))? = next;
        Ok(())
    }

    async fn finish_extension_mutation(
        &self,
        sessions: Vec<HostHandle>,
        id: &str,
        sessions_guard: tokio::sync::OwnedRwLockWriteGuard<()>,
    ) -> std::result::Result<ReadyPayload, Rejection> {
        for host in sessions {
            if let Err(rejection) = host.refresh_extension(id.to_owned()).await
                && rejection.code != "gateway_stopped"
            {
                return Err(rejection);
            }
        }
        drop(sessions_guard);
        let state = self.state.lock().await;
        let payload = gateway_ready(&state).await?;
        drop(state);
        let _ = self.events.send(ServerFrame::new(ServerMessage::Ready {
            payload: payload.clone(),
        }));
        Ok(payload)
    }

    async fn discard_unreferenced_snapshot(
        &self,
        store: &ConfigStore,
        digest: &str,
        created: bool,
    ) {
        if !created {
            return;
        }
        let referenced = {
            let state = self.state.lock().await;
            state.config.lock().map_or(true, |config| {
                config
                    .installed_extensions
                    .values()
                    .any(|extension| extension.digest == digest)
            })
        };
        if !referenced {
            let _ = ExtensionStore::new(store).remove_snapshot(digest);
        }
    }
}

fn unknown_extension(id: &str) -> Rejection {
    Rejection {
        code: "unknown_extension",
        message: format!("extension `{id}` is not installed"),
        fatal: false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use tokio::sync::{Notify, broadcast, mpsc, oneshot};

    use super::*;
    use crate::bots::BotStore;
    use crate::config::{ConfigStore, CredentialStore};
    use crate::extensions::{ExtensionSource, InstalledExtension};
    use crate::wire::{ExtensionHookRecord, ExtensionKind};

    const EXTENSION_ID: &str = "skill:fixture";

    async fn gateway_with_selected_extension() -> (tempfile::TempDir, GatewayHost) {
        let root = tempfile::tempdir().expect("root");
        let listen = "127.0.0.1:8741".parse().expect("listen address");
        let (store, config) =
            ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
        let config = config
            .registering_provider(
                AgentComposition::default().provider,
                "Test".into(),
                Default::default(),
                Vec::new(),
                Vec::new(),
            )
            .expect("provider");
        let mut bot_config = config
            .bot_defaults
            .as_ref()
            .expect("Bot defaults")
            .config
            .clone();
        bot_config.extensions.insert(EXTENSION_ID.into());
        let credentials =
            Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
        let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
        let gateway = GatewayHost::start(store, config, credentials, bots).expect("gateway");
        {
            let state = gateway.state.lock().await;
            state
                .config
                .lock()
                .expect("gateway config")
                .installed_extensions
                .insert(EXTENSION_ID.into(), installed_extension());
            state
                .bots
                .create_bot("fixture", "Fixture", bot_config)
                .expect("Bot");
        }
        (root, gateway)
    }

    fn installed_extension() -> InstalledExtension {
        InstalledExtension {
            kind: ExtensionKind::Skill,
            name: "fixture".into(),
            description: "Fixture".into(),
            version: None,
            source: ExtensionSource {
                url: "https://example.com/fixture.git".into(),
                reference: None,
                subdirectory: None,
            },
            resolved_revision: "a".repeat(40),
            digest: "b".repeat(64),
            skills: vec!["fixture".into()],
            hooks: Vec::new(),
            trusted_hook_digest: None,
        }
    }

    fn fake_extension_race_host(
        bot_id: &str,
    ) -> (HostHandle, mpsc::UnboundedReceiver<()>, Arc<Notify>) {
        let (commands, mut receiver) = mpsc::channel(8);
        let (events, _) = broadcast::channel(8);
        let (refresh_started, refresh_started_receiver) = mpsc::unbounded_channel();
        let release_refresh = Arc::new(Notify::new());
        let actor_release = Arc::clone(&release_refresh);
        tokio::spawn(async move {
            while let Some(command) = receiver.recv().await {
                match command {
                    HostCommand::RefreshExtension { reply, .. } => {
                        let _ = refresh_started.send(());
                        actor_release.notified().await;
                        let _ = reply.send(Ok(()));
                    }
                    HostCommand::ProviderCutoverStatus { reply } => {
                        let _ = reply.send(ProviderCutoverStatus { idle: true });
                    }
                    HostCommand::ReloadBot { reply, .. } => {
                        let _ = reply.send(Ok(()));
                    }
                    HostCommand::CapacityChanged => {}
                    _ => panic!("unexpected host command during extension race"),
                }
            }
        });
        (
            HostHandle {
                inner: Arc::new(HostInner {
                    session_id: Arc::from("extension-race-session"),
                    bot_id: Arc::from(bot_id),
                    commands,
                    events,
                    accepts_file_attachments: Arc::new(AtomicBool::new(false)),
                    alive: Arc::new(AtomicBool::new(true)),
                }),
            },
            refresh_started_receiver,
            release_refresh,
        )
    }

    #[test]
    fn startup_rejects_a_bot_with_a_missing_selected_extension() {
        let root = tempfile::tempdir().expect("root");
        let listen = "127.0.0.1:8741".parse().expect("listen address");
        let (store, config) =
            ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
        let config = config
            .registering_provider(
                AgentComposition::default().provider,
                "Test".into(),
                Default::default(),
                Vec::new(),
                Vec::new(),
            )
            .expect("provider");
        let mut bot_config = config
            .bot_defaults
            .as_ref()
            .expect("Bot defaults")
            .config
            .clone();
        bot_config.extensions.insert(EXTENSION_ID.into());
        let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
        bots.seed_default(config.bot_defaults.as_ref().expect("Bot defaults"))
            .expect("seed Mobius Bot");
        bots.create_bot("fixture", "Fixture", bot_config)
            .expect("Bot");
        let credentials =
            Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));

        let error = match GatewayHost::start(store, config, credentials, bots) {
            Ok(_) => panic!("missing selected extension must fail startup"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("selected extension `skill:fixture` is not installed")
        );
    }

    #[tokio::test]
    async fn uninstall_rejects_an_extension_selected_by_a_bot() {
        let (_root, gateway) = gateway_with_selected_extension().await;
        let rejection = gateway
            .uninstall_extension(EXTENSION_ID.into())
            .await
            .expect_err("selected extension");
        let installed = gateway
            .state
            .lock()
            .await
            .config
            .lock()
            .expect("gateway config")
            .installed_extensions
            .contains_key(EXTENSION_ID);

        assert!(installed);
        assert_eq!(rejection.code, "extension_in_use");
        assert!(rejection.message.contains("Bot @fixture"));
    }

    #[tokio::test]
    async fn extension_refresh_releases_the_session_gate_before_ready_state() {
        let (_root, gateway) = gateway_with_selected_extension().await;
        let bot = gateway
            .state
            .lock()
            .await
            .bots
            .bots()
            .expect("Bots")
            .into_iter()
            .next()
            .expect("fixture Bot");
        let digest = {
            let state = gateway.state.lock().await;
            let mut config = state.config.lock().expect("gateway config");
            let installed = config
                .installed_extensions
                .get_mut(EXTENSION_ID)
                .expect("installed extension");
            installed.hooks.push(ExtensionHookRecord {
                event: "SessionStart".into(),
                matcher: None,
                command: "true".into(),
                timeout_seconds: 5,
            });
            installed.digest.clone()
        };
        let (resident, mut refresh_started, release_refresh) = fake_extension_race_host(&bot.id);
        gateway
            .state
            .lock()
            .await
            .sessions
            .insert(resident.session_id().into(), resident);

        let extension_update = tokio::spawn({
            let gateway = gateway.clone();
            async move {
                gateway
                    .set_extension_hooks_trusted(EXTENSION_ID.into(), digest, true)
                    .await
            }
        });
        refresh_started
            .recv()
            .await
            .expect("extension refresh started");

        let mut next_config = bot.config.config.clone();
        next_config.system_prompt = "Updated during extension refresh".into();
        let (bot_started, bot_waiting) = oneshot::channel();
        let bot_update = tokio::spawn({
            let gateway = gateway.clone();
            let bot = bot.clone();
            async move {
                let _ = bot_started.send(());
                gateway
                    .update_bot(
                        &bot.id,
                        bot.config.revision,
                        "Fixture",
                        &bot.description,
                        bot.tint,
                        next_config,
                    )
                    .await
            }
        });
        bot_waiting.await.expect("Bot update started");
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while gateway.state.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Bot update must own GatewayState while waiting for the session mutation gate");

        release_refresh.notify_one();
        let (extension_result, bot_result) =
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                tokio::join!(extension_update, bot_update)
            })
            .await
            .expect("extension refresh and Bot update must not deadlock");

        extension_result
            .expect("extension task")
            .expect("extension update");
        bot_result.expect("Bot task").expect("Bot update");
    }

    #[test]
    fn update_keeps_hook_trust_only_for_the_same_snapshot() {
        let hook = ExtensionHookRecord {
            event: "SessionStart".into(),
            matcher: None,
            command: "true".into(),
            timeout_seconds: 5,
        };
        let previous_digest = "a".repeat(64);
        let previous = InstalledExtension {
            kind: ExtensionKind::Plugin,
            name: "fixture".into(),
            description: String::new(),
            version: None,
            source: ExtensionSource {
                url: "https://example.com/fixture.git".into(),
                reference: None,
                subdirectory: None,
            },
            resolved_revision: "c".repeat(40),
            digest: previous_digest.clone(),
            skills: Vec::new(),
            hooks: vec![hook.clone()],
            trusted_hook_digest: Some(previous_digest),
        };
        let mut next = InstalledExtension {
            digest: "b".repeat(64),
            trusted_hook_digest: None,
            hooks: vec![hook],
            ..previous.clone()
        };

        if next.digest == previous.digest {
            next.trusted_hook_digest
                .clone_from(&previous.trusted_hook_digest);
        }
        assert_eq!(next.trusted_hook_digest, None);

        next.digest.clone_from(&previous.digest);
        if next.digest == previous.digest {
            next.trusted_hook_digest
                .clone_from(&previous.trusted_hook_digest);
        }
        assert_eq!(next.trusted_hook_digest, previous.trusted_hook_digest);
    }
}
