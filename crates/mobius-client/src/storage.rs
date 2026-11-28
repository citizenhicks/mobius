//! Gateway storage measurements, file deletions and sequential chat exports.

use std::collections::BTreeSet;
use std::iter::Enumerate;
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::vec::IntoIter;

use super::Gateway;
use mobius::SessionId;
use mobius::backend::session_files::{SessionFileOrigin, SessionFileSelection};
use mobius_gateway::storage_usage::StorageUsage;
use mobius_gateway::wire::{ClientMessage, ServerMessage};

/// What a storage message means for the interface.
#[derive(Debug, PartialEq, Eq)]
pub enum StorageEvent {
    /// A purge finished; this many bytes were freed.
    Freed(u64),
    /// The gateway refused a write because storage is full.
    Full,
    /// Files were removed; listings and thumbnails are stale.
    FilesRemoved,
    /// Measure again now.
    Measure,
    /// The gateway asked to measure later.
    MeasureLater,
}

#[derive(Debug, Default)]
enum Export {
    #[default]
    Optional,
    SelectionChanged,
    Running {
        request_id: Arc<str>,
        batch: ExportBatch,
    },
    Saved,
}

#[derive(Debug)]
struct ExportBatch {
    folder: PathBuf,
    sessions: Enumerate<IntoIter<SessionId>>,
    uploads: bool,
    artifacts: bool,
}

#[derive(Debug, Default)]
pub struct Storage {
    pub usage: Option<Arc<StorageUsage>>,
    /// The measurement in flight.
    pub request: Option<Arc<str>>,
    pending: BTreeSet<Arc<str>>,
    before: Option<u64>,
    export: Export,
}

impl Storage {
    /// A deletion or export is still running.
    pub fn busy(&self) -> bool {
        !self.pending.is_empty() || self.exporting()
    }

    pub fn exporting(&self) -> bool {
        matches!(self.export, Export::Running { .. })
    }

    pub fn export_saved(&self) -> bool {
        matches!(self.export, Export::Saved)
    }

    pub fn can_purge(&self) -> bool {
        self.pending.is_empty() && matches!(self.export, Export::Optional | Export::Saved)
    }

    /// Keeps an active export and its selection when the dialog reopens.
    pub fn open_cleanup(&mut self) -> bool {
        if self.exporting() {
            return false;
        }
        self.export = Export::Optional;
        true
    }

    pub fn selection_changed(&mut self) {
        if self.export_saved() {
            self.export = Export::SelectionChanged;
        }
    }

    /// The request that measures storage, unless one is already in flight.
    pub fn measure(&mut self) -> Option<ClientMessage> {
        if self.request.is_some() {
            return None;
        }
        let request_id = super::request_id();
        self.request = Some(Arc::clone(&request_id));
        Some(ClientMessage::GetStorageUsage { request_id })
    }

    pub fn receive(&mut self, message: &ServerMessage) -> Vec<StorageEvent> {
        let mut events = Vec::new();
        match message {
            ServerMessage::StorageUsage { request_id, usage }
                if self.request.as_ref() == Some(request_id) =>
            {
                self.request = None;
                if let Some(before) = self.before.take() {
                    events.push(StorageEvent::Freed(before.saturating_sub(usage.used_bytes)));
                }
                self.usage = Some(Arc::clone(usage));
            }
            ServerMessage::Accepted { request_id } if self.pending.remove(request_id) => {
                events.push(StorageEvent::FilesRemoved);
                if self.pending.is_empty() {
                    self.request = None;
                    events.push(StorageEvent::Measure);
                }
            }
            ServerMessage::Rejected {
                request_id, code, ..
            } => {
                if code == "storage_full" {
                    events.push(StorageEvent::Full);
                }
                if self.request.as_ref() == Some(request_id) {
                    self.request = None;
                    if code == "rate_limited" {
                        events.push(StorageEvent::MeasureLater);
                    }
                }
                if self.pending.remove(request_id) {
                    self.before = None;
                    events.push(StorageEvent::Measure);
                }
            }
            _ => {}
        }
        events
    }

