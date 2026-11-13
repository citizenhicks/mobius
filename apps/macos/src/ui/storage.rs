//! Storage controls use the gateway's file ownership and deletion contract.
use super::navigation::{project_id, session_title, workspace_name};
use super::*;
use crate::gateway::storage::StorageEvent;
use gpui_component::button::ButtonVariants;
use gpui_component::{ActiveTheme, Disableable, Selectable};
use mobius::SessionId;
use mobius::backend::session_files::{SessionFileOrigin, SessionFileSelection};
use mobius_gateway::storage_usage::SessionStorageUsage;
use mobius_gateway::wire::{ClientMessage, ServerMessage};
use std::collections::BTreeSet;

#[derive(Default)]
pub(super) struct StorageUi {
    cache_bytes: Option<u64>,
    selected: BTreeSet<SessionId>,
    by_project: bool,
    kinds: [bool; 3],
}

#[derive(Default)]
struct StorageGroup {
    sessions: Vec<SessionId>,
    bytes: u64,
    files: u64,
}

fn included_size(row: &SessionStorageUsage, kinds: [bool; 3]) -> (u64, u64) {
    [row.uploads, row.artifacts, row.observations]
        .into_iter()
        .zip(kinds)
        .filter_map(|(size, included)| included.then_some(size))
        .fold((0_u64, 0_u64), |(bytes, files), size| {
            (
                bytes.saturating_add(size.bytes),
                files.saturating_add(size.files),
            )
        })
}

fn storage_groups(
    rows: &[SessionStorageUsage],
    by_project: bool,
    kinds: [bool; 3],
) -> Vec<(String, StorageGroup)> {
    let mut groups = BTreeMap::<&str, StorageGroup>::new();
    for row in rows {
        let (bytes, files) = included_size(row, kinds);
        if files == 0 {
            continue;
        }
        let key = if by_project {
            row.project_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .unwrap_or("No project")
        } else {
            &row.session_id
        };
        let group = groups.entry(key).or_default();
        group.sessions.push(row.session_id.share());
        group.bytes = group.bytes.saturating_add(bytes);
        group.files = group.files.saturating_add(files);
    }
    let mut groups = groups
        .into_iter()
        .map(|(id, group)| (id.to_owned(), group))
        .collect::<Vec<_>>();
    groups.sort_by_key(|(_, group)| std::cmp::Reverse(group.bytes));
    groups
}

impl Workspace {
    pub(in crate::ui) fn refresh_storage(&mut self, cx: &mut Context<Self>) {
        if !self.client.link.is_ready() {
            return;
        }
        if let Some(message) = self.client.storage.measure() {
            self.send(message, cx);
        }
    }

