//! Gateway-loaded documents: requests, results and the workspace file list.

use super::code::{LocalPreview, Origin, file_title, read_local_preview, text_preview};
use super::file_tree::FileTree;
use super::*;
use crate::gateway::local::FileRoute;
use crate::gateway::{self, PreviewData, PreviewRequest};
use crate::numeric::ToFloat;
use gpui_component::list::ListItem;
use gpui_component::menu::ContextMenuExt;
use gpui_component::tree::{TreeEntry, TreeEvent, TreeState, tree};
use mobius::SessionId;
use mobius_gateway::wire::{GitDiffScope, WorkspaceFileRecord};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

const FILES_TITLE: &str = "Workspace files";
/// The tree rail: narrow beside the file, resizable within these bounds.
const RAIL: Pixels = px(220.);
const RAIL_MIN: Pixels = px(140.);
const RAIL_MAX: Pixels = px(400.);
const VIEWER_MIN: Pixels = px(160.);

/// The Files tab: the workspace tree in a rail beside the file selected in it.
#[derive(Clone)]
pub(in crate::ui) struct FilesDoc {
    /// None for a tree no chat owns: a local file's or a diff's.
    session_id: Option<SessionId>,
    local_root: Option<Arc<Path>>,
    source: Rc<RefCell<TreeSource>>,
    tree: Entity<TreeState>,
    truncated: bool,
    selected: Option<Selection>,
    /// The tree rail is shown; hiding it gives the file the whole tab.
    rail: bool,
    rail_motion: motion::Reveal,
    /// A refresh of the tree in flight.
    refreshing: Option<Arc<str>>,
    resize: Entity<ResizableState>,
}

/// The lazy path model behind the tree; changes made without an `App` wait for the
/// next render to reach the tree state.
struct TreeSource {
    files: FileTree,
    expanded: BTreeSet<String>,
    stale: bool,
    /// A file to select and scroll to on the next rebuild.
    reveal: Option<String>,
}

impl TreeSource {
    fn rebuild(&mut self, state: &mut TreeState, cx: &mut Context<TreeState>) {
        let (selected, scroll) = match self.reveal.take() {
            Some(path) => (Some(path), true),
            None => (state.selected_item().map(|item| item.id.to_string()), false),
        };
        state.set_items(self.files.items(&self.expanded), cx);
        if let Some(index) = selected.and_then(|path| state.index_of(&path.into())) {
            state.set_selected_index(Some(index), cx);
            if scroll {
                state.scroll_to_item(index, ScrollStrategy::Nearest);
            }
        }
        self.stale = false;
    }
}

#[derive(Clone)]
struct Selection {
    path: String,
    view: FileView,
}

/// What the selected file shows as: the same viewers a file tab uses.
#[derive(Clone)]
enum FileView {
    Loading(Arc<str>),
    Failed(SharedString),
    Code(CodeDoc),
    File(FileDoc),
}

impl FileView {
    fn from_content(content: TabContent) -> Self {
        match content {
            TabContent::Code(doc) => Self::Code(doc),
            TabContent::File(path) => Self::File(path),
            TabContent::Failed(message) => Self::Failed(message.into()),
            _ => Self::Failed("This file can’t be shown here.".into()),
        }
    }
}

