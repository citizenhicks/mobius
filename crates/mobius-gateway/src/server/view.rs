//! What one connection's client already holds of the gateway catalog, so it receives only
//! what changed.

use std::collections::HashMap;

use super::*;
use crate::wire::{
    BotRecord, CatalogHint, READY_SECTIONS, ReadySection, SessionRecord, SessionSlot,
    apply_session_changes, content_revision,
};

pub(super) struct ClientView {
    pub(super) desktop_transport: bool,
    pub(super) local: bool,
    skip: BTreeSet<ReadySection>,
    /// Revision of each section as this client last received it.
    held: BTreeMap<ReadySection, String>,
    /// The session catalog this client last received.
    sessions: Option<Vec<SessionRecord>>,
}

impl ClientView {
    pub(super) fn new(CatalogHint { known, skip }: CatalogHint) -> Self {
        Self {
            desktop_transport: false,
            local: false,
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
        if matches!(
            payload.computer_view,
            crate::wire::ComputerView::RemoteDesktop
        ) && !self.desktop_transport
            || matches!(
                payload.computer_view,
                crate::wire::ComputerView::EmbeddedBrowser
            ) && !self.local
        {
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
            self.sessions = Some(spare.sessions);
        }
        Ok(())
    }

    /// Writes one gateway broadcast as the changes this client has not seen.
    pub(super) async fn write_broadcast(
        &mut self,
        writer: &mut (impl AsyncWrite + Unpin),
        frame: ServerFrame,
    ) -> Result<()> {
        match frame.message {
            ServerMessage::Ready { .. } => self.write_catalog(writer, frame).await,
            ServerMessage::Sessions {
                request_id: None,
                sessions,
            } => self.write_session_changes(writer, sessions).await,
            ServerMessage::Sessions {
                request_id,
                sessions,
            } => self.write_sessions(writer, request_id, sessions).await,
            ServerMessage::Bots { request_id, bots } => {
                self.write_bots(writer, request_id, bots).await
            }
            message => write_frame(writer, &ServerFrame::new(message)).await,
        }
    }

    /// Writes the whole session catalog, which becomes the base of later changes.
    pub(super) async fn write_sessions(
        &mut self,
        writer: &mut (impl AsyncWrite + Unpin),
        request_id: Option<String>,
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
            self.sessions = Some(sessions);
        }
        Ok(())
    }

    /// Writes a Bot catalog; a broadcast this client already holds is dropped.
    pub(super) async fn write_bots(
        &mut self,
        writer: &mut (impl AsyncWrite + Unpin),
        request_id: Option<String>,
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

    async fn write_session_changes(
        &mut self,
        writer: &mut (impl AsyncWrite + Unpin),
        sessions: Vec<SessionRecord>,
    ) -> Result<()> {
        let previous = self.sessions.take();
        let held = previous.as_deref().unwrap_or_default();
        let unchanged = unchanged_positions(held, &sessions);
        // Before any catalog reached this client, every session counts as changed.
        if previous.is_some()
            && sessions.len() == held.len()
            && unchanged
                .iter()
                .enumerate()
                .all(|(position, found)| *found == Some(position))
        {
            self.sessions = previous;
            return Ok(());
        }
        let revision = content_revision(&sessions);
        let slots = sessions
            .into_iter()
            .zip(unchanged)
            .map(|(session, found)| match found {
                Some(_) => SessionSlot::Unchanged(session.session_id),
                None => SessionSlot::Changed(Box::new(session)),
            })
            .collect();
        let frame = ServerFrame::new(ServerMessage::SessionsChanged {
            sessions: slots,
            revision,
        });
        write_frame(writer, &frame).await?;
        if let ServerMessage::SessionsChanged { sessions, revision } = frame.message {
            self.held.insert(ReadySection::Sessions, revision);
            let mut catalog = previous.unwrap_or_default();
            apply_session_changes(&mut catalog, sessions);
            self.sessions = Some(catalog);
        }
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
        .map(|(position, session)| (session.session_id.as_str(), position))
        .collect();
    current
        .iter()
        .map(|session| {
            index
                .get(session.session_id.as_str())
                .copied()
                .filter(|&position| previous[position] == *session)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, updated_at: i64) -> SessionRecord {
        SessionRecord {
            session_id: id.into(),
            session_context: Default::default(),
            parent_session_id: None,
            parent_sequence: None,
            sequence: 1,
            first_user_message: None,
            execution_stats: Default::default(),
            title: None,
            pinned: false,
            activity: Default::default(),
            created_at: 1,
            updated_at,
        }
    }

    async fn written(view: &mut ClientView, frame: ServerFrame) -> Vec<ServerMessage> {
        let mut bytes = Vec::new();
        view.write_broadcast(&mut bytes, frame)
            .await
            .expect("write");
        let mut reader = FrameReader::new(bytes.as_slice());
        let mut messages = Vec::new();
        while let Some(frame) = read_frame::<ServerFrame>(&mut reader).await.expect("frame") {
            messages.push(frame.message);
        }
        messages
    }

    fn catalog(sessions: Vec<SessionRecord>) -> ServerFrame {
        ServerFrame::new(ServerMessage::Sessions {
            request_id: None,
            sessions,
        })
    }

    #[tokio::test]
    async fn session_broadcasts_become_changes_in_catalog_order() {
        let mut fresh = ClientView::new(CatalogHint::default());
        assert!(
            matches!(written(&mut fresh, catalog(Vec::new())).await.as_slice(),
                [ServerMessage::SessionsChanged { sessions, .. }] if sessions.is_empty()),
            "before any catalog even an empty one is sent"
        );
        let mut view = ClientView::new(CatalogHint::default());
        let first = written(&mut view, catalog(vec![session("a", 1), session("b", 1)])).await;
        assert!(matches!(
            first.as_slice(),
            [ServerMessage::SessionsChanged { sessions, .. }]
                if sessions.iter().all(|slot| matches!(slot, SessionSlot::Changed(_)))
        ));
        assert!(
            written(&mut view, catalog(vec![session("a", 1), session("b", 1)]))
                .await
                .is_empty()
        );
        let changed = written(
            &mut view,
            catalog(vec![session("c", 3), session("b", 2), session("a", 1)]),
        )
        .await;
        let [ServerMessage::SessionsChanged { sessions, .. }] = changed.as_slice() else {
            panic!("expected one change: {changed:?}");
        };
        assert!(matches!(
            sessions.as_slice(),
            [
                SessionSlot::Changed(c),
                SessionSlot::Changed(b),
                SessionSlot::Unchanged(a),
            ] if c.session_id == "c" && b.updated_at == 2 && a == "a"
        ));
        let removed = written(&mut view, catalog(vec![session("b", 2)])).await;
        assert!(matches!(
            removed.as_slice(),
            [ServerMessage::SessionsChanged { sessions, .. }]
                if matches!(sessions.as_slice(), [SessionSlot::Unchanged(b)] if b == "b")
        ));
    }

    #[tokio::test]
    async fn a_bot_catalog_the_client_holds_is_not_sent_again() {
        let mut view = ClientView::new(CatalogHint::default());
        let mut response = Vec::new();
        view.write_bots(&mut response, Some("request".into()), Vec::new())
            .await
            .expect("response");
        assert!(!response.is_empty());
        let echo = ServerFrame::new(ServerMessage::Bots {
            request_id: None,
            bots: Vec::new(),
        });
        assert!(written(&mut view, echo).await.is_empty());
    }
}
