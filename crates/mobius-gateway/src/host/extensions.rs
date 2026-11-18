use super::gateway_host::GatewayHost;
use super::publication::finish_publication;
use super::rejection::{Rejection, internal, invalid_config};
use super::state::{GatewayState, gateway_ready_after_unlock};

use crate::Error;
use crate::config::{ConfigStore, GatewayConfig};
use crate::harness::mobius::extensions::ExtensionStore;
use crate::wire::ReadyPayload;
use std::sync::Arc;

impl GatewayHost {
    pub(crate) async fn install_extension(
        &self,
        source: String,
        reference: Option<String>,
        subdirectory: Option<String>,
    ) -> std::result::Result<Arc<ReadyPayload>, Rejection> {
        let (mutation, store) = {
            let state = self.state.lock().await;
            (Arc::clone(&state.extension_mutations), state.store.clone()) // rules-allow: lock_owned and staging run after the state lock is released
        };
        let _extension_mutation = mutation.lock_owned().await;
        let staged = ExtensionStore::new(&store)
            .stage(&source, reference.as_deref(), subdirectory.as_deref())
            .await
            .map_err(invalid_config)?;
        let staged_digest = Arc::clone(&staged.installed.digest);
        let snapshot_created = staged.snapshot_created;
        let id = Arc::clone(&staged.id);
        let result = self
            .mutate_extension(&id, |next, _| {
                if next
                    .installed_extensions
                    .insert(staged.id, staged.installed)
                    .is_some()
                {
                    return Err(invalid_config(Error::Invalid(format!(
                        "extension `{id}` is already installed"
                    ))));
                }
                Ok(())
            })
            .await;
        if result.is_err() {
            self.discard_unreferenced_snapshot(&store, &staged_digest, snapshot_created)
                .await;
        }
        result
    }

    pub(crate) async fn update_extension(
        &self,
        id: Arc<str>,
    ) -> std::result::Result<Arc<ReadyPayload>, Rejection> {
        let (mutation, store) = {
            let state = self.state.lock().await;
            (Arc::clone(&state.extension_mutations), state.store.clone()) // rules-allow: lock_owned and staging run after the state lock is released
        };
        let _extension_mutation = mutation.lock_owned().await;
        let installed = {
            let state = self.state.lock().await;
            let config = state.config()?;
            config
                .installed_extensions
                .get(&*id)
                // Asynchronous staging retains the original installation for concurrent-change validation.
                .cloned() // rules-allow: the original installation validates concurrent changes after staging
                .ok_or_else(|| unknown_extension(&id))?
        };
        let staged = ExtensionStore::new(&store)
            .stage(
                &installed.source.url,
                installed.source.reference.as_deref(),
                installed.source.subdirectory.as_deref(),
            )
            .await
            .map_err(invalid_config)?;
        if staged.id != id || staged.installed.kind != installed.kind {
            self.discard_unreferenced_snapshot(
                &store,
                &staged.installed.digest,
                staged.snapshot_created,
            )
            .await;
            return Err(invalid_config(Error::Invalid(
                "an extension update cannot change package identity".into(),
            )));
        }
        let staged_digest = Arc::clone(&staged.installed.digest);
        let snapshot_created = staged.snapshot_created;
        let result = self
            .mutate_extension(&id, |next, _| {
                let current = next
                    .installed_extensions
                    .get_mut(&*id)
                    .filter(|entry| **entry == installed)
                    .ok_or_else(|| {
                        Rejection::new(
                            "extension_changed",
                            format!("extension `{id}` changed while its update was prepared"),
                        )
                    })?;
                current.replace_snapshot(staged.installed);
                Ok(())
            })
            .await;
        if result.is_err() {
            self.discard_unreferenced_snapshot(&store, &staged_digest, snapshot_created)
                .await;
        }
        result
    }

    pub(crate) async fn uninstall_extension(
        &self,
        id: Arc<str>,
    ) -> std::result::Result<Arc<ReadyPayload>, Rejection> {
        let mutation = {
            let state = self.state.lock().await;
            Arc::clone(&state.extension_mutations)
        };
        let _extension_mutation = mutation.lock_owned().await;
        self.mutate_extension(&id, |next, state| {
            let selected_by = state
                .bots
                .bots()
                .map_err(internal)?
                .into_iter()
                .find(|bot| {
                    bot.config
                        .config
                        .mobius()
                        .is_some_and(|composition| composition.extensions.contains(&*id))
                })
                .map(|bot| format!("Bot @{}", bot.handle));
            let selected_by = selected_by.or_else(|| {
                next.bot_defaults.as_ref()
                    .filter(|agent| agent.config.extensions.contains(&*id))
                    .map(|_| "the default Bot template".to_owned())
            });
            if let Some(selected_by) = selected_by {
                let message = format!("extension `{id}` is selected by {selected_by}; remove it from that profile first");
                return Err(Rejection::new("extension_in_use", message));
            }
            next.installed_extensions.remove(&*id).ok_or_else(|| unknown_extension(&id))?;
            Ok(())
        }).await
    }

