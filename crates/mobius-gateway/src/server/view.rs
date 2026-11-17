//! What one connection's client already holds of the gateway catalog, so it receives only
//! what changed.

use std::collections::HashMap;
use std::ops::Deref;

use serde::Serialize;

use super::{
    Arc, AsyncWrite, BTreeMap, BTreeSet, Error, Result, ServerFrame, ServerMessage, write_frame,
};
use crate::host::SessionCatalogUpdate;
use crate::wire::{
    AgentCatalog, BackgroundApproval, BotRecord, CatalogHint, ComputerView, HarnessDescriptor,
    ProviderInstance, ProviderStatus, READY_SECTIONS, ReadyPayload, ReadySection, SessionRecord,
    SessionSlot, content_revision,
};
use mobius::protocol::SessionFileLimits;

pub(super) struct ClientView {
    pub(super) desktop_transport: bool,
    pub(super) local: bool,
    pub(super) client: crate::wire::ClientCapabilities,
    skip: BTreeSet<ReadySection>,
    held: BTreeMap<ReadySection, u128>,
    /// The session catalog this client last received.
    sessions: Option<HeldSessions>,
}

enum HeldSessions {
    Catalog(Arc<[SessionRecord]>),
    Ready(Arc<ServerFrame>),
}

impl Deref for HeldSessions {
    type Target = [SessionRecord];

    fn deref(&self) -> &[SessionRecord] {
        match self {
            Self::Catalog(sessions) => sessions,
            Self::Ready(frame) => ready_payload(frame).map_or(&[], |payload| &payload.sessions),
        }
    }
}

fn ready_payload(frame: &ServerFrame) -> Option<&ReadyPayload> {
    match &frame.message {
        ServerMessage::Ready { payload } | ServerMessage::GatewayConfigured { payload, .. } => {
            Some(payload)
        }
        _ => None,
    }
}

fn digest(revision: &str) -> Option<u128> {
    u128::from_str_radix(revision, 16).ok()
}

impl ClientView {
    pub(super) fn new(CatalogHint { known, skip }: CatalogHint) -> Self {
        Self {
            desktop_transport: false,
            local: false,
            client: crate::wire::ClientCapabilities::default(),
            skip,
            held: known
                .iter()
                .filter_map(|(section, revision)| Some((*section, digest(revision)?)))
                .collect(),
            sessions: None,
        }
    }

    /// The client's copy of `section` no longer matches any revision it was told.
    pub(super) fn forget(&mut self, section: ReadySection) {
        self.held.remove(&section);
    }

    pub(super) async fn write_catalog(
        &mut self,
        writer: &mut (impl AsyncWrite + Unpin),
        frame: Arc<ServerFrame>,
    ) -> Result<()> {
        let Some(payload) = ready_payload(&frame) else {
            return write_frame(writer, &*frame).await;
        };
        let viewable = match payload.computer_view {
            ComputerView::RemoteDesktop => {
                self.desktop_transport && self.client.remote_desktop_viewer
            }
            ComputerView::EmbeddedBrowser => self.local && self.client.browser_renderer,
            ComputerView::Unavailable => true,
        };
        let mut omitted = BTreeSet::new();
        for section in READY_SECTIONS {
            let revision = payload
                .revisions
                .get(&section)
                .and_then(|value| digest(value));
            if self.skip.contains(&section)
                || (revision.is_some() && self.held.get(&section) == revision.as_ref())
            {
                omitted.insert(section);
            }
        }
        let request_id = match &frame.message {
            ServerMessage::GatewayConfigured { request_id, .. } => Some(&**request_id),
            _ => None,
        };
        let empty_catalog = AgentCatalog::default();
        let encoded = CatalogFrame::new(
            frame.version,
            request_id,
            CatalogPayload::new(payload, &omitted, viewable, &empty_catalog),
        );
        write_frame(writer, &encoded).await?;
        for (section, revision) in &payload.revisions {
            if let (false, Some(revision)) = (self.skip.contains(section), digest(revision)) {
                self.held.insert(*section, revision);
            }
        }
        if !self.skip.contains(&ReadySection::Sessions) {
            // Sent or omitted, the client now holds exactly this catalog.
            self.sessions = Some(match Arc::try_unwrap(frame) {
                Ok(frame) => {
                    let (ServerMessage::Ready { payload }
                    | ServerMessage::GatewayConfigured { payload, .. }) = frame.message
                    else {
                        return Err(Error::Internal("catalog frame lost its payload".into()));
                    };
                    HeldSessions::Catalog(payload.sessions.into())
                }
                Err(frame) => HeldSessions::Ready(frame),
            });
        }
        Ok(())
    }

