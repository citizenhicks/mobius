use super::{
    Arc, GatewayHost, Rejection, RemoteDesktop, Uuid, invalid_bot, invalid_config,
    require_catalog_session,
};
use futures_util::StreamExt as _;
use mobius::SessionId;

const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

struct Takeover {
    remote: Option<Arc<RemoteDesktop>>,
    drain: Option<tokio::task::JoinHandle<()>>,
    granted: bool,
}

impl Drop for Takeover {
    fn drop(&mut self) {
        if self.granted {
            return;
        }
        let Some(remote) = self.remote.take() else {
            return;
        };
        let drain = self.drain.take();
        tokio::spawn(async move {
            if let Some(drain) = drain
                && let Err(error) = drain.await
            {
                tracing::warn!(%error, "desktop execution drain failed");
            }
            remote.cancel_takeover().await;
        });
    }
}

impl GatewayHost {
    pub(crate) async fn open_computer(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), Rejection> {
        self.require_desktop_chat(session_id).await?;
        self.remote_desktop
            .show(session_id)
            .await
            .map_err(invalid_config)
    }

    async fn require_desktop_chat(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), Rejection> {
        let (checkpoints, bots) = {
            let state = self.state.lock().await;
            (Arc::clone(&state.checkpoints), Arc::clone(&state.bots))
        };
        let summary = require_catalog_session(&checkpoints, session_id).await?;
        let bot = bots
            .bot(&summary.session_context.owner_id)
            .map_err(invalid_bot)?;
        if !bot
            .config
            .config
            .middleware
            .enabled(mobius::middleware::computer_control::MANIFEST.id)
        {
            return Err(invalid_config(
                "computer control middleware is disabled for this Bot",
            ));
        }
        Ok(())
    }

    pub(crate) async fn set_desktop_control(
        &self,
        connection: Uuid,
        enabled: bool,
        session_id: Option<SessionId>,
    ) -> std::result::Result<(), Rejection> {
        if !enabled {
            return self
                .remote_desktop
                .release_control(connection)
                .await
                .map_err(invalid_config);
        }
        let session_id = session_id
            .ok_or_else(|| invalid_config("choose a chat before taking desktop control"))?;
        self.require_desktop_chat(&session_id).await?;
        self.remote_desktop
            .begin_takeover()
            .map_err(invalid_config)?;
        let gateway = self.clone(); // rules-allow: the spawned drain task owns a gateway handle
        let mut takeover = Takeover {
            remote: Some(Arc::clone(&self.remote_desktop)),
            granted: false,
            drain: Some(tokio::spawn(async move {
                gateway.drain_desktop_execution().await;
            })),
        };
        tokio::time::timeout(
            DRAIN_TIMEOUT,
            takeover
                .drain
                .as_mut()
                .ok_or_else(|| invalid_config("the desktop execution drain is missing"))?,
        )
        .await
        .map_err(|error| {
            invalid_config(format!(
                "execution cleanup timed out; the desktop remains view-only while cleanup finishes: {error}"
            ))
        })?
        .map_err(invalid_config)?;
        takeover.drain.take();
        tokio::time::timeout(
            DRAIN_TIMEOUT,
            self.remote_desktop.grant_control(connection, session_id),
        )
        .await
        .map_err(|error| {
            invalid_config(format!(
                "execution cleanup timed out; the desktop remains view-only while cleanup finishes: {error}"
            ))
        })?
        .map_err(invalid_config)?;
        takeover.granted = true;
        drop(takeover);
        Ok(())
    }

    async fn drain_desktop_execution(&self) {
        // Startup owns this gate through publication. No actor may appear behind the drain.
        let _capacity = self.capacity_gate.lock().await;
        let residents = {
            let mut state = self.state.lock().await;
            state
                .sessions
                .drain()
                .map(|(_, host)| host)
                .collect::<Vec<_>>()
        };
        let mut draining = futures_util::stream::FuturesUnordered::new();
        for host in residents {
            draining.push(async move {
                host.shutdown().await;
            });
        }
        while draining.next().await.is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the remote desktop keeps the change channel open while the test waits"
    )]
    async fn granted_takeover_does_not_schedule_stale_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let remote = Arc::new(RemoteDesktop::new(
            directory.path(),
            false,
            crate::computer_runtime::ComputerConfig::default(),
        ));
        let mut changed = remote.subscribe();
        drop(Takeover {
            remote: Some(Arc::clone(&remote)),
            drain: None,
            granted: true,
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), changed.recv())
                .await
                .is_err()
        );
    }
}