impl FilesDoc {
    pub(super) fn new<T: 'static>(
        session_id: Option<SessionId>,
        files: Vec<WorkspaceFileRecord>,
        truncated: bool,
        cx: &mut Context<T>,
    ) -> Self {
        let source = Rc::new(RefCell::new(TreeSource {
            files: FileTree::new(files.into_iter().map(|file| file.path)),
            expanded: BTreeSet::new(),
            stale: true,
            reveal: None,
        }));
        let tree = cx.new(|cx| {
            let source = Rc::clone(&source);
            cx.subscribe_self(move |state: &mut TreeState, event: &TreeEvent, cx| {
                let mut source = source.borrow_mut();
                match event {
                    TreeEvent::Expanded(path) => {
                        source.expanded.insert(path.to_string());
                        source.rebuild(state, cx);
                    }
                    TreeEvent::Collapsed(path) => {
                        source.expanded.remove(path.as_str());
                    }
                }
            })
            .detach();
            TreeState::new(cx)
        });
        let resize = cx.new(|_| ResizableState::default());
        // A caller-owned split redraws its drag only through an observer.
        cx.observe(&resize, |_, _, cx| cx.notify()).detach();
        Self {
            session_id,
            local_root: None,
            source,
            tree,
            truncated,
            selected: None,
            rail: false,
            rail_motion: motion::Reveal::new(false),
            refreshing: None,
            resize,
        }
    }

    /// Applies path-model changes to the tree before it draws.
    fn sync_tree(&self, cx: &mut App) {
        let mut source = self.source.borrow_mut();
        if source.stale {
            self.tree.update(cx, |state, cx| source.rebuild(state, cx));
        }
    }

    fn collapse_all(&self) {
        let mut source = self.source.borrow_mut();
        source.expanded.clear();
        source.stale = true;
    }

    fn track_motion(&mut self, now: Instant, still: bool) -> bool {
        if still {
            self.rail_motion.set(self.rail, true, now);
            return false;
        }
        self.rail_motion.moving(now)
    }

    fn rail_width(&self, cx: &App) -> Pixels {
        self.resize
            .read(cx)
            .sizes()
            .get(1)
            .copied()
            .unwrap_or(RAIL)
            .clamp(RAIL_MIN, RAIL_MAX)
    }

    /// A new listing of the same workspace, keeping open folders and the selection.
    fn replace_tree(&mut self, files: Vec<WorkspaceFileRecord>, truncated: bool) {
        let mut source = self.source.borrow_mut();
        source.files = FileTree::new(files.into_iter().map(|file| file.path));
        source.stale = true;
        self.truncated = truncated;
    }

    /// Opens every folder above `path`, then selects and scrolls to its row once the
    /// tree draws; nothing happens when the tree does not list it.
    pub(super) fn reveal(&self, path: &str) {
        let mut source = self.source.borrow_mut();
        if !source.files.contains(path) {
            return;
        }
        let mut folder = path;
        while let Some((parent, _)) = folder.rsplit_once('/') {
            source.expanded.insert(parent.to_owned());
            folder = parent;
        }
        source.reveal = Some(path.to_owned());
        source.stale = true;
    }

    pub(super) fn rail_shown(&self) -> bool {
        self.rail
    }

    pub(super) fn selected_path(&self) -> Option<&str> {
        self.selected
            .as_ref()
            .map(|selection| selection.path.as_str())
    }

    pub(super) fn mention(&self, path: &str) -> chat::Mention {
        match &self.local_root {
            Some(root) => chat::Mention::Local(root.join(path)),
            None => chat::Mention::Path(path.to_owned()),
        }
    }

    pub(super) fn selected_display_path(&self) -> Option<&str> {
        let selection = self.selected.as_ref()?;
        Some(match &selection.view {
            FileView::Code(doc) => &doc.path,
            FileView::File(doc) => &doc.display_path,
            FileView::Loading(_) | FileView::Failed(_) => &selection.path,
        })
    }

    pub(super) fn code_mut(&mut self) -> Option<&mut CodeDoc> {
        match &mut self.selected.as_mut()?.view {
            FileView::Code(doc) => Some(doc),
            _ => None,
        }
    }

    /// The selected file is on screen through a native QuickLook view.
    pub(in crate::ui) fn shows_native(&self) -> bool {
        self.native_file().is_some()
    }

    fn native_file(&self) -> Option<&PathBuf> {
        match &self.selected.as_ref()?.view {
            FileView::File(file) => Some(&file.path),
            _ => None,
        }
    }
}

impl Workspace {
    pub(in crate::ui) fn open_workspace_files(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self
            .client
            .state
            .workspace_session_id()
            .filter(|_| self.active_session_ready())
            .map(SessionId::share)
        else {
            return;
        };
        let id: Arc<str> = format!("{session}:files").into();
        if self.files_doc_mut(&id).is_some() {
            self.refresh_files(&id, cx);
            self.activate_tab(PageTab::Content(id));
            return;
        }
        self.request_preview(id, FILES_TITLE.into(), session, PreviewRequest::Files, cx);
    }

    fn files_doc_mut(&mut self, id: &str) -> Option<&mut FilesDoc> {
        match &mut self.content.tabs.get_mut(id)?.content {
            TabContent::Files(doc) => Some(doc),
            TabContent::Diff(doc) => Some(&mut doc.explorer),
            _ => None,
        }
    }

    fn files_doc(&self, id: &str) -> Option<&FilesDoc> {
        match &self.content.tabs.get(id)?.content {
            TabContent::Files(doc) => Some(doc),
            TabContent::Diff(doc) => Some(&doc.explorer),
            _ => None,
        }
    }

    pub(in crate::ui) fn refresh_open_workspace_files(&mut self, cx: &mut Context<Self>) {
        let ids = self
            .content
            .tabs
            .iter()
            .filter(|(_, tab)| matches!(tab.content, TabContent::Files(_)))
            .map(|(id, _)| Arc::clone(id))
            .collect::<Vec<_>>();
        for id in ids {
            self.refresh_files(&id, cx);
        }
    }

