//! Gateway storage: measuring it, the purges and deletions in flight, and the export that
//! must finish before a purge.

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

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

/// The next chat to export, where it goes and whether uploads and artifacts are included.
#[derive(Debug)]
pub struct ExportStep {
    pub session: SessionId,
    pub destination: PathBuf,
    pub uploads: bool,
    pub artifacts: bool,
}

#[derive(Debug, Default)]
pub struct Storage {
    pub usage: Option<StorageUsage>,
    /// The measurement in flight.
    pub request: Option<Arc<str>>,
    pending: BTreeSet<Arc<str>>,
    before: Option<u64>,
    export_request: Option<Arc<str>>,
    export_queue: VecDeque<(SessionId, PathBuf)>,
    export_kinds: [bool; 3],
    pub export_failed: bool,
    pub export_required: bool,
    pub export_complete: bool,
    export_folder: Option<PathBuf>,
}

impl Storage {
    /// A deletion or export is still running.
    pub fn busy(&self) -> bool {
        !self.pending.is_empty() || self.exporting()
    }

    pub fn exporting(&self) -> bool {
        self.export_request.is_some()
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
                self.usage = Some(usage.clone()); // rules-allow: every package's hook reads the same shared message
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

    /// Queues each chat's export into its own folder inside `folder`.
    pub fn begin_export(&mut self, folder: PathBuf, sessions: Vec<SessionId>, kinds: [bool; 3]) {
        self.export_required = true;
        self.export_complete = false;
        self.export_failed = false;
        self.export_kinds = kinds;
        self.export_queue = sessions
            .into_iter()
            .enumerate()
            .map(|(index, session)| (session, folder.join(format!("Chat-{}", index + 1))))
            .collect();
        self.export_folder = Some(folder);
    }

    pub fn next_export(&mut self) -> Option<ExportStep> {
        let (session, destination) = self.export_queue.pop_front()?;
        Some(ExportStep {
            session,
            destination,
            uploads: self.export_kinds[0],
            artifacts: self.export_kinds[1],
        })
    }

    /// Records the export request a step started, or that it could not start.
    pub fn export_started(&mut self, request_id: Option<Arc<str>>) {
        if request_id.is_none() {
            self.export_queue.clear();
            self.export_failed = true;
        }
        self.export_request = request_id;
    }

    /// Takes one chat's export result; `false` when `request_id` is not the running export.
    pub fn exported(&mut self, request_id: &str, failed: bool) -> bool {
        if self.export_request.as_deref() != Some(request_id) {
            return false;
        }
        self.export_request = None;
        self.export_failed |= failed;
        if self.export_failed {
            self.export_queue.clear();
        }
        true
    }

    /// The export folder once every chat is exported without a failure.
    pub fn finish_export(&mut self) -> Option<Option<PathBuf>> {
        if self.exporting() || self.export_failed {
            return None;
        }
        self.export_complete = true;
        Some(self.export_folder.take())
    }

    pub fn disconnected(&mut self) {
        self.request = None;
        self.pending.clear();
        self.before = None;
        self.export_queue.clear();
        self.export_request = None;
        self.export_failed = true;
        self.export_complete = false;
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
