//! The chat catalog re-renders only when a hash of its inputs changes.
//! Transcript text stays outside this scene cache to register selection each frame.

use super::*;
use crate::gateway::Update;
use mobius_gateway::wire::ServerMessage;
use std::hash::{DefaultHasher, Hash, Hasher};

pub(super) struct Region {
    workspace: WeakEntity<Workspace>,
    key: u64,
}

impl Render for Region {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A cached view lays its content out as a root, so the root must fill the box.
        div().size_full().flex().flex_col().children(
            self.workspace
                .update(cx, |workspace, cx| workspace.catalog_region(window, cx))
                .ok(),
        )
    }
}

pub(super) struct Regions {
    pub(super) catalog: Entity<Region>,
    pub(super) catalog_revision: u64,
}

impl Regions {
    pub(super) fn new(cx: &mut Context<Workspace>) -> Self {
        let workspace = cx.weak_entity();
        Self {
            catalog: cx.new(|_| Region { workspace, key: 0 }),
            catalog_revision: 0,
        }
    }

    pub(super) fn gateway_update(&mut self, update: &Update, selected_session: Option<&str>) {
        let catalog = affects_catalog(update, selected_session);
        self.catalog_revision = self.catalog_revision.wrapping_add(u64::from(catalog));
    }
}

/// Gateway-owned inputs consumed by the catalog. Keep this aligned with the reducer.
fn affects_catalog(update: &Update, selected_session: Option<&str>) -> bool {
    let message = match update {
        // These can clear the selected session even when link readiness is already false;
        // the catalog uses that session's workspace to order projects.
        Update::Connecting { .. }
        | Update::Connected(_)
        | Update::Reconnecting { .. }
        | Update::SendFailed { .. } => return true,
        Update::Message(message) => message.as_ref(),
        _ => return false,
    };
    match message {
        ServerMessage::Ready { .. }
        | ServerMessage::GatewayConfigured { .. }
        | ServerMessage::Sessions { .. }
        | ServerMessage::SessionsChanged { .. }
        | ServerMessage::Bots { .. }
        | ServerMessage::SessionOpened { .. } => true,
        ServerMessage::SessionChanged { payload } => {
            selected_session == Some(payload.session.session_id.as_ref())
        }
        _ => false,
    }
}

/// Hashes a region's inputs; region render functions must read nothing else.
pub(super) fn key(inputs: impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    inputs.hash(&mut hasher);
    hasher.finish()
}

impl Workspace {
    /// Places `region` as a cached child that re-renders in this frame only when `key` changed.
    pub(super) fn cached_region(
        region: &Entity<Region>,
        key: u64,
        style: StyleRefinement,
        cx: &mut App,
    ) -> AnyElement {
        region.update(cx, |region, cx| {
            if region.key != key {
                region.key = key;
                cx.notify();
            }
        });
        Entity::clone(region).cached(style).into_any_element()
    }
}