    /// Lists the workspace again without leaving the file on screen.
    pub(super) fn refresh_files(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(root) = self
            .files_doc_mut(id)
            .and_then(|doc| doc.local_root.as_ref().map(Arc::clone))
        {
            self.refresh_local_files(id, root, cx);
            return;
        }
        let Some(session) = self
            .files_doc_mut(id)
            .and_then(|doc| doc.session_id.as_ref().map(SessionId::share))
        else {
            return;
        };
        let request = gateway::request_id();
        let sent = self.send_preview(Arc::clone(&request), session, PreviewRequest::Files);
        match sent {
            Ok(()) => {
                if let Some(doc) = self.files_doc_mut(id) {
                    doc.refreshing = Some(request);
                }
            }
            Err(message) => self.report(message),
        }
        cx.notify();
    }

    fn send_preview(
        &self,
        request: Arc<str>,
        session: SessionId,
        preview: PreviewRequest,
    ) -> Result<(), String> {
        self.client
            .gateway
            .as_ref()
            .ok_or_else(|| "Gateway is unavailable".to_owned())
            .and_then(|gateway| {
                gateway
                    .preview(request, session, preview)
                    .map_err(|error| gateway::describe(&error))
            })
    }

    /// Selects a file in its viewer, or reveals that file's changes in the diff.
    fn select_workspace_file(&mut self, id: &str, path: &str, cx: &mut Context<Self>) {
        if let Some(TabContent::Diff(doc)) =
            self.content.tabs.get_mut(id).map(|tab| &mut tab.content)
        {
            doc.select_path(path);
            cx.notify();
            return;
        }
        if self
            .files_doc_mut(id)
            .is_some_and(|doc| doc.selected_path() == Some(path))
        {
            return;
        }
        self.load_workspace_selection(id, path.to_owned(), cx);
    }

    pub(super) fn load_workspace_selection(
        &mut self,
        id: &str,
        path: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(root) = self
            .files_doc_mut(id)
            .and_then(|doc| doc.local_root.as_ref().map(Arc::clone))
        {
            self.load_local_selection(id, path, &root, cx);
            return;
        }
        let Some(session) = self
            .files_doc_mut(id)
            .and_then(|doc| doc.session_id.as_ref().map(SessionId::share))
        else {
            return;
        };
        let request = gateway::request_id();
        let sent = self.send_preview(
            Arc::clone(&request),
            session,
            PreviewRequest::File(path.clone()), // rules-allow: the gateway request and the selection each own the path
        );
        let view = sent.map_or_else(
            |message| FileView::Failed(message.into()),
            |()| FileView::Loading(request),
        );
        self.show_selection(id, path, view);
        cx.notify();
    }

    fn show_selection(&mut self, id: &str, path: String, view: FileView) {
        let Some(tab) = self.content.tabs.get_mut(id) else {
            return;
        };
        let TabContent::Files(doc) = &mut tab.content else {
            return;
        };
        tab.title = file_title(&path).into();
        doc.selected = Some(Selection { path, view });
    }

    /// Opens a workspace selection in the shared file-and-tree viewer.
    fn open_workspace_selection(
        &mut self,
        id: Arc<str>,
        session: SessionId,
        path: &str,
        cx: &mut Context<Self>,
    ) {
        if self.files_doc_mut(&id).is_none() {
            let owner = TabOwner::Chat(session.share());
            let doc = FilesDoc::new(
                Some(session),
                vec![WorkspaceFileRecord {
                    path: path.to_owned(),
                    size: 0,
                }],
                false,
                cx,
            );
            self.open_content_for(
                owner,
                ContentTab {
                    id: Arc::clone(&id),
                    title: file_title(path).into(),
                    content: TabContent::Files(Box::new(doc)),
                },
                cx,
            );
            self.refresh_files(&id, cx);
        }
        if let Some(doc) = self.files_doc_mut(&id) {
            doc.reveal(path);
        }
        self.select_workspace_file(&id, path, cx);
        self.activate_tab(PageTab::Content(id));
        cx.notify();
    }

    pub(in crate::ui) fn open_git_diff(&mut self, scope: GitDiffScope, cx: &mut Context<Self>) {
        let Some(session) = self
            .client
            .state
            .workspace_session_id()
            .filter(|_| self.active_session_ready())
            .map(SessionId::share)
        else {
            return;
        };
        self.request_preview(
            format!("{session}:diff").into(),
            "Changes".into(),
            session,
            PreviewRequest::Diff(scope),
            cx,
        );
    }

    /// Opens a file on its gateway's machine, or directly when that gateway runs here.
    pub(in crate::ui) fn open_workspace_file(
        &mut self,
        session: SessionId,
        path: String,
        cx: &mut Context<Self>,
    ) {
        self.open_workspace_file_in_tab(session, path, false, cx);
    }

    /// Opens a workspace file in its own tab, with the same viewer and workspace tree.
    pub(in crate::ui) fn open_workspace_file_tab(
        &mut self,
        session: SessionId,
        path: String,
        cx: &mut Context<Self>,
    ) {
        self.open_workspace_file_in_tab(session, path, true, cx);
    }