    /// The deletions that purge `kinds` from `sessions`, in batches the gateway accepts.
    pub fn purge(
        &mut self,
        sessions: &[SessionId],
        origins: &[SessionFileOrigin],
    ) -> Vec<ClientMessage> {
        self.before = self.usage.as_ref().map(|usage| usage.used_bytes);
        sessions
            .chunks(1024)
            .map(|batch| {
                let request_id = super::request_id();
                self.pending.insert(Arc::clone(&request_id));
                ClientMessage::DeleteSessions {
                    request_id,
                    session_ids: batch.iter().map(SessionId::share).collect(),
                    selection: SessionFileSelection::Origins(origins.to_vec()),
                }
            })
            .collect()
    }

    /// Follows a deletion sent elsewhere, so its answer refreshes storage.
    pub fn track(&mut self, request_id: Arc<str>) {
        self.pending.insert(request_id);
    }

    /// Starts a sequential export of the selected chats.
    ///
    /// # Errors
    ///
    /// When another export is active or the gateway refuses the first request.
    pub fn begin_export(
        &mut self,
        gateway: Option<&Gateway>,
        folder: PathBuf,
        sessions: Vec<SessionId>,
        kinds: [bool; 3],
    ) -> Result<(), String> {
        if self.exporting() {
            return Err("An export is already running.".into());
        }
        let batch = ExportBatch {
            folder,
            sessions: sessions.into_iter().enumerate(),
            uploads: kinds[0],
            artifacts: kinds[1],
        };
        self.advance_export(batch, gateway).map(|_| ())
    }

    /// Advances only the acknowledged batch and returns its folder once, on completion.
    ///
    /// # Errors
    ///
    /// When the transfer failed or the gateway refuses the next request.
    pub fn exported(
        &mut self,
        gateway: Option<&Gateway>,
        request_id: &str,
        result: Result<(), String>,
    ) -> Result<Option<PathBuf>, String> {
        match mem::take(&mut self.export) {
            Export::Running {
                request_id: expected,
                batch,
            } if expected.as_ref() == request_id => {
                result?;
                self.advance_export(batch, gateway)
            }
            other => {
                self.export = other;
                Ok(None)
            }
        }
    }

    fn advance_export(
        &mut self,
        mut batch: ExportBatch,
        gateway: Option<&Gateway>,
    ) -> Result<Option<PathBuf>, String> {
        let Some((index, session)) = batch.sessions.next() else {
            self.export = Export::Saved;
            return Ok(Some(batch.folder));
        };
        let destination = batch.folder.join(format!("Chat-{}", index + 1));
        self.export = Export::Optional;
        let request_id = gateway
            .and_then(|gateway| {
                gateway
                    .export_storage(session, destination, batch.uploads, batch.artifacts)
                    .ok()
            })
            .ok_or("Export could not start. Try again before purging.")?;
        self.export = Export::Running { request_id, batch };
        Ok(None)
    }

    pub fn disconnected(&mut self) {
        self.request = None;
        self.pending.clear();
        self.before = None;
        self.export = Export::Optional;
    }

    /// Forgets a request that never reached the gateway.
    pub fn send_failed(&mut self, request_id: Option<&str>) {
        if self.request.as_deref() == request_id {
            self.request = None;
        }
        if let Some(request_id) = request_id {
            self.pending.remove(request_id);
        }
        self.before = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{Storage, StorageEvent};
    use mobius_gateway::wire::{ClientMessage, ServerMessage};

    #[test]
    fn a_finished_deletion_remeasures_and_a_throttled_measurement_retries() {
        let mut storage = Storage::default();
        storage.track("delete".into());
        assert!(storage.busy());
        let events = storage.receive(&ServerMessage::Accepted {
            request_id: "delete".into(),
        });
        assert_eq!(events, [StorageEvent::FilesRemoved, StorageEvent::Measure]);
        assert!(!storage.busy());

        let Some(ClientMessage::GetStorageUsage { request_id }) = storage.measure() else {
            panic!("a measurement request");
        };
        assert!(storage.measure().is_none(), "one measurement at a time");
        let events = storage.receive(&ServerMessage::Rejected {
            request_id,
            code: "rate_limited".into(),
            message: "slow down".into(),
            fatal: false,
        });
        assert_eq!(events, [StorageEvent::MeasureLater]);
        assert!(storage.measure().is_some());
    }
}