    pub(in crate::ui) fn storage_message(
        &mut self,
        message: &ServerMessage,
        cx: &mut Context<Self>,
    ) {
        for event in self.client.storage.receive(message) {
            match event {
                StorageEvent::Freed(bytes) => {
                    self.announce(Tone::Success, format!("Freed {}.", byte_count(bytes)));
                }
                StorageEvent::Full => self.notify_storage_full(),
                StorageEvent::FilesRemoved => {
                    self.runtime.thumbnails.clear_files();
                    self.load_chat_sources();
                    self.refresh_open_workspace_files(cx);
                }
                StorageEvent::Measure => self.refresh_storage(cx),
                StorageEvent::MeasureLater => {
                    let epoch = self.client.epoch;
                    cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_secs(5))
                            .await;
                        if let Err(error) = this.update(cx, |this, cx| {
                            if this.client.epoch == epoch {
                                this.refresh_storage(cx);
                            }
                        }) {
                            tracing::debug!(%error, "the workspace closed before storage was measured again");
                        }
                    })
                    .detach();
                }
            }
        }
    }

    pub(in crate::ui) fn storage_update(&mut self, update: &crate::gateway::Update) {
        if let crate::gateway::Update::Uploaded {
            endpoint,
            result: Err(failure),
            ..
        } = update
            && endpoint == self.client.endpoint.as_str()
            && failure.storage_full
        {
            self.notify_storage_full();
        }
    }

    fn notify_storage_full(&mut self) {
        self.shell.notices.push((
            Tone::Error,
            "Cloud storage is full. Free up space to upload files.".into(),
            Some(navigation::NoticeAction::Storage),
        ));
    }

    pub(in crate::ui) fn storage_group(&self) -> Group {
        let Some(usage) = &self.client.storage.usage else {
            return Group::new().child(
                Row::new("storage-refresh", "Storage")
                    .subtitle("Measuring…")
                    .placeholder(),
            );
        };
        let status = usage.limit_bytes.map_or_else(
            || format!("{} used", byte_count(usage.used_bytes)),
            |limit| format!("{} of {}", byte_count(usage.used_bytes), byte_count(limit)),
        );
        let owned: u64 = usage
            .sessions
            .iter()
            .map(|row| {
                row.uploads
                    .bytes
                    .saturating_add(row.artifacts.bytes)
                    .saturating_add(row.observations.bytes)
            })
            .fold(0, u64::saturating_add);
        Group::new()
            .child(Row::new("storage-total", "Storage").value(status))
            .child(Row::new("storage-chats", "Chats").value(format!(
                "{} · {}",
                usage.session_count,
                byte_count(owned)
            )))
            .child(Row::new("storage-projects", "Projects").value(format!(
                    "{} · {}",
                    usage.project_count,
                    byte_count(
                        usage
                            .projects
                            .iter()
                            .map(|p| p.size.bytes)
                            .fold(0, u64::saturating_add)
                    )
                )))
    }

    pub(in crate::ui) fn data_settings(&self, cx: &Context<Self>) -> Page {
        page("settings-data", PageHeader::new("Data"))
            .child(Section::new("Gateway storage").child(self.storage_group()))
            .child(Section::new("On this Mac").child(Group::new()
                .child(Row::new("storage-local-cache", "Cache")
                    .subtitle(self.storage.cache_bytes.map_or_else(|| "Measuring…".into(), |bytes| format!("{} of cached images", byte_count(bytes))))
                    .control(self.storage_delete_button("clear-local-cache", "Clear cached data", cx)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.confirm_destructive("Clear cached data?", "Removes downloaded image thumbnails from this Mac. Chats, drafts and gateways stay.", "Clear cache", window, cx, |this, _, _| {
                                this.runtime.thumbnails.clear_files();
                                this.storage.cache_bytes = Some(0);
                                this.announce(Tone::Success, "Cached data cleared.".into());
                            });
                        }))))
                .child(Row::new("storage-local-gateways", "Gateway information")
                    .subtitle(format!("{} saved · Cloud sign-in", counted(self.shell.saved_endpoints.len(), "gateway", "gateways")))
                    .control(self.storage_delete_button("clear-gateway-info", "Clear gateway information", cx)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.confirm_destructive("Clear gateway information?", "Forgets saved gateway connections and signs out of Cloud on this Mac. Gateway data and project files stay.", "Clear information", window, cx, |this, _, cx| {
                                let endpoints = this.shell.saved_endpoints.clone(); // rules-allow: forgetting each gateway edits the saved list
                                for endpoint in endpoints { this.forget_saved_gateway(&endpoint, cx); }
                                this.cloud(crate::host::platform::CloudAction::SignOut);
                            });
                        }))))))
            .child(button("storage-cleanup", "Clean up remote storage").primary().w_full()
                .h(height::LARGE).pill_corners()
                .icon(icon("hi.trash", &self.palette).text_color(self.palette.on_button_accent()))
                .disabled(!self.client.link.is_ready())
                .on_click(cx.listener(|this, _, window, cx| this.open_storage_cleanup(window, cx))))
    }

    fn storage_delete_button(
        &self,
        id: &'static str,
        label: &'static str,
        cx: &App,
    ) -> gpui_component::button::Button {
        pill(id)
            .glyph("hi.trash")
            .tint(self.palette.danger)
            .button(cx)
            .size(height::REGULAR)
            .p_0()
            .bg(self.palette.card.blend(self.palette.danger.opacity(0.15)))
            .tooltip(label)
            .accessibility_label(label)
    }

    pub(in crate::ui) fn measure_local_cache(&self, cx: &Context<Self>) {
        let paths = self.runtime.thumbnails.cache_paths();
        cx.spawn(async move |this, cx| {
            let result =
                cx.background_executor()
                    .spawn(async move {
                        crate::gateway::local::total_size(paths.iter().map(AsRef::as_ref))
                    })
                    .await;
            if let Err(error) = this.update(cx, |this, cx| {
                match result {
                    Ok(bytes) => this.storage.cache_bytes = Some(bytes),
                    Err(error) => this.report(format!("Could not measure cache: {error}")),
                }
                cx.notify();
            }) {
                tracing::debug!(%error, "the workspace closed before the cache was measured");
            }
        })
        .detach();
    }

    pub(in crate::ui) fn open_storage_cleanup(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.storage.selected.clear();
        self.storage.kinds = [true; 3];
        self.client.storage.export_required = false;
        self.client.storage.export_complete = false;
        self.client.storage.export_failed = false;
        self.refresh_storage(cx);
        let owner = cx.entity();
        let body = cx.new(|cx| {
            cx.observe(&owner, |_, _, cx| cx.notify()).detach();
            StorageDialog {
                workspace: owner.downgrade(),
                part: StorageDialogPart::Body,
            }
        });
        let header = cx.new(|cx| {
            cx.observe(&owner, |_, _, cx| cx.notify()).detach();
            StorageDialog {
                workspace: owner.downgrade(),
                part: StorageDialogPart::Header,
            }
        });
        let footer = cx.new(|cx| {
            cx.observe(&owner, |_, _, cx| cx.notify()).detach();
            StorageDialog {
                workspace: owner.downgrade(),
                part: StorageDialogPart::Actions,
            }
        });
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title(Entity::clone(&header))
                .close_button(false)
                .w(width::DIALOG_FORM)
                .child(Entity::clone(&body))
                .footer(Entity::clone(&footer))
        });
    }

    fn storage_header(&self, cx: &Context<Self>) -> Div {
        let busy = !self.client.link.is_ready() || self.client.storage.busy();
        div()
            .w_full()
            .flex()
            .items_center()
            .child(
                toolbar_button("storage-close", "Close", "hi.x", cx)
                    .disabled(self.client.storage.busy())
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(heading("Free up space").flex_1().text_center())
            .child(
                toolbar_button(
                    "storage-clear-selection",
                    "Clear selection",
                    "hi.filterMailRemove",
                    cx,
                )
                .disabled(busy || self.storage.selected.is_empty())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.storage.selected.clear();
                    this.client.storage.export_complete = false;
                    cx.notify();
                })),
            )
    }

    fn storage_sheet(&self, cx: &Context<Self>) -> Div {
        let busy = !self.client.link.is_ready() || self.client.storage.busy();
        let body = dialog_body().child(
            segmented(
                "storage-group",
                ["By chat", "By project"],
                Some(usize::from(self.storage.by_project)),
                cx.listener(|this, index: &usize, _, cx| {
                    if this.client.storage.busy() {
                        return;
                    }
                    this.storage.by_project = *index == 1;
                    this.storage.selected.clear();
                    this.client.storage.export_complete = false;
                    cx.notify();
                }),
            )
            .full_width(),
        );
        body.child(
            div()
                .flex()
                .flex_col()
                .gap(space::S)
                .child(
                    div()
                        .typography(Type::Caption)
                        .text_color(self.palette.muted)
                        .child("Select what to purge"),
                )
                .child(self.storage_filters(busy, cx)),
        )
        .child(self.storage_rows(busy, cx))
    }

    fn storage_filters(&self, busy: bool, cx: &Context<Self>) -> Div {
        button_row().children(
            [
                ("Uploads", "hi.upload01"),
                ("Artifacts", "hi.download01"),
                ("Screenshots", "hi.imageCrop"),
            ]
            .into_iter()
            .zip(self.storage.kinds)
            .enumerate()
            .map(|(index, ((label, glyph), selected))| {
                pill(("storage-kind", index))
                    .label(label)
                    .glyph(glyph)
                    .tint(if selected {
                        self.palette.accent
                    } else {
                        self.palette.text
                    })
                    .button(cx)
                    .flex_1()
                    .selected(selected)
                    .when(selected, |button| {
                        button
                            .bg(self.palette.accent_soft)
                            .border_color(self.palette.accent)
                    })
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(kind) = this.storage.kinds.get_mut(index) {
                            *kind = !*kind;
                        }
                        let kinds = this.storage.kinds;
                        let available = this.client.storage.usage.as_ref().map_or_else(
                            BTreeSet::new,
                            |usage| {
                                usage
                                    .sessions
                                    .iter()
                                    .filter(|row| included_size(row, kinds).1 > 0)
                                    .map(|row| row.session_id.as_ref())
                                    .collect()
                            },
                        );
                        this.storage.selected.retain(|id| available.contains(&**id));
                        this.client.storage.export_complete = false;
                        cx.notify();
                    }))
            }),
        )
    }

    fn storage_footer(&self, cx: &Context<Self>) -> Div {
        let busy = !self.client.link.is_ready() || self.client.storage.busy();
        let enabled = !busy
            && !self.storage.selected.is_empty()
            && self.storage.kinds.iter().any(|v| *v)
            && (!self.client.storage.export_required
                || self.client.storage.export_complete
                || self.client.storage.export_failed);
        button_row()
            .child(
                button(
                    "storage-export",
                    if self.client.storage.exporting() {
                        "Exporting…"
                    } else if self.client.storage.export_complete {
                        "Export saved"
                    } else {
                        "Export first"
                    },
                )
                .icon(
                    icon(
                        if self.client.storage.export_complete {
                            "hi.checkCircle"
                        } else {
                            "hi.download01"
                        },
                        &self.palette,
                    )
                    .text_color(if self.client.storage.export_complete {
                        self.palette.signal
                    } else {
                        self.palette.icon_ink()
                    }),
                )
                .h(height::LARGE)
                .pill_corners()
                .when(self.client.storage.export_complete, |button| {
                    button.text_color(self.palette.signal)
                })
                .flex_1()
                .disabled(
                    busy || self.storage.selected.is_empty()
                        || (!self.storage.kinds[0] && !self.storage.kinds[1]),
                )
                .on_click(cx.listener(|this, _, window, cx| this.export_storage(window, cx))),
            )
            .child(
                button("storage-purge", "Purge")
                    .danger()
                    .icon(
                        icon("hi.trash", &self.palette)
                            .text_color(cx.theme().button_danger_foreground),
                    )
                    .h(height::LARGE)
                    .pill_corners()
                    .flex_1()
                    .disabled(!enabled)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.confirm_purge_storage(window, cx)),
                    ),
            )
    }

    fn storage_rows(&self, busy: bool, cx: &Context<Self>) -> impl IntoElement {
        let groups = storage_groups(
            self.client
                .storage
                .usage
                .as_ref()
                .map_or(&[], |usage| usage.sessions.as_slice()),
            self.storage.by_project,
            self.storage.kinds,
        );
        let mut rows = div()
            .id("storage-rows")
            .max_h(height::DIALOG_LIST_MAX)
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(space::S);
        for (key, group) in groups {
            let selected = group
                .sessions
                .iter()
                .all(|id| self.storage.selected.contains(id));
            let id = SharedString::from(key);
            let title: SharedString = if self.storage.by_project {
                self.client
                    .state
                    .sessions
                    .iter()
                    .find(|session| project_id(session) == Some(id.as_ref()))
                    .and_then(|session| session.session_context.workspace_label.as_deref())
                    .map_or_else(
                        || SharedString::clone(&id),
                        |path| workspace_name(path).to_owned().into(),
                    )
            } else {
                self.client.state.session_record(&id).map_or_else(
                    || SharedString::clone(&id),
                    |session| session_title(session).into(),
                )
            };
            rows = rows.child(
                Row::new(id, title)
                    .subtitle(counted(group.files, "file", "files"))
                    .value(byte_count(group.bytes))
                    .leading(Leading::Selection(selected))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if busy {
                            return;
                        }
                        for id in &group.sessions {
                            if selected {
                                this.storage.selected.remove(id);
                            } else {
                                this.storage.selected.insert(id.share());
                            }
                        }
                        this.client.storage.export_complete = false;
                        cx.notify();
                    })),
            );
        }
        rows
    }

    fn confirm_purge_storage(&self, window: &mut Window, cx: &mut Context<Self>) {
        let ids = self
            .storage
            .selected
            .iter()
            .map(SessionId::share)
            .collect::<Vec<_>>();
        let origins = [
            SessionFileOrigin::Upload,
            SessionFileOrigin::Artifact,
            SessionFileOrigin::Observation,
        ]
        .into_iter()
        .zip(self.storage.kinds)
        .filter_map(|(kind, selected)| selected.then_some(kind))
        .collect::<Vec<_>>();
        let (count, size) = self.client.storage.usage.as_ref().map_or_default(|usage| {
            usage
                .sessions
                .iter()
                .filter(|s| ids.contains(&s.session_id))
                .fold((0_u64, 0_u64), |mut total, row| {
                    for (size, selected) in [&row.uploads, &row.artifacts, &row.observations]
                        .into_iter()
                        .zip(self.storage.kinds)
                    {
                        if selected {
                            total.0 = total.0.saturating_add(size.files);
                            total.1 = total.1.saturating_add(size.bytes);
                        }
                    }
                    total
                })
        });
        self.confirm_destructive(
            format!(
                "Purge {} ({})?",
                counted(count, "file", "files"),
                byte_count(size)
            ),
            "This cannot be undone. Chat history and project files stay. Shared content may reduce the space freed.",
            "Purge",
            window,
            cx,
            move |this, _, cx| {
                for message in this.client.storage.purge(&ids, &origins) {
                    this.send(message, cx);
                }
            },
        );
    }

    fn export_storage(&self, window: &Window, cx: &Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Export files here".into()),
        });
        let epoch = self.client.epoch;
        let ids = self
            .storage
            .selected
            .iter()
            .map(SessionId::share)
            .collect::<Vec<_>>();
        let kinds = self.storage.kinds;
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(folder) = paths.first()
            {
                let folder = folder.join(format!("Mobius-export-{}", uuid::Uuid::new_v4()));
                if let Err(error) = this.update(cx, |this, cx| {
                    if this.client.epoch != epoch {
                        return;
                    }
                    this.storage.selected = ids.iter().map(SessionId::share).collect();
                    this.storage.kinds = kinds;
                    this.client.storage.begin_export(folder, ids, kinds);
                    this.start_storage_export();
                    cx.notify();
                }) {
                    tracing::debug!(%error, "the workspace closed before the export started");
                }
            }
        })
        .detach();
    }

    pub(in crate::ui) fn storage_exported(
        &mut self,
        id: &str,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        if !self.client.storage.exported(id, result.is_err()) {
            return;
        }
        if let Err(error) = result {
            self.report(error);
        }
        self.start_storage_export();
        if let Some(folder) = self.client.storage.finish_export() {
            self.shell.notices.push((
                Tone::Success,
                "Files exported. Click to show them in Finder.".into(),
                folder.map(navigation::NoticeAction::Reveal),
            ));
        }
        cx.notify();
    }

    fn start_storage_export(&mut self) {
        let Some(step) = self.client.storage.next_export() else {
            return;
        };
        let started = self.client.gateway.as_ref().and_then(|gateway| {
            gateway
                .export_storage(step.session, step.destination, step.uploads, step.artifacts)
                .ok()
        });
        if started.is_none() {
            self.report("Export could not start. Try again before purging.");
        }
        self.client.storage.export_started(started);
    }

    pub(in crate::ui) fn confirm_delete_file(
        &self,
        mention: chat::Mention,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self
            .client
            .state
            .selected_session_id
            .as_ref()
            .map(SessionId::share)
        else {
            return;
        };
        let title = match &mention {
            chat::Mention::ChatFile(file) => format!("Delete “{}”?", file.name),
            chat::Mention::Path(path) => format!("Delete “{path}”?"),
            _ => return,
        };
        self.confirm_destructive(
            title,
            "This permanently removes the file. Chat history stays.",
            "Delete",
            window,
            cx,
            move |this, _, cx| {
                let id = crate::gateway::request_id();
                let message = match &mention {
                    chat::Mention::ChatFile(file) => ClientMessage::DeleteSessions {
                        request_id: Arc::clone(&id),
                        session_ids: vec![session.share()],
                        selection: SessionFileSelection::Ids(vec![file.id.clone()]), // rules-allow: the confirm callback is Fn, so each request owns its copy
                    },
                    chat::Mention::Path(path) => ClientMessage::DeleteWorkspaceFile {
                        request_id: Arc::clone(&id),
                        session_id: session.share(),
                        path: path.clone(), // rules-allow: the confirm callback is Fn, so each request owns its copy
                    },
                    _ => return,
                };
                this.client.storage.track(id);
                this.send(message, cx);
            },
        );
    }
}