    fn open_workspace_file_in_tab(
        &mut self,
        session: SessionId,
        path: String,
        separate: bool,
        cx: &mut Context<Self>,
    ) {
        let workspace = self
            .client
            .state
            .session
            .as_ref()
            .filter(|open| open.session.session_id == session)
            .and_then(|open| open.workspace.as_ref())
            .map(|workspace| workspace.path.as_path());
        let route = gateway::local::route_file(path, workspace, self.client.local_gateway);
        match route {
            FileRoute::Gateway(path) => {
                let id = if separate {
                    format!("{session}:file:{path}")
                } else {
                    format!("{session}:files")
                };
                self.open_workspace_selection(id.into(), session, &path, cx);
            }
            FileRoute::Local(path) => {
                let id = path.to_string_lossy().into_owned();
                let title = file_title(&id);
                self.open_file_preview_for(TabOwner::Chat(session), id, title, path, cx);
            }
            FileRoute::Unavailable(message) => {
                self.report(message);
                cx.notify();
            }
        }
    }

    pub(super) fn request_preview(
        &mut self,
        tab_id: Arc<str>,
        title: SharedString,
        session: SessionId,
        request: PreviewRequest,
        cx: &mut Context<Self>,
    ) {
        let id = gateway::request_id();
        let result = self.send_preview(Arc::clone(&id), session.share(), request);
        let owner = TabOwner::Chat(session.share());
        let content = result.map_or_else(TabContent::Failed, |()| TabContent::Loading {
            request: id,
            session_id: Some(session),
        });
        self.open_content_for(
            owner,
            ContentTab {
                id: tab_id,
                title,
                content,
            },
            cx,
        );
    }

    pub(in crate::ui) fn preview_loaded(
        &mut self,
        request: &str,
        session: &SessionId,
        result: Result<PreviewData, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(result) = self.chat_sources_loaded(request, session, result, cx) else {
            cx.notify();
            return;
        };
        let Some(tab_id) = self
            .content
            .tabs
            .iter()
            .find_map(|(id, tab)| match &tab.content {
                TabContent::Loading {
                    request: pending,
                    session_id,
                } if **pending == *request && session_id.as_ref() == Some(session) => {
                    Some(Arc::clone(id))
                }
                _ => None,
            })
        else {
            self.files_loaded(request, session, result, cx);
            return;
        };
        let content = match result {
            Ok(PreviewData::Files { files, truncated }) => TabContent::Files(Box::new(
                FilesDoc::new(Some(session.share()), files, truncated, cx),
            )),
            Ok(PreviewData::Diff { scope, text }) => {
                TabContent::Diff(DiffDoc::new(session.share(), scope, &text, cx))
            }
            Ok(PreviewData::File { path, bytes }) => self.preview_bytes(session, &path, &bytes),
            Ok(PreviewData::ChatFiles(_)) => return,
            Err(error) => TabContent::Failed(error),
        };
        if let Some(tab) = self.content.tabs.get_mut(&tab_id) {
            tab.content = content;
        }
        self.unify_file_tab(&tab_id, cx);
        cx.notify();
    }

    /// A Files tab's own requests: its selected file, or a new listing of its tree.
    fn files_loaded(
        &mut self,
        request: &str,
        session: &SessionId,
        result: Result<PreviewData, String>,
        cx: &mut Context<Self>,
    ) {
        let pending = |doc: &FilesDoc| {
            doc.session_id.as_ref() == Some(session)
                && (doc.refreshing.as_deref() == Some(request)
                    || matches!(&doc.selected, Some(Selection { view: FileView::Loading(id), .. }) if **id == *request))
        };
        let Some(id) = self
            .content
            .tabs
            .iter()
            .find_map(|(id, tab)| match &tab.content {
                TabContent::Files(doc) if pending(doc) => Some(Arc::clone(id)),
                _ => None,
            })
        else {
            return;
        };
        let view = match result {
            Ok(PreviewData::Files { files, truncated }) => {
                if let Some(doc) = self.files_doc_mut(&id) {
                    doc.refreshing = None;
                    doc.replace_tree(files, truncated);
                }
                None
            }
            Ok(PreviewData::File { path, bytes }) => Some(FileView::from_content(
                self.preview_bytes(session, &path, &bytes),
            )),
            Ok(PreviewData::Diff { .. } | PreviewData::ChatFiles(_)) => None,
            Err(error) => {
                let Some(doc) = self.files_doc_mut(&id) else {
                    return;
                };
                if doc.refreshing.as_deref() == Some(request) {
                    doc.refreshing = None;
                    self.report(error);
                    cx.notify();
                    return;
                }
                Some(FileView::Failed(error.into()))
            }
        };
        if let Some(view) = view
            && let Some(selection) = self
                .files_doc_mut(&id)
                .and_then(|doc| doc.selected.as_mut())
        {
            selection.view = view;
        }
        cx.notify();
    }