    /// Writes the whole session catalog, which becomes the base of later changes.
    pub(super) async fn write_sessions(
        &mut self,
        writer: &mut (impl AsyncWrite + Unpin),
        request_id: Option<Arc<str>>,
        sessions: Vec<SessionRecord>,
    ) -> Result<()> {
        if let Some(revision) = digest(&content_revision(&sessions)) {
            self.held.insert(ReadySection::Sessions, revision);
        }
        let frame = ServerFrame::new(ServerMessage::Sessions {
            request_id,
            sessions,
        });
        write_frame(writer, &frame).await?;
        if let ServerMessage::Sessions { sessions, .. } = frame.message {
            self.sessions = Some(HeldSessions::Catalog(sessions.into()));
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
        let revision = digest(&content_revision(&bots));
        if request_id.is_none()
            && revision.is_some()
            && self.held.get(&ReadySection::Bots) == revision.as_ref()
        {
            return Ok(());
        }
        if let Some(revision) = revision {
            self.held.insert(ReadySection::Bots, revision);
        }
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
        if let Some(revision) = digest(&catalog.revision) {
            self.held.insert(ReadySection::Sessions, revision);
        }
        // The client applied these changes to what it held and now holds this catalog.
        self.sessions = Some(HeldSessions::Catalog(catalog.sessions));
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

#[derive(Serialize)]
struct CatalogFrame<'frame> {
    version: u16,
    #[serde(flatten)]
    message: CatalogMessage<'frame>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CatalogMessage<'frame> {
    Ready {
        payload: CatalogPayload<'frame>,
    },
    GatewayConfigured {
        request_id: &'frame str,
        payload: CatalogPayload<'frame>,
    },
}

impl<'frame> CatalogFrame<'frame> {
    const fn new(
        version: u16,
        request_id: Option<&'frame str>,
        payload: CatalogPayload<'frame>,
    ) -> Self {
        let message = match request_id {
            Some(request_id) => CatalogMessage::GatewayConfigured {
                request_id,
                payload,
            },
            None => CatalogMessage::Ready { payload },
        };
        Self { version, message }
    }
}

#[derive(Serialize)]
struct CatalogPayload<'frame> {
    gateway_version: &'frame str,
    machine_name: &'frame str,
    computer_view: ComputerView,
    bots: &'frame [BotRecord],
    sessions: &'frame [SessionRecord],
    background_approvals: &'frame [Arc<BackgroundApproval>],
    providers: &'frame [ProviderStatus],
    provider_instances: &'frame [ProviderInstance],
    agent_catalog: &'frame AgentCatalog,
    harnesses: &'frame [HarnessDescriptor],
    max_active_sessions: usize,
    session_file_limits: SessionFileLimits,
    revisions: &'frame BTreeMap<ReadySection, String>,
    omitted: &'frame BTreeSet<ReadySection>,
}

impl<'frame> CatalogPayload<'frame> {
    fn new(
        payload: &'frame ReadyPayload,
        omitted: &'frame BTreeSet<ReadySection>,
        viewable: bool,
        empty_catalog: &'frame AgentCatalog,
    ) -> Self {
        let sent = |section| !omitted.contains(&section);
        let config = sent(ReadySection::Config);
        Self {
            gateway_version: &payload.gateway_version,
            machine_name: &payload.machine_name,
            computer_view: if viewable {
                payload.computer_view
            } else {
                ComputerView::Unavailable
            },
            bots: if sent(ReadySection::Bots) {
                &payload.bots
            } else {
                &[]
            },
            sessions: if sent(ReadySection::Sessions) {
                &payload.sessions
            } else {
                &[]
            },
            background_approvals: &payload.background_approvals,
            providers: if config { &payload.providers } else { &[] },
            provider_instances: if config {
                &payload.provider_instances
            } else {
                &[]
            },
            agent_catalog: if config {
                &payload.agent_catalog
            } else {
                empty_catalog
            },
            harnesses: if sent(ReadySection::Harnesses) {
                &payload.harnesses
            } else {
                &[]
            },
            max_active_sessions: payload.max_active_sessions,
            session_file_limits: payload.session_file_limits,
            revisions: &payload.revisions,
            omitted,
        }
    }
}
