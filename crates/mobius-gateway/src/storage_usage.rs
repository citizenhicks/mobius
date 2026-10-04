//! Bounded storage measurements shared by paired clients and telemetry collectors.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// File counts and logical bytes; not an estimate of reclaimable disk space.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageSize {
    /// Sum of regular file lengths and symlink metadata lengths; targets are not followed.
    pub bytes: u64,
    /// Number of regular files and symlink entries.
    pub files: u64,
    /// False when a limit or inaccessible entry prevented measurement, or a symlink
    /// appeared within or replaced the charged blob directory.
    pub complete: bool,
}
/// A stable storage ownership category.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageCategory {
    /// Stable category ID, never a path.
    pub id: String,
    /// Measured size.
    pub size: StorageSize,
}
/// Logical files eligible for selective cleanup while preserving chat history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStorageUsage {
    /// Chat ID for navigation and later cleanup.
    pub session_id: String,
    /// Optional owning project ID.
    pub project_id: Option<String>,
    /// User uploads, including shared content once per reference.
    pub uploads: StorageSize,
    /// Agent artifacts eligible for cleanup, including shared content once per reference.
    pub artifacts: StorageSize,
    /// Hidden agent observations and fork grants.
    pub observations: StorageSize,
}
/// A project measurement; project totals can overlap gateway storage or other projects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectStorageUsage {
    /// Stable project ID.
    pub project_id: String,
    /// Files under the registered project directory.
    pub size: StorageSize,
}
/// Comprehensive usage report. Logical references and project totals are not additive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageUsage {
    /// Schema version independent of the gateway wire version.
    pub version: u32,
    /// Measurement time in Unix seconds.
    pub measured_at: i64,
    /// Non-overlapping categories within the gateway state directory.
    pub categories: Vec<StorageCategory>,
    /// Per-chat logical ownership.
    pub sessions: Vec<SessionStorageUsage>,
    /// Total chats measured before the telemetry transport bounds its detail rows.
    pub session_count: usize,
    /// Distinct registered projects.
    pub projects: Vec<ProjectStorageUsage>,
    /// Total registered project directories measured.
    pub project_count: usize,
    /// Detail rows were omitted by the measurement budget or telemetry transport limit.
    pub details_truncated: bool,
    /// Aggregate bytes and file counts of gateway categories only.
    /// Completeness also covers project discovery and measurement for upload admission.
    pub gateway_total: StorageSize,
    /// Maximum content blob and project file bytes; absent for self-hosted gateways.
    pub limit_bytes: Option<u64>,
    /// Content blob and project file bytes charged against the allowance.
    /// Nested project paths are charged once; separate file copies count separately.
    pub used_bytes: u64,
}
impl StorageUsage {
    pub(crate) fn bound_telemetry_details(&mut self) {
        self.sessions.sort_by_key(|session| {
            std::cmp::Reverse(
                session
                    .uploads
                    .bytes
                    .saturating_add(session.artifacts.bytes)
                    .saturating_add(session.observations.bytes),
            )
        });
        self.projects
            .sort_by_key(|project| std::cmp::Reverse(project.size.bytes));
        self.details_truncated |= self.sessions.len() > 64 || self.projects.len() > 64;
        self.sessions.truncate(64);
        self.projects.truncate(64);
    }
}

pub(crate) struct MeasurementBudget {
    started: std::time::Instant,
    entries: usize,
}
impl MeasurementBudget {
    pub(crate) fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            entries: 0,
        }
    }
    pub(crate) fn deadline(&self) -> tokio::time::Instant {
        tokio::time::Instant::from_std(self.started) + std::time::Duration::from_secs(2)
    }
    async fn wait<F: std::future::Future>(&self, future: F) -> Option<F::Output> {
        if self.exhausted() {
            return None;
        }
        tokio::time::timeout_at(self.deadline(), future).await.ok()
    }
    fn exhausted(&self) -> bool {
        self.entries >= 100_000 || self.started.elapsed().as_secs() >= 2
    }
}

fn measure(path: &Path, budget: &mut MeasurementBudget) -> StorageSize {
    measure_content(path, budget, None).0
}

fn measure_content(
    path: &Path,
    budget: &mut MeasurementBudget,
    blobs: Option<&Path>,
) -> (StorageSize, u64) {
    measure_paths(vec![(path.to_path_buf(), 0)], budget, blobs)
}