    fn preview_bytes(&self, session: &SessionId, name: &str, bytes: &[u8]) -> TabContent {
        let Some(directory) = &self.downloads else {
            return TabContent::Failed("Unable to create a private preview directory.".into());
        };
        stored_preview(session, name, bytes, directory.path())
    }

    /// Both local previews and gateway files use the same viewer and right-hand tree.
    pub(super) fn unify_file_tab(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(tab) = self.content.tabs.get_mut(id) else {
            return;
        };
        let local = match &tab.content {
            TabContent::Code(CodeDoc {
                external_file: Some(file),
                ..
            })
            | TabContent::File(file) => &file.path,
            _ => return,
        };
        let Some(root) = local.parent().map(Arc::<Path>::from) else {
            return;
        };
        let name = file_title(&local.to_string_lossy());
        let view = match std::mem::replace(&mut tab.content, TabContent::Launcher) {
            TabContent::Code(doc) => FileView::Code(doc),
            TabContent::File(file) => FileView::File(file),
            other => {
                tab.content = other;
                return;
            }
        };
        let mut doc = FilesDoc::new(
            None,
            vec![WorkspaceFileRecord {
                path: name.clone(), // rules-allow: the tree listing and the selection each own the name
                size: 0,
            }],
            false,
            cx,
        );
        doc.local_root = Some(root);
        doc.selected = Some(Selection { path: name, view });
        tab.content = TabContent::Files(Box::new(doc));
        self.refresh_files(id, cx);
    }

    fn refresh_local_files(&mut self, id: &str, root: Arc<Path>, cx: &Context<Self>) {
        let request = gateway::request_id();
        if let Some(doc) = self.files_doc_mut(id) {
            doc.refreshing = Some(Arc::clone(&request));
        }
        let id = id.to_owned();
        let list = cx
            .background_executor()
            .spawn(async move { gateway::local::list(&root) });
        cx.spawn(async move |this, cx| {
            let result = list.await;
            let updated = this.update(cx, |this, cx| {
                let Some(doc) = this
                    .files_doc_mut(&id)
                    .filter(|doc| doc.refreshing.as_deref() == Some(&request))
                else {
                    return;
                };
                doc.refreshing = None;
                match result {
                    Ok((files, truncated)) => doc.replace_tree(files, truncated),
                    Err(error) => this.report(gateway::describe_io(&error)),
                }
                cx.notify();
            });
            if let Err(error) = updated {
                tracing::debug!(%error, "the workspace closed before its local file list arrived");
            }
        })
        .detach();
    }

    fn load_local_selection(
        &mut self,
        id: &str,
        path: String,
        root: &Path,
        cx: &mut Context<Self>,
    ) {
        let request = gateway::request_id();
        let local = root.join(&path);
        self.show_selection(id, path, FileView::Loading(Arc::clone(&request)));
        let id = id.to_owned();
        let load = cx.background_executor().spawn(async move {
            read_local_preview(local).map(|preview| match preview {
                LocalPreview::Text(doc) => FileView::Code(*doc),
                LocalPreview::Native(path) => FileView::File(FileDoc::local(path)),
            })
        });
        cx.spawn(async move |this, cx| {
            let view = load
                .await
                .unwrap_or_else(|message| FileView::Failed(message.into()));
            let updated = this.update(cx, |this, cx| {
                // The selection still waits on this request, so only its view changes.
                if let Some(selection) = this
                    .files_doc_mut(&id)
                    .and_then(|doc| doc.selected.as_mut())
                    .filter(|selection| matches!(&selection.view, FileView::Loading(pending) if *pending == request))
                {
                    selection.view = view;
                    cx.notify();
                }
            });
            if let Err(error) = updated {
                tracing::debug!(%error, "the workspace closed before its local file loaded");
            }
        }).detach();
        cx.notify();
    }

    pub(in crate::ui) fn open_file_preview(
        &mut self,
        id: impl Into<Arc<str>>,
        title: impl Into<SharedString>,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.open_file_preview_for(TabOwner::of(&self.page_tabs), id, title, path, cx);
    }

