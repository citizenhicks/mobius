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
            let _sessions = gate.write_owned().await;
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
            self.finish_extension_mutation(sessions, &id).await
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
            let _sessions = gate.write_owned().await;
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
            self.finish_extension_mutation(sessions, &id).await
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
        let _sessions = gate.write_owned().await;
        let next = {
            let current = state
                .config
                .lock()
                .map_err(|_| internal("gateway configuration lock is poisoned"))?;
            let mut next = current.clone();
            next.installed_extensions
                .remove(&id)
                .ok_or_else(|| unknown_extension(&id))?;
            next
        };
        self.commit_extensions(&state, next)?;
        let sessions = state.sessions.values().cloned().collect();
        drop(state);
        self.finish_extension_mutation(sessions, &id).await
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
        let _sessions = gate.write_owned().await;
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
        self.finish_extension_mutation(sessions, &id).await
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
    ) -> std::result::Result<ReadyPayload, Rejection> {
        for host in sessions {
            if let Err(rejection) = host.refresh_extension(id.to_owned()).await
                && rejection.code != "gateway_stopped"
            {
                return Err(rejection);
            }
        }
        let state = self.state.lock().await;
        let payload = gateway_ready(&state).await?;
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
    use crate::config::{ConfigStore, CredentialStore};
    use crate::cron::CronStore;
    use crate::extensions::{ExtensionSource, InstalledExtension};
    use crate::wire::{ExtensionHookRecord, ExtensionKind};

    const EXTENSION_ID: &str = "skill:fixture";

    async fn gateway_with_selected_extension() -> (tempfile::TempDir, GatewayHost, HostHandle) {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let listen = "127.0.0.1:8741".parse().expect("listen address");
        let (store, config) =
            ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
        let mut config = config
            .registering_provider(
                AgentComposition::default().provider,
                "Test".into(),
                Default::default(),
                Vec::new(),
                Vec::new(),
            )
            .expect("provider");
        config
            .default_agent
            .as_mut()
            .expect("default agent")
            .config
            .extensions
            .insert(EXTENSION_ID.into());
        let credentials =
            Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
        let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
        let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
        let host = gateway
            .create_session(&workspace)
            .await
            .expect("create session");
        {
            let state = gateway.state.lock().await;
            state
                .config
                .lock()
                .expect("gateway config")
                .installed_extensions
                .insert(EXTENSION_ID.into(), installed_extension());
        }
        (root, gateway, host)
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
    async fn committed_mutation_refreshes_selected_session_before_ready_failure() {
        let (_root, gateway, host) = gateway_with_selected_extension().await;
        let mut updates = host.subscribe();
        let activities = Arc::clone(&gateway.state.lock().await.activities);
        let poisoned = std::thread::spawn(move || {
            let _guard = activities.lock().expect("activities");
            panic!("poison activities");
        })
        .join();
        assert!(poisoned.is_err());

        let rejection = gateway
            .uninstall_extension(EXTENSION_ID.into())
            .await
            .expect_err("ready must fail");
        let installed = gateway
            .state
            .lock()
            .await
            .config
            .lock()
            .expect("gateway config")
            .installed_extensions
            .contains_key(EXTENSION_ID);

        assert!(!installed);
        assert!(
            rejection
                .message
                .contains("session activity lock is poisoned")
        );
        assert!(matches!(
            updates.try_recv().expect("session refresh"),
            ServerFrame {
                message: ServerMessage::SessionChanged { .. },
                ..
            }
        ));
    }

    #[tokio::test]
    async fn committed_mutation_ignores_stopped_session_refresh() {
        let (_root, gateway, host) = gateway_with_selected_extension().await;
        let mut updates = gateway.subscribe();
        assert!(host.stop_if_idle().await);

        let ready = gateway
            .uninstall_extension(EXTENSION_ID.into())
            .await
            .expect("stopped session refresh must not reject a committed mutation");
        let installed = gateway
            .state
            .lock()
            .await
            .config
            .lock()
            .expect("gateway config")
            .installed_extensions
            .contains_key(EXTENSION_ID);

        assert!(!installed);
        assert!(
            ready
                .extensions
                .iter()
                .all(|extension| extension.id != EXTENSION_ID)
        );
        assert!(matches!(
            updates.try_recv().expect("gateway-ready broadcast"),
            ServerFrame {
                message: ServerMessage::Ready { .. },
                ..
            }
        ));
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