fn measure_paths(
    mut pending: Vec<(PathBuf, u32)>,
    budget: &mut MeasurementBudget,
    blobs: Option<&Path>,
) -> (StorageSize, u64) {
    let mut used_bytes = 0_u64;
    let mut size = StorageSize {
        complete: true,
        ..Default::default()
    };
    while let Some((path, depth)) = pending.pop() {
        budget.entries += 1;
        if budget.exhausted() {
            size.complete = false;
            break;
        }
        if depth > 64 {
            size.complete = false;
            continue;
        }
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && depth == 0 => continue,
            Err(_) => {
                size.complete = false;
                continue;
            }
        };
        if metadata.is_symlink() {
            size.bytes = size.bytes.saturating_add(metadata.len());
            size.files = size.files.saturating_add(1);
            // Browser profiles legitimately contain runtime links. Blob storage must remain
            // regular files: a linked blob or ancestor makes the charged total untrustworthy.
            if blobs.is_some_and(|blobs| path.starts_with(blobs) || blobs.starts_with(&path)) {
                size.complete = false;
            }
            continue;
        }
        if metadata.is_file() {
            if blobs.is_some_and(|blobs| path.parent() == Some(blobs)) {
                used_bytes = used_bytes.saturating_add(metadata.len());
            }
            size.bytes = size.bytes.saturating_add(metadata.len());
            size.files = size.files.saturating_add(1);
        }
        if metadata.is_dir() {
            match std::fs::read_dir(path) {
                Ok(children) => {
                    for child in children {
                        if budget.exhausted() || pending.len() >= 100_000 {
                            size.complete = false;
                            break;
                        }
                        match child {
                            Ok(child) if pending.len() < 100_000 => {
                                pending.push((child.path(), depth + 1))
                            }
                            _ => size.complete = false,
                        }
                    }
                }
                Err(_) => size.complete = false,
            }
        }
    }
    (size, used_bytes)
}

fn gateway_categories(root: &Path, budget: &mut MeasurementBudget) -> (Vec<StorageCategory>, u64) {
    let mut used_bytes = 0_u64;
    let mut categories = std::collections::BTreeMap::<String, StorageSize>::new();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => {
            return (
                vec![StorageCategory {
                    id: "other".into(),
                    size: StorageSize::default(),
                }],
                0,
            );
        }
    };
    for entry in entries {
        if budget.exhausted() {
            categories.entry("other".into()).or_default().complete = false;
            break;
        }
        let Ok(entry) = entry else {
            categories.entry("other".into()).or_default().complete = false;
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let id = match name.as_ref() {
            "session-files" => "session_files",
            name if name.starts_with("checkpoints.sqlite3") => "session_history",
            name if name.starts_with("bots.sqlite3") => "bots_and_routines",
            _ => "other",
        };
        let blobs = root.join("session-files/blobs");
        let (measured, charged) = measure_content(&entry.path(), budget, Some(&blobs));
        used_bytes = used_bytes.saturating_add(charged);
        let size = categories.entry(id.into()).or_insert_with(|| StorageSize {
            complete: true,
            ..Default::default()
        });
        size.bytes = size.bytes.saturating_add(measured.bytes);
        size.files = size.files.saturating_add(measured.files);
        size.complete &= measured.complete;
    }
    (
        categories
            .into_iter()
            .map(|(id, size)| StorageCategory { id, size })
            .collect(),
        used_bytes,
    )
}

fn project_sizes(
    mut projects: Vec<(String, PathBuf)>,
    budget: &mut MeasurementBudget,
) -> (Vec<ProjectStorageUsage>, u64) {
    // Paths sort parents before their children, irrespective of project ID order.
    projects.sort_by(|(_, left), (_, right)| left.cmp(right));
    let mut charged_root = None::<PathBuf>;
    let mut used_bytes = 0_u64;
    let projects = projects
        .into_iter()
        .map(|(project_id, path)| {
            let size = measure(&path, budget);
            if charged_root
                .as_ref()
                .is_none_or(|root| !path.starts_with(root))
            {
                used_bytes = used_bytes.saturating_add(size.bytes);
                charged_root = Some(path);
            }
            ProjectStorageUsage { project_id, size }
        })
        .collect();
    (projects, used_bytes)
}