    pub(in crate::ui) fn open_file_preview_for(
        &mut self,
        owner: TabOwner,
        id: impl Into<Arc<str>>,
        title: impl Into<SharedString>,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let (id, title): (Arc<str>, SharedString) = (id.into(), title.into());
        let id = self.content.navigation.register_file(&owner, id);
        let request = gateway::request_id();
        // A downloaded chat file reads as its own name, not its private temporary path.
        let label = self
            .downloads
            .as_ref()
            .is_some_and(|dir| path.starts_with(dir.path()))
            .then(|| SharedString::clone(&title));
        self.open_content_for(
            owner,
            ContentTab {
                id: Arc::clone(&id),
                title,
                content: TabContent::Loading {
                    request: Arc::clone(&request),
                    session_id: None,
                },
            },
            cx,
        );
        let load = cx
            .background_executor()
            .spawn(async move { read_local_preview(path) });
        cx.spawn(async move |this,cx| {
            let content = match load.await {
                Ok(LocalPreview::Text(mut doc)) => {
                    if let Some(label) = label {
                        doc.path = label.to_string();
                    }
                    TabContent::Code(*doc)
                }
                Ok(LocalPreview::Native(file)) => {
                    let display_path = label.map_or_else(|| file.to_string_lossy().into_owned(), |label| label.to_string());
                    TabContent::File(FileDoc::new(file, display_path))
                },
                Err(error) => TabContent::Failed(error),
            };
            let updated = this.update(cx,|this,cx| {
                if let Some(tab)=this.content.tabs.get_mut(&id)
                    && matches!(&tab.content, TabContent::Loading { request: pending, .. } if pending == &request) {
                    tab.content = content;
                    this.unify_file_tab(&id, cx);
                    cx.notify();
                }
            });
            if let Err(error) = updated {
                tracing::debug!(%error, "the workspace closed before its file preview loaded");
            }
        }).detach();
    }

    /// The file on the left, the tree in a narrow rail at the pane's outer edge.
    pub(super) fn files_document(
        &mut self,
        id: &str,
        doc: &FilesDoc,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let bar = Self::files_toolbar(id, doc, cx);
        let body = self.files_document_body(id, doc, window, cx);
        document(bar, body, cx)
    }

    fn files_document_body(
        &mut self,
        id: &str,
        doc: &FilesDoc,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let viewer = div()
            .flex()
            .flex_col()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(self.files_viewer(id, doc, window, cx))
            .into_any_element();
        self.explorer_body(id, doc, viewer, cx)
    }

    /// The shared animated, resizable explorer beside a file or its changes.
    pub(super) fn explorer_body(
        &self,
        id: &str,
        doc: &FilesDoc,
        viewer: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(shown) = doc.rail_motion.sliding(doc.rail_width(cx), Instant::now()) {
            let rail = self.files_rail(id, doc, cx);
            return div()
                .flex()
                .size_full()
                .child(div().flex_1().min_w_0().h_full().child(viewer))
                .child(Self::files_rail_frame(shown, doc.rail_width(cx), rail))
                .into_any_element();
        }
        if !doc.rail {
            return viewer;
        }
        let rail = self.files_rail(id, doc, cx);
        gpui_component::h_resizable(SharedString::from(format!("files-panes:{id}")))
            .with_state(&doc.resize)
            .child(
                gpui_component::resizable_panel()
                    .size_range(VIEWER_MIN..Pixels::MAX)
                    .child(viewer),
            )
            .child(
                gpui_component::resizable_panel()
                    .size(RAIL)
                    .size_range(RAIL_MIN..RAIL_MAX)
                    .fixed()
                    .child(Self::files_rail_frame(
                        doc.rail_width(cx),
                        doc.rail_width(cx),
                        rail,
                    )),
            )
            .into_any_element()
    }

    fn files_rail_frame(shown: Pixels, full: Pixels, rail: AnyElement) -> Div {
        let frame = div()
            .relative()
            .w(shown)
            .h_full()
            .flex_shrink_0()
            .overflow_hidden()
            .child(div().w(full).h_full().child(rail));
        #[cfg(feature = "snapshot")]
        let frame = frame.child(crate::ui::snapshot::track("motion:files-rail"));
        frame
    }

    pub(super) fn track_files_motion(&mut self, window: &Window, cx: &App) {
        let Some(PageTab::Content(id)) = self.page_tabs.get(self.active_page) else {
            return;
        };
        let doc = self
            .content
            .tabs
            .get_mut(id)
            .and_then(|tab| match &mut tab.content {
                TabContent::Files(doc) => Some(doc.as_mut()),
                TabContent::Diff(doc) => Some(doc.explorer.as_mut()),
                _ => None,
            });
        if let Some(doc) = doc
            && doc.track_motion(Instant::now(), cx.reduce_motion())
        {
            window.request_animation_frame();
        }
    }