// The dialog layer is built during Workspace::render. Render its live controls in
// child entities, after that borrow ends, so gateway updates can refresh the sheet.
enum StorageDialogPart {
    Header,
    Body,
    Actions,
}

struct StorageDialog {
    workspace: WeakEntity<Workspace>,
    part: StorageDialogPart,
}

impl Render for StorageDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.workspace
            .update(cx, |this, cx| match self.part {
                StorageDialogPart::Header => this.storage_header(cx),
                StorageDialogPart::Body => this.storage_sheet(cx),
                StorageDialogPart::Actions => this.storage_footer(cx),
            })
            .unwrap_or_else(|_| div())
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionStorageUsage, storage_groups};
    use mobius_gateway::storage_usage::StorageSize;

    fn row(
        id: &str,
        project: &str,
        uploads: (u64, u64),
        artifacts: (u64, u64),
    ) -> SessionStorageUsage {
        let size = |(bytes, files)| StorageSize {
            bytes,
            files,
            complete: true,
        };
        SessionStorageUsage {
            session_id: id.parse().unwrap(),
            project_id: Some(project.to_owned()),
            uploads: size(uploads),
            artifacts: size(artifacts),
            observations: size((0, 0)),
        }
    }

    #[test]
    fn cleanup_projects_aggregate_only_the_included_files_before_ranking() {
        let rows = [
            row("first", "combined", (200, 1), (9_000, 4)),
            row("second", "combined", (300, 2), (0, 0)),
            row("third", "other", (400, 1), (0, 0)),
        ];
        let groups = storage_groups(&rows, true, [true, false, false]);
        assert_eq!(groups[0].0, "combined");
        assert_eq!((groups[0].1.bytes, groups[0].1.files), (500, 3));
        assert_eq!(
            groups[0].1.sessions,
            [rows[0].session_id.share(), rows[1].session_id.share()]
        );
        assert_eq!(groups[1].0, "other");
    }
}