    pub(crate) async fn set_extension_hooks_trusted(
        &self,
        id: Arc<str>,
        expected_digest: Arc<str>,
        trusted: bool,
    ) -> std::result::Result<Arc<ReadyPayload>, Rejection> {
        let mutation = {
            let state = self.state.lock().await;
            Arc::clone(&state.extension_mutations)
        };
        let _extension_mutation = mutation.lock_owned().await;
        self.mutate_extension(&id, |next, _| {
            let installed = next
                .installed_extensions
                .get_mut(&*id)
                .ok_or_else(|| unknown_extension(&id))?;
            if installed.digest != expected_digest {
                return Err(Rejection::new(
                    "extension_changed",
                    format!("extension `{id}` changed before its hook trust changed"),
                ));
            }
            if installed.hooks.is_empty() {
                return Err(invalid_config(Error::Invalid(format!(
                    "extension `{id}` has no executable hooks"
                ))));
            }
            installed.trusted_hook_digest = trusted.then(|| Arc::clone(&installed.digest));
            Ok(())
        })
        .await
    }

    fn commit_extensions(
        state: &GatewayState,
        next: GatewayConfig,
    ) -> std::result::Result<Option<crate::publication::Outcome>, Rejection> {
        let mut current = state.config()?;
        if *current == next {
            return Ok(None);
        }
        let next = next.validated().map_err(invalid_config)?;
        let publication =
            crate::publication::Outcome::applied(state.store.save(&next)).map_err(internal)?;
        *current = next.into_inner();
        drop(current);
        Ok(Some(publication))
    }

    async fn mutate_extension(
        &self,
        id: &str,
        edit: impl FnOnce(&mut GatewayConfig, &GatewayState) -> std::result::Result<(), Rejection>,
    ) -> std::result::Result<Arc<ReadyPayload>, Rejection> {
        let _sessions_guard = self.begin_exclusive_mutation().await?;
        let state = self.state.lock().await;
        let mut next = state.config()?.clone(); // rules-allow: validate and save a candidate before replacing live configuration
        edit(&mut next, &state)?;
        let Some(publication) = Self::commit_extensions(&state, next)? else {
            return gateway_ready_after_unlock(state).await;
        };
        let ready = match state
            .bots
            .prepared
            .invalidate(|prepared| Ok(prepared.uses_extension(id)))
            .await
        {
            Ok(()) => self.publish_ready_after_unlock(state).await,
            Err(error) => {
                drop(state);
                Err(error)
            }
        };
        finish_publication(publication.confirm(), ready)
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
                    .any(|extension| *extension.digest == *digest)
            })
        };
        if !referenced && let Err(error) = ExtensionStore::new(store).remove_snapshot(digest) {
            tracing::warn!(%error, "failed to remove an unreferenced extension snapshot");
        }
    }
}

fn unknown_extension(id: &str) -> Rejection {
    Rejection::new(
        "unknown_extension",
        format!("extension `{id}` is not installed"),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::harness::mobius::extensions::{ExtensionSource, InstalledExtension};
    use crate::wire::{AgentComposition, ExtensionHookRecord, ExtensionKind, ServerMessage};

    const EXTENSION_ID: &str = "plugin:fixture";

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
        let gateway = crate::host::tests::start_test_gateway(store, config).await;
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
                .create_bot("fixture", "Fixture", bot_config.into())
                .expect("Bot");
        }
        (root, gateway)
    }

    fn installed_extension() -> InstalledExtension {
        InstalledExtension {
            kind: ExtensionKind::Plugin,
            name: "fixture".into(),
            description: "Fixture".into(),
            version: None,
            source: ExtensionSource {
                url: "https://example.com/fixture.git".into(),
                reference: None,
                subdirectory: None,
            },
            resolved_revision: "a".repeat(40).into(),
            digest: "b".repeat(64).into(),
            skills: Arc::from(["fixture".into()]),
            hooks: Arc::from([ExtensionHookRecord {
                event: "SessionStart".into(),
                matcher: None,
                command: "true".into(),
                timeout_seconds: 5,
            }]),
            trusted_hook_digest: None,
        }
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
    async fn hook_trust_persists_and_only_broadcasts_when_changed() {
        let (root, gateway) = gateway_with_selected_extension().await;
        let mut updates = gateway.subscribe();
        for changed in [true, false] {
            let ready = gateway
                .set_extension_hooks_trusted(EXTENSION_ID.into(), "b".repeat(64).into(), true)
                .await
                .unwrap();
            assert!(
                ready
                    .agent_catalog
                    .extensions
                    .iter()
                    .any(|ext| &*ext.id == EXTENSION_ID && ext.hooks_trusted)
            );
            if changed {
                assert!(matches!(
                    updates.try_recv().unwrap().message(),
                    ServerMessage::Ready { .. }
                ));
            } else {
                assert!(matches!(
                    updates.try_recv(),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                ));
            }
        }
        let (_, saved) = ConfigStore::open(root.path().join("state")).unwrap();
        let installed = &saved.installed_extensions[EXTENSION_ID];
        assert_eq!(
            installed.trusted_hook_digest.as_ref(),
            Some(&installed.digest)
        );
    }

    #[test]
    fn update_keeps_hook_trust_only_for_the_same_snapshot() {
        let mut installed = installed_extension();
        installed.trusted_hook_digest = Some(Arc::clone(&installed.digest));
        installed.replace_snapshot(installed_extension());
        assert_eq!(
            installed.trusted_hook_digest.as_ref(),
            Some(&installed.digest)
        );

        let changed = InstalledExtension {
            digest: "c".repeat(64).into(),
            ..installed_extension()
        };
        installed.replace_snapshot(changed);
        assert!(installed.trusted_hook_digest.is_none());
    }
}