    /// One toolbar spans the file and its tree, independent of file origin.
    fn files_toolbar(id: &str, doc: &FilesDoc, cx: &Context<Self>) -> Div {
        let bar = match doc.selected.as_ref() {
            Some(Selection {
                view: FileView::Code(code),
                ..
            }) => Self::code_toolbar(id, code, cx),
            Some(Selection {
                view: FileView::File(file),
                ..
            }) => toolbar()
                .child(path_label(
                    &file.display_path,
                    FileKind::for_name(&file.display_path).glyph,
                    cx,
                ))
                .child(spacer())
                .child(Self::file_actions(file, &file.display_path, cx))
                .child(Self::selection_refresh(id, cx)),
            Some(selection) => toolbar()
                .child(path_label(
                    &selection.path,
                    FileKind::for_name(&selection.path).glyph,
                    cx,
                ))
                .child(spacer())
                .child(Self::selection_refresh(id, cx)),
            None => toolbar().child(spacer()),
        };
        bar.when(
            !doc.source.borrow().expanded.is_empty() && doc.rail,
            |bar| {
                let id = id.to_owned();
                bar.child(
                    toolbar_button(
                        "collapse-folders",
                        "Collapse all folders",
                        chevron(true, &palette(cx)),
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(doc) = this.files_doc_mut(&id) {
                            doc.collapse_all();
                        }
                        cx.notify();
                    })),
                )
            },
        )
        .child(Self::rail_toggle(id, doc.rail, cx))
    }

    fn files_viewer(
        &mut self,
        id: &str,
        doc: &FilesDoc,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(selection) = &doc.selected else {
            return EmptyState::new("hi.fileText", "Select a file")
                .detail("Choose a file in the list to view it here.")
                .into_any_element();
        };
        let glyph = FileKind::for_name(&selection.path).glyph;
        let name = file_title(&selection.path);
        match &selection.view {
            FileView::Code(code) => self.code_view(id, code, window, cx),
            FileView::Loading(_) => EmptyState::new(glyph, format!("Opening {name}"))
                .loading()
                .into_any_element(),
            FileView::Failed(message) => {
                EmptyState::new(Tone::Error.glyph(), format!("Couldn’t open {name}"))
                    .detail(SharedString::clone(message))
                    .into_any_element()
            }
            FileView::File(file) => Self::native_content(
                id,
                NativeView::File(file.path.to_string_lossy().into_owned()),
                cx,
            )
            .into_any_element(),
        }
    }

    fn selection_refresh(id: &str, cx: &Context<Self>) -> Button {
        let id = id.to_owned();
        toolbar_button("refresh", "Refresh", "hi.refresh03", cx).on_click(cx.listener(
            move |this, _, _, cx| {
                let path = this
                    .files_doc_mut(&id)
                    .and_then(|doc| doc.selected_path().map(str::to_owned));
                if let Some(path) = path {
                    this.load_workspace_selection(&id, path, cx);
                }
            },
        ))
    }

    pub(super) fn rail_toggle(id: &str, shown: bool, cx: &Context<Self>) -> Button {
        let id = id.to_owned();
        let label = if shown {
            "Hide file explorer"
        } else {
            "Show file explorer"
        };
        let toggle = toolbar_button("files-rail", label, "hi.folders", cx)
            .toggle(shown, cx)
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(doc) = this.files_doc_mut(&id) {
                    doc.rail = !doc.rail;
                    doc.rail_motion
                        .set(doc.rail, cx.reduce_motion(), Instant::now());
                }
                cx.notify();
            }));
        #[cfg(feature = "snapshot")]
        let toggle = toggle.child(crate::ui::snapshot::probe("motion:files-toggle".into()));
        toggle
    }

    /// The workspace tree: folders expand in place, a file shows beside it.
    fn files_rail(&self, id: &str, doc: &FilesDoc, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .when(doc.truncated, |el| {
                el.child(super::diff::truncation_warning(
                    "Some files are not shown.",
                    &p,
                ))
            })
            .map(|el| {
                if doc.source.borrow().files.is_empty() {
                    el.child(EmptyState::new("hi.folder", "No files"))
                } else {
                    el.child(Self::files_tree(id, doc, cx))
                }
            });
        body.size_full().pt(space::S).into_any_element()
    }

    fn files_tree(id: &str, doc: &FilesDoc, cx: &mut Context<Self>) -> AnyElement {
        doc.sync_tree(cx);
        let (view, tab) = (cx.entity().downgrade(), id.to_owned());
        tree(&doc.tree, move |index, entry, _, _, cx| {
            view.update(cx, |this, cx| this.workspace_tree_row(&tab, entry, cx))
                .unwrap_or_else(|_| ListItem::new(index))
        })
        .flex_1()
        .min_h_0()
        .pt(space::S)
        .into_any_element()
    }

    /// A folder toggles in place; a file shows in the tab's viewer, highlighted here.
    fn workspace_tree_row(&self, tab: &str, entry: &TreeEntry, cx: &Context<Self>) -> ListItem {
        let item = entry.item();
        let folder = entry.is_folder();
        let glyph = match (folder, entry.is_expanded()) {
            (true, true) => "hi.folderOpen",
            (true, false) => "hi.folder",
            _ => FileKind::for_name(&item.label).glyph,
        };
        let caret = div()
            .flex()
            .flex_shrink_0()
            .justify_center()
            .w(glyph::INLINE)
            .when(folder, |cell| {
                cell.child(chevron(entry.is_expanded(), &palette(cx)))
            });
        let content = div()
            .id(SharedString::from(format!("workspace:{}", item.id)))
            .flex()
            .items_center()
            .gap(space::S)
            .h(height::REGULAR)
            .child(caret)
            .child(icon(glyph, &palette(cx)).size(glyph::INLINE))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .typography(Type::Body)
                    .child(SharedString::clone(&item.label)),
            );
        #[cfg(feature = "snapshot")]
        let content = content
            .child(crate::ui::snapshot::probe(format!(
                "motion:folder:{}",
                item.id
            )))
            .when(item.id == "src/main.rs", |view| {
                view.child(crate::ui::snapshot::track("motion:folder-child-src"))
            });
        let path = SharedString::clone(&item.id);
        let row = ListItem::new("row")
            .h(height::REGULAR)
            .py_0()
            .pl(space::S + (glyph::INLINE + space::S) * entry.depth().to_f32())
            .pr(space::S)
            .control_corners()
            .cursor_pointer();
        if folder {
            // The tree toggles the folder; the toolbar's collapse control follows it.
            return row
                .child(content)
                .on_click(cx.listener(|_, _, _, cx| cx.notify()));
        }
        let mention = self.files_doc(tab).map_or_else(
            || chat::Mention::Path(path.to_string()),
            |doc| {
                doc.selected_path()
                    .filter(|selected| *selected == path.as_ref())
                    .and_then(|_| self.tab_mention(tab, cx))
                    .unwrap_or_else(|| doc.mention(&path))
            },
        );
        let tab = tab.to_owned();
        row.child(
            content
                // The row's menu replaces the tree's own, which has no items here.
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .context_menu(Self::file_menu(mention, cx)),
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.select_workspace_file(&tab, &path, cx);
        }))
    }
}