pub(crate) async fn measure_usage(
    root: PathBuf,
    checkpoints: std::sync::Arc<dyn mobius::backend::checkpoint::CheckpointStore>,
    files: mobius::backend::session_files::SessionFileStore,
    bots: std::sync::Arc<crate::bots::BotStore>,
    limit_bytes: Option<u64>,
    tls: Option<crate::config::TlsConfig>,
    mut budget: MeasurementBudget,
) -> crate::Result<StorageUsage> {
    use mobius::backend::checkpoint::SessionPageRequest;
    use mobius::backend::session_files::SessionFileOrigin::{Artifact, Observation, Upload};
    let mut sessions = Vec::new();
    let mut projects = std::collections::BTreeMap::new();
    let mut cursor = None;
    let mut complete = true;
    'pages: loop {
        let Some(page) = budget
            .wait(checkpoints.list_sessions_page(SessionPageRequest {
                owner_id: None,
                cursor,
                limit: 128,
            }))
            .await
        else {
            complete = false;
            break;
        };
        let page = page?;
        for summary in page.sessions {
            if budget.exhausted() {
                complete = false;
                break 'pages;
            }
            budget.entries += 1;
            let project_id = summary.session_context.workspace_id;
            if let Some(id) = &project_id
                && !projects.contains_key(id)
            {
                let Some(metadata) = budget
                    .wait(checkpoints.session_metadata(&summary.session_id))
                    .await
                else {
                    complete = false;
                    break 'pages;
                };
                if let Some(metadata) = metadata? {
                    let bots = std::sync::Arc::clone(&bots);
                    let root = root.clone();
                    let tls = tls.clone();
                    let spec = tokio::task::spawn_blocking(move || {
                        crate::config::ChatSpec::from_metadata_if_present(
                            &metadata,
                            &bots,
                            &root,
                            tls.as_ref(),
                        )
                    });
                    let Some(spec) = budget.wait(spec).await else {
                        complete = false;
                        break 'pages;
                    };
                    match spec.map_err(|error| {
                        crate::Error::Config(format!("storage metadata task failed: {error}"))
                    })? {
                        Ok(Some(spec)) => {
                            if let Some(path) = spec.workspace {
                                projects.insert(id.clone(), path);
                            }
                        }
                        Ok(None) => {}
                        Err(_) => complete = false,
                    }
                }
            }
            let mut uploads = StorageSize {
                complete: true,
                ..Default::default()
            };
            let mut artifacts = uploads;
            let mut observations = uploads;
            let records = budget
                .wait(
                    files.list_cleanup_files(&summary.session_id, &[Upload, Artifact, Observation]),
                )
                .await;
            match records {
                Some(Ok(records)) => {
                    budget.entries += records.len();
                    for (origin, file) in records {
                        let size = match origin {
                            Upload => &mut uploads,
                            Artifact => &mut artifacts,
                            Observation => &mut observations,
                        };
                        size.bytes = size.bytes.saturating_add(file.size);
                        size.files += 1;
                    }
                }
                _ => {
                    complete = false;
                    uploads.complete = false;
                    artifacts.complete = false;
                    observations.complete = false;
                }
            }
            sessions.push(SessionStorageUsage {
                session_id: summary.session_id,
                project_id,
                uploads,
                artifacts,
                observations,
            });
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    let deadline = budget.deadline();
    let measurement = tokio::task::spawn_blocking(move || {
        let (categories, used_bytes) = gateway_categories(&root, &mut budget);
        let (projects, project_bytes) = project_sizes(projects.into_iter().collect(), &mut budget);
        (
            categories,
            projects,
            used_bytes.saturating_add(project_bytes),
        )
    });
    let (categories, projects, used_bytes) = tokio::time::timeout_at(deadline, measurement)
        .await
        .map_err(|_| crate::Error::Config("storage measurement exceeded its time budget".into()))?
        .map_err(|error| crate::Error::Config(format!("storage measurement failed: {error}")))?;
    let mut gateway_total = StorageSize {
        complete: complete && projects.iter().all(|project| project.size.complete),
        ..Default::default()
    };
    for category in &categories {
        gateway_total.bytes = gateway_total.bytes.saturating_add(category.size.bytes);
        gateway_total.files = gateway_total.files.saturating_add(category.size.files);
        gateway_total.complete &= category.size.complete;
    }
    Ok(StorageUsage {
        version: 1,
        measured_at: chrono::Utc::now().timestamp(),
        session_count: sessions.len(),
        project_count: projects.len(),
        details_truncated: !complete,
        categories,
        sessions,
        projects,
        gateway_total,
        limit_bytes,
        used_bytes,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn measures_regular_files_without_following_symlinks() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), b"data").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.path(), root.path().join("loop")).unwrap();
        let size = super::measure(root.path(), &mut super::MeasurementBudget::new());
        #[cfg(unix)]
        assert_eq!(
            (size.bytes, size.files),
            (
                4 + std::fs::symlink_metadata(root.path().join("loop"))
                    .unwrap()
                    .len(),
                2
            )
        );
        #[cfg(not(unix))]
        assert_eq!((size.bytes, size.files), (4, 1));
        assert!(size.complete);
    }
    #[test]
    fn depth_limit_skips_only_the_deep_subtree() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file");
        std::fs::write(&file, b"data").unwrap();
        let (size, _) = super::measure_paths(
            vec![(file, 1), (root.path().join("deep"), 65)],
            &mut super::MeasurementBudget::new(),
            None,
        );
        assert_eq!((size.bytes, size.files, size.complete), (4, 1, false));
    }

    #[test]
    fn charged_blobs_are_counted_during_the_category_walk() {
        let root = tempfile::tempdir().unwrap();
        let blobs = root.path().join("session-files/blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(blobs.join("blob"), b"payload").unwrap();
        std::fs::write(root.path().join("session-files/metadata"), b"meta").unwrap();
        let (categories, used) =
            super::gateway_categories(root.path(), &mut super::MeasurementBudget::new());
        assert_eq!(used, 7);
        assert_eq!(categories[0].size.bytes, 11);
    }

    #[test]
    fn quota_includes_project_files_and_copies_but_counts_nested_paths_once() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let blobs = state.join("session-files/blobs");
        let workspace = root.path().join("work");
        let nested = workspace.join("nested");
        let attachments = workspace.join(".mobius/attachments");
        let sibling = root.path().join("work-other");
        for path in [&blobs, &nested, &attachments, &sibling] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(blobs.join("blob"), b"payload").unwrap();
        std::fs::write(state.join("session-files/metadata"), b"meta").unwrap();
        std::fs::write(workspace.join("file"), b"payload").unwrap();
        std::fs::write(attachments.join("copy"), b"payload").unwrap();
        std::fs::write(nested.join("file"), b"data").unwrap();
        std::fs::write(sibling.join("file"), b"other").unwrap();
        let mut budget = super::MeasurementBudget::new();
        let (_, blob_bytes) = super::gateway_categories(&state, &mut budget);
        let (projects, project_bytes) = super::project_sizes(
            vec![
                ("nested".into(), nested),
                ("parent".into(), workspace.clone()),
                ("same-path".into(), workspace),
                ("sibling".into(), sibling),
            ],
            &mut budget,
        );
        assert_eq!(blob_bytes + project_bytes, 30);
        assert!(projects.iter().all(|project| project.size.complete));
        let sizes: std::collections::BTreeMap<_, _> = projects
            .into_iter()
            .map(|project| (project.project_id, (project.size.bytes, project.size.files)))
            .collect();
        assert_eq!(sizes["parent"], (18, 3));
        assert_eq!(sizes["same-path"], (18, 3));
        assert_eq!(sizes["nested"], (4, 1));
        assert_eq!(sizes["sibling"], (5, 1));
    }

    #[cfg(unix)]
    #[test]
    fn browser_profile_symlinks_preserve_complete_usage_without_charging_targets() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("unrelated"), b"outside data").unwrap();
        let profile = root.path().join("desktop/profile");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join("Preferences"), b"preferences").unwrap();
        let link = profile.join("SingletonSocket");
        std::os::unix::fs::symlink(outside.path().join("missing-runtime-socket"), &link).unwrap();
        let (categories, used) =
            super::gateway_categories(root.path(), &mut super::MeasurementBudget::new());
        assert_eq!(used, 0);
        assert!(categories.iter().all(|category| category.size.complete));
        assert_eq!(categories[0].size.files, 2);
        assert_eq!(
            categories[0].size.bytes,
            11 + std::fs::symlink_metadata(link).unwrap().len()
        );
    }

    #[cfg(unix)]
    #[test]
    fn charged_blob_symlinks_and_linked_blob_roots_make_usage_incomplete() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("blob"), b"outside payload").unwrap();
        let blobs = root.path().join("session-files/blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(blobs.join("regular"), b"data").unwrap();
        std::os::unix::fs::symlink(outside.path().join("blob"), blobs.join("linked")).unwrap();
        let (categories, used) =
            super::gateway_categories(root.path(), &mut super::MeasurementBudget::new());
        assert_eq!(used, 4);
        assert!(!categories[0].size.complete);
        std::fs::remove_dir_all(&blobs).unwrap();
        std::os::unix::fs::symlink(outside.path(), &blobs).unwrap();
        let (categories, used) =
            super::gateway_categories(root.path(), &mut super::MeasurementBudget::new());
        assert_eq!(used, 0);
        assert!(!categories[0].size.complete);
    }

    #[tokio::test]
    async fn expired_measurement_budget_skips_async_reads() {
        let budget = super::MeasurementBudget {
            started: std::time::Instant::now() - std::time::Duration::from_secs(3),
            entries: 0,
        };
        assert!(
            budget
                .wait(async { panic!("expired operation was polled") })
                .await
                .is_none()
        );
    }
}
