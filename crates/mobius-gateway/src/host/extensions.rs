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
            let sessions_guard = self.begin_exclusive_mutation().await?;
            let state = self.state.lock().await;
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
            if !self.commit_extensions(&state, next)? {
                return gateway_ready(&state).await;
            }
            drop(state);
            self.finish_extension_mutation(&id, sessions_guard).await
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
            let sessions_guard = self.begin_exclusive_mutation().await?;
            let state = self.state.lock().await;
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
            if !self.commit_extensions(&state, next)? {
                return gateway_ready(&state).await;
            }
            drop(state);
            self.finish_extension_mutation(&id, sessions_guard).await
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
        let sessions_guard = self.begin_exclusive_mutation().await?;
        let state = self.state.lock().await;
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
        if !self.commit_extensions(&state, next)? {
            return gateway_ready(&state).await;
        }
        drop(state);
        self.finish_extension_mutation(&id, sessions_guard).await
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
        let sessions_guard = self.begin_exclusive_mutation().await?;
        let state = self.state.lock().await;
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
        if !self.commit_extensions(&state, next)? {
            return gateway_ready(&state).await;
        }
        drop(state);
        self.finish_extension_mutation(&id, sessions_guard).await
    }

    fn commit_extensions(
        &self,
        state: &GatewayState,
        next: GatewayConfig,
    ) -> std::result::Result<bool, Rejection> {
        let mut current = state
            .config
            .lock()
            .map_err(|_| internal("gateway configuration lock is poisoned"))?;
        if *current == next {
            return Ok(false);
        }
        state.store.save(&next).map_err(internal)?;
        *current = next;
        Ok(true)
    }

    async fn finish_extension_mutation(
        &self,
        id: &str,
        _sessions_guard: tokio::sync::OwnedRwLockWriteGuard<()>,
    ) -> std::result::Result<ReadyPayload, Rejection> {
        let state = self.state.lock().await;
        for prepared in state.bots.prepared.lock().await.values() {
            if prepared.bot.config.config.extensions.contains(id) {
                prepared.invalidate();
            }
        }
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
        let gateway = GatewayHost::start(store, config, credentials, bots)
            .await
            .expect("gateway");
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

    #[tokio::test]
    async fn startup_rejects_a_bot_with_a_missing_selected_extension() {
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

        let error = match GatewayHost::start(store, config, credentials, bots).await {
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
    async fn extension_changes_only_rebuild_dependent_bots_when_used() {
        let (root, gateway) = gateway_with_selected_extension().await;
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let (bot, other, digest, operations) = {
            let state = gateway.state.lock().await;
            let bots = state.bots.bots().expect("Bots");
            let bot = bots
                .iter()
                .find(|bot| bot.config.config.extensions.contains(EXTENSION_ID))
                .unwrap()
                .clone();
            let other = bots
                .iter()
                .find(|bot| !bot.config.config.extensions.contains(EXTENSION_ID))
                .unwrap()
                .clone();
            let mut config = state.config.lock().unwrap();
            let installed = config.installed_extensions.get_mut(EXTENSION_ID).unwrap();
            installed.hooks.push(ExtensionHookRecord {
                event: "SessionStart".into(),
                matcher: None,
                command: "true".into(),
                timeout_seconds: 5,
            });
            (
                bot,
                other,
                installed.digest.clone(),
                Arc::clone(&state.store.runtime_operations),
            )
        };
        let selected = gateway.create_session(&workspace, &bot.id).await.unwrap();
        let sibling = gateway.create_session(&workspace, &bot.id).await.unwrap();
        let unrelated = gateway.create_session(&workspace, &other.id).await.unwrap();
        let before = operations.counts();
        gateway
            .set_extension_hooks_trusted(EXTENSION_ID.into(), digest, true)
            .await
            .unwrap();
        assert_eq!(
            operations.counts(),
            before,
            "editing an extension does no preparation or assembly"
        );
        unrelated.accepts_file_attachments().await.unwrap();
        assert_eq!(operations.counts(), before, "unrelated Bot stays prepared");
        selected.accepts_file_attachments().await.unwrap();
        sibling.accepts_file_attachments().await.unwrap();
        assert_eq!(
            operations.counts(),
            (before.0 + 1, before.1 + 2),
            "one preparation shared by both dependent chats"
        );
        let before_noop = operations.counts();
        let digest = gateway
            .state
            .lock()
            .await
            .config
            .lock()
            .unwrap()
            .installed_extensions[EXTENSION_ID]
            .digest
            .clone();
        gateway
            .set_extension_hooks_trusted(EXTENSION_ID.into(), digest, true)
            .await
            .unwrap();
        selected.accepts_file_attachments().await.unwrap();
        assert_eq!(
            operations.counts(),
            before_noop,
            "unchanged extension trust does no runtime work"
        );
        gateway.shutdown().await;
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
