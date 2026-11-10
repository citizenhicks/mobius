//! What one connection's client already holds of the gateway catalog, so it receives only
//! what changed.

use std::collections::HashMap;

use super::{Arc, AsyncWrite, BTreeMap, BTreeSet, Result, ServerFrame, ServerMessage, write_frame};
use crate::host::SessionCatalogUpdate;
use crate::wire::{
    BotRecord, CatalogHint, READY_SECTIONS, ReadySection, SessionRecord, SessionSlot,
    content_revision,
};

pub(super) struct ClientView {
    pub(super) desktop_transport: bool,
    pub(super) local: bool,
    pub(super) client: crate::wire::ClientCapabilities,
    skip: BTreeSet<ReadySection>,
    /// Revision of each section as this client last received it.
    held: BTreeMap<ReadySection, String>,
    /// The session catalog this client last received.
    sessions: Option<Arc<[SessionRecord]>>,
}

impl ClientView {
    pub(super) fn new(CatalogHint { known, skip }: CatalogHint) -> Self {
        Self {
            desktop_transport: false,
            local: false,
            client: crate::wire::ClientCapabilities::default(),
            skip,
            held: known,
            sessions: None,
        }
    }

    /// The client's copy of `section` no longer matches any revision it was told.
    pub(super) fn forget(&mut self, section: ReadySection) {
        self.held.remove(&section);
    }

    /// Writes a Ready or GatewayConfigured frame without the sections this client holds.
    pub(super) async fn write_catalog(
        &mut self,
        writer: &mut (impl AsyncWrite + Unpin),
        mut frame: ServerFrame,
    ) -> Result<()> {
        let (ServerMessage::Ready { payload } | ServerMessage::GatewayConfigured { payload, .. }) =
            &mut frame.message
        else {
            return write_frame(writer, &frame).await;
        };
        let viewable = match payload.computer_view {
            crate::wire::ComputerView::RemoteDesktop => {
                self.desktop_transport && self.client.remote_desktop_viewer
            }
            crate::wire::ComputerView::EmbeddedBrowser => {
                self.local && self.client.browser_renderer
            }
            crate::wire::ComputerView::Unavailable => true,
        };
        if !viewable {
            payload.computer_view = crate::wire::ComputerView::Unavailable;
        }
        let mut spare = payload.blank();
        for section in READY_SECTIONS {
            let revision = payload.revisions.get(&section);
            if self.skip.contains(&section)
                || (revision.is_some() && self.held.get(&section) == revision)
            {
                payload.swap_section(&mut spare, section);
                payload.omitted.insert(section);
            }
        }
        write_frame(writer, &frame).await?;
        let (ServerMessage::Ready { payload } | ServerMessage::GatewayConfigured { payload, .. }) =
            &mut frame.message
        else {
            return Ok(());
        };
        for (section, revision) in std::mem::take(&mut payload.revisions) {
            if !self.skip.contains(&section) {
                self.held.insert(section, revision);
            }
        }
        if !self.skip.contains(&ReadySection::Sessions) {
            // Sent or omitted, the client now holds exactly this catalog.
            if !payload.omitted.contains(&ReadySection::Sessions) {
                payload.swap_section(&mut spare, ReadySection::Sessions);
            }
            self.sessions = Some(spare.sessions.into());
        }
        Ok(())
    }

    /// Writes one shared gateway broadcast frame, a Ready catalog without what this client
    /// holds.
    pub(super) async fn write_broadcast(
        &mut self,
        writer: &mut (impl AsyncWrite + Unpin),
        frame: Arc<ServerFrame>,
    ) -> Result<()> {
        if matches!(frame.message, ServerMessage::Ready { .. }) {
            self.write_catalog(writer, Arc::unwrap_or_clone(frame))
                .await
        } else {
            write_frame(writer, &*frame).await
        }
    }

    /// Writes the whole session catalog, which becomes the base of later changes.
    pub(super) async fn write_sessions(
        &mut self,
        writer: &mut (impl AsyncWrite + Unpin),
        request_id: Option<Arc<str>>,
        sessions: Vec<SessionRecord>,
    ) -> Result<()> {
        self.held
            .insert(ReadySection::Sessions, content_revision(&sessions));
        let frame = ServerFrame::new(ServerMessage::Sessions {
            request_id,
            sessions,
        });
        write_frame(writer, &frame).await?;
        if let ServerMessage::Sessions { sessions, .. } = frame.message {
            self.sessions = Some(sessions.into());
        }
        Ok(())
    }

    /// Writes a Bot catalog; a broadcast this client already holds is dropped.
    pub(super) async fn write_bots(
        &mut self,
        writer: &mut (impl AsyncWrite + Unpin),
        request_id: Option<Arc<str>>,
        bots: Vec<BotRecord>,
    ) -> Result<()> {
        let revision = content_revision(&bots);
        if request_id.is_none() && self.held.get(&ReadySection::Bots) == Some(&revision) {
            return Ok(());
        }
        self.held.insert(ReadySection::Bots, revision);
        write_frame(
            writer,
            &ServerFrame::new(ServerMessage::Bots { request_id, bots }),
        )
        .await
    }

    /// Writes a broadcast session catalog as the changes this client has not seen.
    pub(super) async fn write_session_changes(
        &mut self,
        writer: &mut (impl AsyncWrite + Unpin),
        catalog: SessionCatalogUpdate,
    ) -> Result<()> {
        let previous = self.sessions.take();
        let held = previous.as_deref().unwrap_or_default();
        let unchanged = unchanged_positions(held, &catalog.sessions);
        // Before any catalog reached this client, every session counts as changed.
        if previous.is_some()
            && catalog.sessions.len() == held.len()
            && unchanged
                .iter()
                .enumerate()
                .all(|(position, found)| *found == Some(position))
        {
            self.sessions = previous;
            return Ok(());
        }
        let slots = catalog
            .sessions
            .iter()
            .zip(unchanged)
            .map(|(session, found)| match found {
                Some(_) => SessionSlot::Unchanged(session.session_id.share()),
                None => SessionSlot::Changed(Box::new(session.clone())), // rules-allow: the frame owns each changed record; the shared catalog stays intact
            })
            .collect();
        let frame = ServerFrame::new(ServerMessage::SessionsChanged {
            sessions: slots,
            revision: catalog.revision.to_string(),
        });
        write_frame(writer, &frame).await?;
        if let ServerMessage::SessionsChanged { revision, .. } = frame.message {
            self.held.insert(ReadySection::Sessions, revision);
        }
        // The client applied these changes to what it held and now holds this catalog.
        self.sessions = Some(catalog.sessions);
        Ok(())
    }
}

/// For each current session, its position in `previous` when it did not change.
fn unchanged_positions(
    previous: &[SessionRecord],
    current: &[SessionRecord],
) -> Vec<Option<usize>> {
    let index: HashMap<&str, usize> = previous
        .iter()
        .enumerate()
        .map(|(position, session)| (session.session_id.as_ref(), position))
        .collect();
    current
        .iter()
        .map(|session| {
            index
                .get(session.session_id.as_ref())
                .copied()
                .filter(|&position| previous.get(position) == Some(session))
        })
        .collect()
}