#[cfg(feature = "snapshot")]
impl Workspace {
    /// Shows `paths` as the workspace files tab with `expanded` folders open.
    pub(in crate::ui) fn open_files_list(
        &mut self,
        paths: Vec<String>,
        expanded: &[&str],
        cx: &mut Context<Self>,
    ) {
        let files = paths
            .into_iter()
            .map(|path| WorkspaceFileRecord { path, size: 0 })
            .collect();
        let doc = FilesDoc::new(None, files, false, cx);
        doc.source
            .borrow_mut()
            .expanded
            .extend(expanded.iter().map(std::string::ToString::to_string));
        self.open_content(
            ContentTab {
                id: "snapshot:files".into(),
                title: FILES_TITLE.into(),
                content: TabContent::Files(Box::new(doc)),
            },
            cx,
        );
    }

    /// Shows the local `path`, relative to this repository, as the Files tab's selection.
    pub(in crate::ui) fn select_files_now(
        &mut self,
        path: &str,
        source: bool,
        cx: &mut Context<Self>,
    ) {
        let local = std::env::current_dir().unwrap_or_default().join(path);
        let view = match read_local_preview(local) {
            Ok(LocalPreview::Text(mut doc)) => {
                path.clone_into(&mut doc.path);
                doc.source = source;
                FileView::Code(*doc)
            }
            Ok(LocalPreview::Native(local)) => FileView::File(FileDoc::local(local)),
            Err(error) => FileView::Failed(error.into()),
        };
        if let Some(doc) = self.files_doc_mut("snapshot:files") {
            doc.reveal(path);
        }
        self.show_selection("snapshot:files", path.to_owned(), view);
        cx.notify();
    }
}

/// Retains the gateway path while preparing an independent local copy for native apps.
fn stored_preview(session: &SessionId, name: &str, bytes: &[u8], directory: &Path) -> TabContent {
    let path = crate::ui::runtime::private_path(directory, name);
    match gateway::local::save(path, bytes) {
        Ok(path) => match text_preview(name, bytes, Origin::Workspace(session.share())) {
            Some(mut doc) => {
                doc.external_file = Some(FileDoc::new(path, name.to_owned()));
                TabContent::Code(doc)
            }
            None => TabContent::File(FileDoc::new(path, name.to_owned())),
        },
        Err(error) => TabContent::Failed(gateway::describe_io(&error)),
    }
}

#[cfg(test)]
mod external_file_tests {
    use super::{TabContent, stored_preview};

    #[test]
    fn gateway_binary_keeps_complete_bytes_for_native_apps() {
        let directory = tempfile::tempdir().expect("preview directory");
        let bytes = b"%PDF-1.7\0binary";
        let session = "chat".parse().unwrap();
        let TabContent::File(path) =
            stored_preview(&session, "docs/report.pdf", bytes, directory.path())
        else {
            panic!("native preview")
        };
        assert_eq!(path.display_path, "docs/report.pdf");
        assert_eq!(std::fs::read(path.path).expect("saved content"), bytes);
    }
}
